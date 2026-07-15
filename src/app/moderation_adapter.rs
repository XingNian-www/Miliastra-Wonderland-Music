use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::chat_observation::ChatObservationExclusiveGuard;
use super::command::{self, ModerationAction};
use super::decision_lock::DecisionScreenLock;
use super::frame_source::{Canvas, load_frame};
use super::workflow_actions::{self, TemplateMode};
use super::{
    AutomationApp, FrameArgs, PendingTask, PendingTaskExecution, ResolvedTemplateArgs,
    TemplateArgs, TemporaryPrimaryHold, TrackedPendingTask, UiResidency,
};
use crate::features::moderation::{
    ModerationCommandPort, ModerationExecutionPort, ModerationPrimaryHold,
    ModerationResultExecution, ModerationResultPreparation, ModerationResultTask, ModerationStart,
    ModerationTaskPort, ModerationVotePort, ModerationVoteWork, is_moderation_vote_message,
};

#[derive(Clone, Copy, Debug)]
enum ModerationUiState {
    OpenFriendPanel,
    OpenSearchPanel,
    EnterUid,
    WaitSearchResult,
    ClickAction,
    ConfirmAction,
    WaitActionApplied,
    Done,
}

impl ModerationUiState {
    fn label(self) -> &'static str {
        match self {
            Self::OpenFriendPanel => "打开好友界面",
            Self::OpenSearchPanel => "打开 UID 搜索",
            Self::EnterUid => "输入 UID",
            Self::WaitSearchResult => "等待搜索结果",
            Self::ClickAction => "点击执行动作",
            Self::ConfirmAction => "确认动作",
            Self::WaitActionApplied => "等待动作完成",
            Self::Done => "完成",
        }
    }
}

struct AppModerationVotePort {
    worker: AutomationApp,
    observation_session: Option<ChatObservationExclusiveGuard>,
    screen_lock: DecisionScreenLock,
    template_args: ResolvedTemplateArgs,
    canvas: Canvas,
}

impl AppModerationVotePort {
    fn new(worker: AutomationApp) -> Result<Self> {
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
        workflow_actions::wait(duration.as_millis().min(u128::from(u64::MAX)) as u64);
    }

    fn is_running(&self) -> bool {
        self.worker.running.load(AtomicOrdering::SeqCst)
    }

    fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>> {
        let frame = load_frame(
            &FrameArgs { image: None },
            &self.canvas,
            &self.worker.game_ui,
        )
        .context("管理投票截图失败")?;
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

impl ModerationCommandPort for AutomationApp {
    fn send_hall(&mut self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn prepare_vote_hold(&mut self) -> Result<Box<dyn ModerationPrimaryHold>> {
        self.ensure_ui_residency(UiResidency::Primary, "管理投票等待前准备")?;
        let hold = TemporaryPrimaryHold::new(self.chat_listener.clone())?;
        self.update_monitor_chat_listener();
        Ok(Box::new(hold))
    }
}

impl ModerationTaskPort for AutomationApp {
    fn is_running(&self) -> bool {
        self.running.load(AtomicOrdering::SeqCst)
    }

    fn submit_result(&self, task: ModerationResultTask) -> Result<()> {
        self.push_pending_task(PendingTask::ModerationResult(task))
    }

    fn sync_listener_state(&self) {
        self.update_monitor_chat_listener();
    }
}

impl ModerationExecutionPort for AutomationApp {
    fn prepare_result(&mut self, label: &str) -> Result<ModerationResultPreparation> {
        match self.prepare_command_ui(label) {
            Ok(true) => Ok(ModerationResultPreparation::Ready),
            Ok(false) => {
                log::info!("投票结果处理前未能回到一级界面，保留任务: {label}");
                Ok(ModerationResultPreparation::Retry)
            }
            Err(error) if super::is_target_window_unavailable_error(&error) => Err(error),
            Err(error) => {
                log::error!("投票结果处理前准备界面失败，保留任务 {label}: {error:#}");
                Ok(ModerationResultPreparation::Retry)
            }
        }
    }

    fn send_hall(&mut self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn execute_action(&mut self, command: &command::ModerationCommand) -> Result<bool> {
        self.execute_moderation_steps(command)
    }

    fn sync_listener_state(&mut self) {
        self.update_monitor_chat_listener();
    }

    fn wait_after_action(&mut self) {
        workflow_actions::wait(self.config.timing.command.return_retry_ms);
    }
}

impl AutomationApp {
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
        task_id: u64,
        task: ModerationResultTask,
    ) -> Result<PendingTaskExecution> {
        let moderation = self.moderation.clone();
        match moderation.execute_result(task, self)? {
            ModerationResultExecution::Completed => Ok(PendingTaskExecution::Completed),
            ModerationResultExecution::Retry(task) => {
                let tracked = TrackedPendingTask {
                    id: task_id,
                    task: PendingTask::ModerationResult(task),
                };
                if let Err(error) = self.push_pending_task_front(tracked) {
                    self.update_monitor_chat_listener();
                    return Err(error);
                }
                Ok(PendingTaskExecution::Requeued)
            }
        }
    }

    fn collect_moderation_vote_screen_lock(&self) -> DecisionScreenLock {
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui) else {
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

    fn execute_moderation_steps(&self, command: &command::ModerationCommand) -> Result<bool> {
        log::info!("开始执行{} UID{}", command.action.label(), command.uid);
        let result = self.execute_moderation_steps_inner(command);
        let returned = self.return_to_primary_from_transient_ui(command.action.label());
        if matches!(result, Ok(true)) && !returned {
            log::error!(
                "{} UID{} 已执行，但返回一级界面失败，继续尝试发送成功通告",
                command.action.label(),
                command.uid
            );
        }
        result
    }

    fn execute_moderation_steps_inner(&self, command: &command::ModerationCommand) -> Result<bool> {
        self.ensure_ui_residency(UiResidency::Primary, "管理操作打开好友界面前准备")?;
        let locator = self.ui_locator(self.template_poll_ms());
        let mut state = ModerationUiState::OpenFriendPanel;

        loop {
            log::debug!(
                "{} UID{} UI 状态: {}",
                command.action.label(),
                command.uid,
                state.label()
            );

            state = match state {
                ModerationUiState::OpenFriendPanel => {
                    workflow_actions::press_key_text("o", &self.game_ui)?;
                    if !self.wait_template_atom(
                        &locator,
                        &self.config.templates.friend_panel,
                        self.config.moderation.friend_panel_region,
                        self.config.timing.command.ui_timeout_ms,
                        "好友界面",
                    )? {
                        log::error!("未找到好友界面模板");
                        return Ok(false);
                    }
                    ModerationUiState::OpenSearchPanel
                }
                ModerationUiState::OpenSearchPanel => {
                    workflow_actions::press_key_text("e", &self.game_ui)?;
                    workflow_actions::wait(self.config.timing.invite.step_ms);
                    workflow_actions::press_key_text("e", &self.game_ui)?;
                    if !self.wait_template_atom(
                        &locator,
                        &self.config.templates.friend_search_panel,
                        self.config.moderation.search_panel_region,
                        self.config.timing.command.ui_timeout_ms,
                        "好友搜索界面",
                    )? {
                        log::error!("未找到搜索按钮模板");
                        return Ok(false);
                    }
                    ModerationUiState::EnterUid
                }
                ModerationUiState::EnterUid => {
                    log::info!(
                        "UID 搜索点击: input=({}, {}) button=({}, {})",
                        self.config.moderation.search_input_point.x,
                        self.config.moderation.search_input_point.y,
                        self.config.moderation.search_button_point.x,
                        self.config.moderation.search_button_point.y,
                    );
                    workflow_actions::click_point(
                        self.config.moderation.search_input_point,
                        &self.game_ui,
                    )?;
                    workflow_actions::wait(self.config.timing.input.click_ms);
                    workflow_actions::paste(
                        &command.uid,
                        &self.game_ui,
                        self.config.timing.input.text_ms,
                    )?;
                    workflow_actions::click_point(
                        self.config.moderation.search_button_point,
                        &self.game_ui,
                    )?;
                    ModerationUiState::WaitSearchResult
                }
                ModerationUiState::WaitSearchResult => {
                    if !self.click_template_atom(
                        &locator,
                        &self.config.templates.friend_more_settings,
                        self.config.moderation.more_settings_region,
                        self.config.timing.moderation.search_result_timeout_ms,
                        "更多设置",
                    )? {
                        log::error!("等待更多设置模板超时");
                        return Ok(false);
                    }
                    ModerationUiState::ClickAction
                }
                ModerationUiState::ClickAction => {
                    let (region, template, label) = match command.action {
                        ModerationAction::Blacklist => (
                            self.config.moderation.blacklist_region,
                            &self.config.templates.friend_blacklist,
                            "拉黑按钮",
                        ),
                        ModerationAction::BlockChat => (
                            self.config.moderation.block_chat_region,
                            &self.config.templates.friend_block_chat,
                            "屏蔽聊天按钮",
                        ),
                    };
                    if !self.click_template_atom(
                        &locator,
                        template,
                        region,
                        self.config.timing.command.ui_timeout_ms,
                        label,
                    )? {
                        log::error!("未找到{}模板", label);
                        return Ok(false);
                    }
                    ModerationUiState::ConfirmAction
                }
                ModerationUiState::ConfirmAction => {
                    if !self.click_template_atom(
                        &locator,
                        &self.config.templates.friend_confirm,
                        self.config.moderation.confirm_region,
                        self.config.timing.command.ui_timeout_ms,
                        "确认按钮",
                    )? {
                        log::error!("未找到确认按钮模板");
                        return Ok(false);
                    }
                    ModerationUiState::WaitActionApplied
                }
                ModerationUiState::WaitActionApplied => {
                    let applied =
                        workflow_actions::locate_template(
                            &locator,
                            &self.config.templates.friend_confirm,
                            self.config.moderation.confirm_region,
                            self.config.templates.marker_threshold,
                            self.config.timing.moderation.confirm_wait_ms,
                            TemplateMode::Absent {
                                stability: Some(self.workflow_stability(
                                    self.config.timing.moderation.confirm_wait_ms,
                                )),
                            },
                            || self.running.load(AtomicOrdering::SeqCst),
                        );
                    if let Err(error) = applied {
                        log::error!("等待确认按钮模板消失超时: {error:#}");
                        return Ok(false);
                    }
                    ModerationUiState::Done
                }
                ModerationUiState::Done => {
                    log::info!("{} UID{} 完成", command.action.label(), command.uid);
                    return Ok(true);
                }
            };
        }
    }
}
