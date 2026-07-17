use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::formal_task::FormalTaskClient;
use super::{
    ApplicationRuntime, PendingTask, PendingTaskExecution, ResidencyPurpose, ResolvedTemplateArgs,
    TemporaryPrimaryHold, UiResidency,
};
use crate::features::moderation::{
    ModerationCommandPort, ModerationExecutionPort, ModerationPrimaryHold,
    ModerationResultExecution, ModerationResultTask, ModerationStart, ModerationTaskPort,
    ModerationVotePort, ModerationVoteWork, is_moderation_vote_message,
};
use crate::interfaces::chat as command;
use crate::observation::chat::{ChatObservationExclusiveGuard, ChatObservationShared};
use crate::observation::decision::DecisionScreenLock;
use crate::runtime::monitor::MonitorShared;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::scheduler::FormalTaskEnqueueOutcome;
use crate::runtime::ui::InputCertainty;
use crate::ui::atoms::GameUi;
use crate::ui::frame::{Canvas, load_frame};
use crate::ui::geometry::Rect;
use crate::ui::routines::{
    ExecuteModeration, ModerationEffect, ModerationUiAction, UiResidencyOutcome,
};

struct ModerationVoteContext {
    running: Arc<AtomicBool>,
    game_ui: GameUi,
    ocr: OcrRuntimeHandle,
    monitor: MonitorShared,
    chat_observations: ChatObservationShared,
    template_args: ResolvedTemplateArgs,
    chat_rect: Rect,
    canvas: Canvas,
}

impl ModerationVoteContext {
    fn open(self) -> Result<AppModerationVotePort> {
        let observation_session = self.chat_observations.begin_exclusive()?;
        let mut port = AppModerationVotePort {
            running: self.running,
            game_ui: self.game_ui,
            ocr: self.ocr,
            monitor: self.monitor,
            observation_session: Some(observation_session),
            screen_lock: DecisionScreenLock::default(),
            template_args: self.template_args,
            chat_rect: self.chat_rect,
            canvas: self.canvas,
        };
        port.screen_lock = port.collect_screen_lock();
        Ok(port)
    }
}

struct AppModerationVotePort {
    running: Arc<AtomicBool>,
    game_ui: GameUi,
    ocr: OcrRuntimeHandle,
    monitor: MonitorShared,
    observation_session: Option<ChatObservationExclusiveGuard>,
    screen_lock: DecisionScreenLock,
    template_args: ResolvedTemplateArgs,
    chat_rect: Rect,
    canvas: Canvas,
}

impl AppModerationVotePort {
    fn collect_screen_lock(&self) -> DecisionScreenLock {
        let Ok(frame) = load_frame(&self.canvas, &self.game_ui) else {
            return DecisionScreenLock::default();
        };
        let Ok(messages) = super::listener::scan_chat_with_shared_ocr(
            &self.ocr,
            &self.monitor,
            self.chat_rect,
            &frame.image,
            &self.template_args,
        ) else {
            return DecisionScreenLock::default();
        };
        DecisionScreenLock::from_messages(
            &messages,
            &|message_type| message_type == "pink",
            &is_moderation_vote_message,
        )
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
        self.running.load(AtomicOrdering::SeqCst)
    }

    fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>> {
        let frame = load_frame(&self.canvas, &self.game_ui).context("管理投票截图失败")?;
        let messages = super::listener::scan_chat_with_shared_ocr(
            &self.ocr,
            &self.monitor,
            self.chat_rect,
            &frame.image,
            &self.template_args,
        )
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

struct ModerationTaskContext {
    running: Arc<AtomicBool>,
    formal_tasks: Option<FormalTaskClient>,
}

impl ModerationTaskPort for ModerationTaskContext {
    fn is_running(&self) -> bool {
        self.running.load(AtomicOrdering::SeqCst)
    }

    fn submit_result(&self, task: ModerationResultTask) -> Result<()> {
        let tasks = self
            .formal_tasks
            .clone()
            .ok_or_else(|| anyhow::anyhow!("正式任务执行运行时尚未启动"))?;
        match tasks.enqueue(PendingTask::ModerationResult(task))? {
            FormalTaskEnqueueOutcome::Queued(_) => Ok(()),
            FormalTaskEnqueueOutcome::Duplicate => {
                log::info!("管理投票结果已在待执行范围内，跳过重复入队");
                Ok(())
            }
        }
    }

    fn sync_listener_state(&self) {}
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
        if let Err(error) =
            self.establish_ui_residency(UiResidency::Primary, ResidencyPurpose::ListenerModeSwitch)
        {
            hold.release();
            return Err(error).context("管理投票无法建立临时一级监听驻留");
        }
        Ok(Box::new(hold))
    }
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
        moderation_action_result(command, outcome.effect(), outcome.residency())
    }

    fn sync_listener_state(&mut self) {}

    fn wait_after_action(&mut self) {
        // The typed UI routine already waits for action confirmation and residency recovery.
    }
}

fn moderation_action_result(
    command: &command::ModerationCommand,
    effect: &ModerationEffect,
    residency: &UiResidencyOutcome,
) -> Result<bool> {
    match effect {
        ModerationEffect::Applied => {
            if let UiResidencyOutcome::Failed(failure) = residency {
                log::error!(
                    "{} UID{} 已确认执行，但一级驻留恢复失败，不会重放动作：{failure}",
                    command.action.label(),
                    command.uid
                );
            }
            Ok(true)
        }
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

impl ApplicationRuntime {
    pub(super) fn execute_moderation_with_vote(
        &mut self,
        command: &command::ModerationCommand,
    ) -> Result<bool> {
        let moderation = self.moderation.clone();
        match moderation.start(command, self)? {
            ModerationStart::Duplicate => Ok(false),
            ModerationStart::Started(work) => {
                self.spawn_moderation_vote(work)?;
                Ok(true)
            }
        }
    }

    fn spawn_moderation_vote(&self, work: ModerationVoteWork) -> Result<()> {
        let mut workers = self
            .moderation_workers
            .lock()
            .map_err(|_| anyhow::anyhow!("管理投票线程句柄锁已损坏"))?;
        let vote_context = ModerationVoteContext {
            running: Arc::clone(&self.running),
            game_ui: self.game_ui.clone(),
            ocr: self.ocr.clone(),
            monitor: self.monitor.clone(),
            chat_observations: self.chat_observations.clone(),
            template_args: self.chat_templates.clone(),
            chat_rect: self.config.screen.chat_rect.into(),
            canvas: Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            },
        };
        let task_context = ModerationTaskContext {
            running: Arc::clone(&self.running),
            formal_tasks: self.formal_tasks.clone(),
        };
        let moderation = self.moderation.clone();
        workers.push(thread::spawn(move || {
            let action = work.command().action;
            let uid = work.command().uid.clone();
            log::info!("{} UID{} 后台投票线程已启动", action.label(), uid);
            let result = match vote_context.open() {
                Ok(mut port) => moderation.run_vote(work, &mut port, &task_context),
                Err(error) => {
                    log::error!("{}后台投票失败: {error:#}", action.label());
                    moderation.fail_vote(work, &task_context)
                }
            };
            if let Err(error) = result {
                log::error!("后台投票结果加入队列失败: {error:#}");
            }
        }));
        Ok(())
    }

    pub(super) fn join_moderation_workers(&self) {
        let workers = match self.moderation_workers.lock() {
            Ok(mut workers) => workers.drain(..).collect::<Vec<_>>(),
            Err(_) => {
                log::error!("管理投票线程句柄锁已损坏，无法等待线程关闭");
                return;
            }
        };
        for worker in workers {
            if let Err(error) = worker.join() {
                log::error!("管理投票线程 panic: {error:?}");
            }
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::chat::{ModerationAction, ModerationCommand};
    use crate::runtime::ui::UiRoutineFailure;

    #[test]
    fn applied_action_is_not_erased_by_residency_failure() {
        let command = ModerationCommand {
            action: ModerationAction::Blacklist,
            uid: "123456789".to_string(),
            requester: "管理员".to_string(),
        };
        let residency = UiResidencyOutcome::Failed(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "recover_moderation",
            "primary UI was not reached",
        ));

        assert!(
            moderation_action_result(&command, &ModerationEffect::Applied, &residency).unwrap()
        );
    }
}
