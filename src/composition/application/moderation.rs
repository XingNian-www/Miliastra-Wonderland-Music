use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{
    ApplicationRuntime, PendingTask, PendingTaskExecution, ResidencyPurpose, ResolvedTemplateArgs,
    TemplateArgs, TemporaryPrimaryHold, UiResidency,
};
use crate::features::moderation::{
    ModerationCommandPort, ModerationExecutionPort, ModerationPrimaryHold,
    ModerationResultExecution, ModerationResultTask, ModerationStart, ModerationTaskPort,
    ModerationVotePort, ModerationVoteWork, is_moderation_vote_message,
};
use crate::interfaces::chat as command;
use crate::observation::chat::ChatObservationExclusiveGuard;
use crate::observation::decision::DecisionScreenLock;
use crate::runtime::ui::InputCertainty;
use crate::ui::frame::{Canvas, load_frame};
use crate::ui::routines::{
    ExecuteModeration, ModerationEffect, ModerationUiAction, UiResidencyOutcome,
};

struct AppModerationVotePort {
    worker: ApplicationRuntime,
    observation_session: Option<ChatObservationExclusiveGuard>,
    screen_lock: DecisionScreenLock,
    template_args: ResolvedTemplateArgs,
    canvas: Canvas,
}

impl AppModerationVotePort {
    fn new(worker: ApplicationRuntime) -> Result<Self> {
        let observation_session = worker.chat_observations.begin_exclusive()?;
        let screen_lock = worker.collect_moderation_vote_screen_lock();
        let template_args = TemplateArgs::default().resolve(&worker.config);
        let canvas = Canvas {
            width: worker.config.screen.expected_width,
            height: worker.config.screen.expected_height,
            resize: true,
        };
        Ok(Self {
            worker,
            observation_session: Some(observation_session),
            screen_lock,
            template_args,
            canvas,
        })
    }
}

impl ModerationVotePort for AppModerationVotePort {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    fn is_running(&self) -> bool {
        self.worker.running.load(AtomicOrdering::SeqCst)
    }

    fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>> {
        let frame = load_frame(&self.canvas, &self.worker.game_ui).context("管理投票截图失败")?;
        let messages = self
            .worker
            .scan_chat_with_shared_ocr(&frame.image, &self.template_args)
            .context("管理投票 OCR 失败")?;
        Ok(messages
            .into_iter()
            .filter(|message| {
                message.message_type == "pink" && !self.screen_lock.is_existing(message)
            })
            .map(|message| message.text)
            .collect())
    }

    fn finish(&mut self) {
        self.observation_session.take();
    }
}

impl ModerationPrimaryHold for TemporaryPrimaryHold {
    fn release(&mut self) {
        TemporaryPrimaryHold::release(self);
    }
}

impl ModerationCommandPort for ApplicationRuntime {
    fn send_hall(&mut self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn prepare_vote_hold(&mut self) -> Result<Box<dyn ModerationPrimaryHold>> {
        let mut hold = TemporaryPrimaryHold::new(self.business.clone())?;
        if let Err(error) = self.establish_ui_residency(
            UiResidency::Primary,
            ResidencyPurpose::DecisionObservation("管理投票观察切换"),
        ) {
            hold.release();
            return Err(error);
        }
        Ok(Box::new(hold))
    }
}

impl ModerationTaskPort for ApplicationRuntime {
    fn is_running(&self) -> bool {
        self.running.load(AtomicOrdering::SeqCst)
    }

    fn submit_result(&self, task: ModerationResultTask) -> Result<()> {
        self.push_pending_task(PendingTask::ModerationResult(task))
    }

    fn sync_listener_state(&self) {}
}

impl ModerationExecutionPort for ApplicationRuntime {
    fn send_hall(&mut self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn execute_action(&mut self, command: &command::ModerationCommand) -> Result<bool> {
        let action = match command.action {
            command::ModerationAction::Blacklist => ModerationUiAction::Blacklist,
            command::ModerationAction::BlockChat => ModerationUiAction::BlockChat,
        };
        let outcome = self
            .moderation_ui
            .submit(ExecuteModeration::new(action, &command.uid))
            .context("提交管理 UI 事务")?
            .wait()
            .context("等待管理 UI 事务")?;
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            log::error!(
                "{} UID{} 已完成目标阶段，但一级驻留恢复失败: {failure}",
                command.action.label(),
                command.uid
            );
        }
        match outcome.effect() {
            ModerationEffect::Applied => Ok(true),
            ModerationEffect::Failed(failure)
                if matches!(
                    failure.certainty(),
                    InputCertainty::BeforeInput | InputCertainty::ConfirmedFailure
                ) =>
            {
                log::error!(
                    "{} UID{} 确认未执行: {failure}",
                    command.action.label(),
                    command.uid
                );
                Ok(false)
            }
            ModerationEffect::Failed(failure) => Err(anyhow::anyhow!(
                "{} UID{} 执行结果未知，禁止重放：{failure}",
                command.action.label(),
                command.uid
            )),
        }
    }

    fn sync_listener_state(&mut self) {}

    fn wait_after_action(&mut self) {
        // The typed UI routine already waits for action confirmation and residency recovery.
    }
}

impl ApplicationRuntime {
    pub(super) fn execute_moderation_with_vote(
        &mut self,
        command: &command::ModerationCommand,
    ) -> Result<bool> {
        let moderation = self.moderation.clone();
        match moderation.start(command, self)? {
            ModerationStart::Duplicate => Ok(false),
            ModerationStart::Started(work) => {
                self.spawn_moderation_vote(work);
                Ok(true)
            }
        }
    }

    fn spawn_moderation_vote(&self, work: ModerationVoteWork) {
        let worker = self.clone_for_background_task();
        let moderation = self.moderation.clone();
        thread::spawn(move || {
            let action = work.command().action;
            let uid = work.command().uid.clone();
            log::info!("{} UID{} 后台投票线程已启动", action.label(), uid);
            let result = match AppModerationVotePort::new(worker.clone_for_background_task()) {
                Ok(mut port) => moderation.run_vote(work, &mut port, &worker),
                Err(error) => {
                    log::error!("{}后台投票失败: {error:#}", action.label());
                    moderation.fail_vote(work, &worker)
                }
            };
            if let Err(error) = result {
                log::error!("后台投票结果加入队列失败: {error:#}");
            }
        });
    }

    pub(super) fn execute_moderation_vote_result(
        &mut self,
        task: ModerationResultTask,
    ) -> Result<PendingTaskExecution> {
        let moderation = self.moderation.clone();
        match moderation.execute_result(task, self)? {
            ModerationResultExecution::Completed => Ok(PendingTaskExecution::Completed),
        }
    }

    fn collect_moderation_vote_screen_lock(&self) -> DecisionScreenLock {
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&canvas, &self.game_ui) else {
            return DecisionScreenLock::default();
        };
        let Ok(messages) = self.scan_chat_with_shared_ocr(&frame.image, &template_args) else {
            return DecisionScreenLock::default();
        };
        DecisionScreenLock::from_messages(
            &messages,
            &|message_type| message_type == "pink",
            &is_moderation_vote_message,
        )
    }
}
