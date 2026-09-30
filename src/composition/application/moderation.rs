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
use crate::features::command::{ModuleCommand, RoutedCommand};
use crate::features::identity::{IdentityAccess, IdentityRole};
use crate::features::moderation::{
    ModerationCommandPort, ModerationExecutionPort, ModerationPrimaryHold,
    ModerationResultExecution, ModerationResultTask, ModerationService, ModerationStart,
    ModerationTaskPort, ModerationVotePort, ModerationVoteWork, is_moderation_vote_message,
};
use crate::interfaces::chat as command;
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
    template_args: ResolvedTemplateArgs,
    chat_rect: Rect,
    canvas: Canvas,
}

impl ModerationVoteContext {
    fn open(self) -> Result<AppModerationVotePort> {
        let mut port = AppModerationVotePort {
            running: self.running,
            game_ui: self.game_ui,
            ocr: self.ocr,
            monitor: self.monitor,
            // 投票独立扫描粉色好友消息，只用屏幕基线过滤存量票；不占用聊天观察流，
            // 一级、二级监听仍可持续发布和执行普通大厅命令。
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
            None,
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
            None,
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

    fn finish(&mut self) {}
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
        let mut hold = TemporaryPrimaryHold::new(self.business.business.clone())?;
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
            .ui
            .moderation_ui
            .submit(ExecuteModeration::new(action, &command.uid))
            .context("提交管理 UI 事务")?
            .wait()
            .context("等待管理 UI 事务")?;
        moderation_action_result(command, outcome.effect(), outcome.residency())
    }

    fn sync_listener_state(&mut self) {}

    fn wait_after_action(&mut self) {
        // 类型化 UI 流程已经等待操作确认和界面驻留恢复。
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

// requester 是展示字段（监听器会替换为备注），不能作为授权主体；
// 也不信任排队时的 role 快照，执行时重新查询精确的发送者身份。
fn moderation_requester_role(
    identity: &IdentityAccess,
    parsed: &RoutedCommand,
) -> Option<IdentityRole> {
    if parsed.permission_required.is_some() {
        return None;
    }
    identity.role_of(&parsed.username)
}

fn prepare_direct_blacklist(
    service: &ModerationService,
    identity: &IdentityAccess,
    parsed: &RoutedCommand,
) -> Result<Option<ModerationStart>> {
    let ModuleCommand::Moderation(command) = &parsed.command else {
        return Ok(None);
    };
    let mut reserved =
        service.reserve_direct_blacklist(command, moderation_requester_role(identity, parsed))?;
    if let Some(ModerationStart::Ready(task)) = &mut reserved {
        task.bind_direct_sender(&parsed.username);
    }
    Ok(reserved)
}

fn reauthorize_moderation_result(
    identity: &IdentityAccess,
    task: &mut ModerationResultTask,
) -> bool {
    if !task.is_direct() {
        return true;
    }
    if task
        .direct_sender()
        .and_then(|sender| identity.role_of(sender))
        .is_some_and(|role| matches!(role, IdentityRole::Admin | IdentityRole::Owner))
    {
        return true;
    }
    // 排队期间撤权或缺少可信发送者时，绝不执行预批准的管理员操作。
    task.cancel();
    false
}

impl ApplicationRuntime {
    /// 聊天观察线程只接管租约并入队，不发送消息、不执行任何游戏 UI。
    pub(super) fn enqueue_direct_blacklist(&self, parsed: &RoutedCommand) -> Result<bool> {
        let Some(start) = prepare_direct_blacklist(
            &self.business.moderation,
            &self.lifecycle.live_configs.identity,
            parsed,
        )?
        else {
            return Ok(false);
        };
        match start {
            ModerationStart::Ready(task) => {
                self.log_executed_command(parsed, &task.label())?;
                // 入队失败时 task 的 Drop 仅释放本次 token，旧结果仍保持失效。
                self.push_pending_task(PendingTask::ModerationResult(task))?;
            }
            ModerationStart::Duplicate => {
                log::info!("管理员拉黑已在执行范围内，跳过重复请求");
            }
            ModerationStart::Started(_) => unreachable!("direct reservation cannot start a vote"),
        }
        Ok(true)
    }

    pub(super) fn execute_moderation_with_vote(
        &mut self,
        parsed: &RoutedCommand,
        command: &command::ModerationCommand,
    ) -> Result<bool> {
        let moderation = self.business.moderation.clone();
        let requester_role =
            moderation_requester_role(&self.lifecycle.live_configs.identity, parsed);
        match moderation.start(command, requester_role, self)? {
            ModerationStart::Duplicate => Ok(false),
            ModerationStart::Ready(task) => {
                moderation.execute_result(task, self)?;
                Ok(true)
            }
            ModerationStart::Started(work) => {
                self.spawn_moderation_vote(work)?;
                Ok(true)
            }
        }
    }

    fn spawn_moderation_vote(&self, work: ModerationVoteWork) -> Result<()> {
        let mut workers = self
            .business
            .moderation_workers
            .lock()
            .map_err(|_| anyhow::anyhow!("管理投票线程句柄锁已损坏"))?;
        let vote_context = ModerationVoteContext {
            running: Arc::clone(&self.lifecycle.running),
            game_ui: self.ui.game_ui.clone(),
            ocr: self.ui.ocr.clone(),
            monitor: self.lifecycle.monitor.clone(),
            template_args: self.ui.chat_templates.clone(),
            chat_rect: self.lifecycle.config.screen.chat_rect.into(),
            canvas: Canvas {
                width: self.lifecycle.config.screen.expected_width,
                height: self.lifecycle.config.screen.expected_height,
                resize: true,
            },
        };
        let task_context = ModerationTaskContext {
            running: Arc::clone(&self.lifecycle.running),
            formal_tasks: self.business.formal_tasks.clone(),
        };
        let moderation = self.business.moderation.clone();
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
        let workers = match self.business.moderation_workers.lock() {
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
        mut task: ModerationResultTask,
    ) -> Result<PendingTaskExecution> {
        if !reauthorize_moderation_result(&self.lifecycle.live_configs.identity, &mut task) {
            log::warn!("管理员拉黑任务因发起者权限已失效而取消");
            return Ok(PendingTaskExecution::Completed);
        }
        let moderation = self.business.moderation.clone();
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

    #[derive(Default)]
    struct TestModerationLedger(
        std::sync::Mutex<crate::features::moderation::ModerationWorkflowRegistry>,
    );

    impl crate::features::moderation::ModerationWorkflowLedger for TestModerationLedger {
        fn acquire(
            &self,
            key: crate::features::moderation::ModerationWorkflowKey,
            direct: bool,
        ) -> Result<Option<Arc<crate::features::moderation::ModerationWorkflowToken>>> {
            Ok(self.0.lock().unwrap().acquire(key, direct))
        }
        fn release(
            &self,
            key: crate::features::moderation::ModerationWorkflowKey,
            token: Arc<crate::features::moderation::ModerationWorkflowToken>,
        ) -> Result<bool> {
            Ok(self.0.lock().unwrap().release(&key, &token))
        }
        fn contains(
            &self,
            key: crate::features::moderation::ModerationWorkflowKey,
        ) -> Result<bool> {
            Ok(self.0.lock().unwrap().contains(&key))
        }
    }

    fn test_moderation_service() -> ModerationService {
        ModerationService::new(
            crate::features::moderation::ModerationPolicy::new(
                Duration::from_secs(4),
                Duration::from_secs(1),
                1,
                1,
            ),
            Arc::new(TestModerationLedger::default()),
        )
    }

    struct TestHold;
    impl ModerationPrimaryHold for TestHold {
        fn release(&mut self) {}
    }

    #[derive(Default)]
    struct TestModerationPort {
        messages: Vec<String>,
        actions: usize,
    }
    impl ModerationCommandPort for TestModerationPort {
        fn send_hall(&mut self, message: &str) -> Result<()> {
            self.messages.push(message.to_string());
            Ok(())
        }
        fn prepare_vote_hold(&mut self) -> Result<Box<dyn ModerationPrimaryHold>> {
            Ok(Box::new(TestHold))
        }
    }
    impl ModerationExecutionPort for TestModerationPort {
        fn send_hall(&mut self, message: &str) -> Result<()> {
            self.messages.push(message.to_string());
            Ok(())
        }
        fn execute_action(&mut self, _: &ModerationCommand) -> Result<bool> {
            self.actions += 1;
            Ok(true)
        }
        fn sync_listener_state(&mut self) {}
        fn wait_after_action(&mut self) {}
    }

    fn moderation_identity() -> IdentityAccess {
        use crate::features::identity::{IdentityConfig, IdentityMapping};
        IdentityAccess::new(IdentityConfig {
            mappings: [
                ("主人", IdentityRole::Owner),
                ("管理员", IdentityRole::Admin),
                ("普通好友", IdentityRole::Friend),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (nickname, role))| IdentityMapping {
                nickname: nickname.to_string(),
                id: uuid::Uuid::from_u128(index as u128 + 1),
                role,
                note: format!("{nickname}展示备注"),
            })
            .collect(),
        })
    }

    fn moderation_routed(sender: &str) -> RoutedCommand {
        use crate::features::command::{CommandAuthority, ModuleCommand};
        let mut parsed = RoutedCommand::console(
            "拉黑",
            "拉黑123456789",
            ModuleCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".to_string(),
                requester: "主人".to_string(),
            }),
        );
        parsed.username = sender.to_string();
        parsed.authority = CommandAuthority::Friend;
        parsed.message_type = "pink".to_string();
        // 即便内层 requester 和缓存角色伪造为主人，也只能信任真实发送者的映射。
        parsed.role = Some(IdentityRole::Owner);
        parsed
    }

    #[test]
    fn moderation_authorization_uses_exact_live_sender_not_requester_or_source() {
        use crate::features::command::CommandAuthority;
        let identity = moderation_identity();
        for (sender, expected) in [
            ("主人", Some(IdentityRole::Owner)),
            ("管理员", Some(IdentityRole::Admin)),
            ("普通好友", Some(IdentityRole::Friend)),
            ("未映射好友", None),
            ("管理员展示备注", None),
            ("管理员 ", None),
        ] {
            for source in [CommandAuthority::Friend, CommandAuthority::HallMember] {
                let mut parsed = moderation_routed(sender);
                parsed.authority = source;
                assert_eq!(moderation_requester_role(&identity, &parsed), expected);
            }
        }
    }

    #[test]
    fn moderation_authorization_rechecks_revoked_roles_and_denied_routes() {
        use crate::features::identity::IdentityConfig;
        let identity = moderation_identity();
        let mut parsed = moderation_routed("管理员");
        assert_eq!(
            moderation_requester_role(&identity, &parsed),
            Some(IdentityRole::Admin)
        );
        parsed.permission_required = Some(IdentityRole::Admin);
        assert_eq!(moderation_requester_role(&identity, &parsed), None);
        parsed.permission_required = None;
        identity.replace(IdentityConfig::default());
        assert_eq!(moderation_requester_role(&identity, &parsed), None);
    }

    #[test]
    fn moderation_admission_reserves_only_live_admin_blacklists() {
        let identity = moderation_identity();
        for sender in ["管理员", "主人", "普通好友", "未映射好友", "管理员展示备注"]
        {
            let service = test_moderation_service();
            let parsed = moderation_routed(sender);
            let reserved = prepare_direct_blacklist(&service, &identity, &parsed).unwrap();
            if matches!(sender, "管理员" | "主人") {
                let Some(ModerationStart::Ready(task)) = reserved else {
                    panic!("direct task expected");
                };
                assert_eq!(task.direct_sender(), Some(sender));
            } else {
                assert!(
                    reserved.is_none(),
                    "forged requester/role must not grant admission"
                );
            }
            let mut blocked = moderation_routed(sender);
            let ModuleCommand::Moderation(command) = &mut blocked.command else {
                unreachable!()
            };
            command.action = ModerationAction::BlockChat;
            assert!(
                prepare_direct_blacklist(&service, &identity, &blocked)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn moderation_queued_takeover_avoids_old_result_dedup_and_invalidates_decision() {
        use crate::runtime::scheduler::{
            FormalScheduler, FormalTaskCancellationToken, FormalTaskDedupKey,
            FormalTaskExecutionOutcome, FormalTaskSubmission, FormalTaskWork,
        };
        struct NoopWork;
        impl FormalTaskWork for NoopWork {
            fn execute(
                self: Box<Self>,
                _: FormalTaskCancellationToken,
            ) -> FormalTaskExecutionOutcome {
                FormalTaskExecutionOutcome::Completed(Ok(String::new()))
            }
            fn cancel(self: Box<Self>) {}
        }
        let submission = |task: &ModerationResultTask| {
            FormalTaskSubmission::new(
                task.label(),
                Some(FormalTaskDedupKey::new(task.dedup_key())),
                false,
                Box::new(NoopWork),
            )
        };
        for approved in [false, true] {
            let identity = moderation_identity();
            let service = test_moderation_service();
            let parsed = moderation_routed("管理员");
            let ModuleCommand::Moderation(command) = &parsed.command else {
                unreachable!()
            };
            let mut port = TestModerationPort::default();
            let ModerationStart::Started(work) = service
                .start(command, Some(IdentityRole::Friend), &mut port)
                .unwrap()
            else {
                panic!("vote expected");
            };
            let old = work.finish(approved);
            let mut scheduler = FormalScheduler::new();
            assert!(matches!(
                scheduler.enqueue(submission(&old)).unwrap(),
                FormalTaskEnqueueOutcome::Queued(_)
            ));
            let Some(ModerationStart::Ready(mut direct)) =
                prepare_direct_blacklist(&service, &identity, &parsed).unwrap()
            else {
                panic!("takeover expected");
            };
            assert_ne!(old.dedup_key(), direct.dedup_key());
            assert_ne!(direct.dedup_key(), command.lock_key());
            assert!(matches!(
                scheduler.enqueue(submission(&direct)).unwrap(),
                FormalTaskEnqueueOutcome::Queued(_)
            ));
            port.messages.clear();
            service.execute_result(old, &mut port).unwrap();
            assert!(port.messages.is_empty());
            assert_eq!(port.actions, 0);
            assert!(service.is_active(command).unwrap());
            assert!(reauthorize_moderation_result(&identity, &mut direct));
            service.execute_result(direct, &mut port).unwrap();
            assert_eq!(port.actions, 1);
        }
    }

    #[test]
    fn moderation_queued_direct_task_rechecks_revocation_not_forged_requester() {
        use crate::features::identity::{IdentityConfig, IdentityMapping};
        for downgrade in [false, true] {
            let identity = moderation_identity();
            let service = test_moderation_service();
            let parsed = moderation_routed("管理员");
            let ModuleCommand::Moderation(command) = &parsed.command else {
                unreachable!()
            };
            let Some(ModerationStart::Ready(mut task)) =
                prepare_direct_blacklist(&service, &identity, &parsed).unwrap()
            else {
                panic!("direct task expected");
            };
            let mut mappings = vec![IdentityMapping {
                nickname: "主人".to_string(),
                id: uuid::Uuid::from_u128(1),
                role: IdentityRole::Owner,
                note: String::new(),
            }];
            if downgrade {
                mappings.push(IdentityMapping {
                    nickname: "管理员".to_string(),
                    id: uuid::Uuid::from_u128(2),
                    role: IdentityRole::Friend,
                    note: String::new(),
                });
            }
            identity.replace(IdentityConfig { mappings });
            assert!(!reauthorize_moderation_result(&identity, &mut task));
            assert!(!service.is_active(command).unwrap());
            let mut port = TestModerationPort::default();
            service.execute_result(task, &mut port).unwrap();
            assert_eq!(port.actions, 0);
            assert!(port.messages.is_empty());
        }
    }

    #[test]
    fn moderation_queued_direct_drop_or_cancel_releases_only_its_generation() {
        let identity = moderation_identity();
        let service = test_moderation_service();
        let parsed = moderation_routed("管理员");
        let ModuleCommand::Moderation(command) = &parsed.command else {
            unreachable!()
        };
        let Some(ModerationStart::Ready(mut canceled)) =
            prepare_direct_blacklist(&service, &identity, &parsed).unwrap()
        else {
            panic!("direct task expected");
        };
        canceled.cancel();
        assert!(!service.is_active(command).unwrap());
        let Some(ModerationStart::Ready(replacement)) =
            prepare_direct_blacklist(&service, &identity, &parsed).unwrap()
        else {
            panic!("replacement expected");
        };
        drop(canceled);
        assert!(service.is_active(command).unwrap());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        drop(sender.send(replacement).unwrap_err().0);
        assert!(!service.is_active(command).unwrap());
        let Some(ModerationStart::Ready(mut unbound)) = service
            .reserve_direct_blacklist(command, Some(IdentityRole::Admin))
            .unwrap()
        else {
            panic!("direct task expected");
        };
        assert!(
            !reauthorize_moderation_result(&identity, &mut unbound),
            "unbound queued privilege must fail closed"
        );
        assert!(!service.is_active(command).unwrap());
    }

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
