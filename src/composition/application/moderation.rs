use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::formal_task::FormalTaskClient;
use super::{
    ApplicationRuntime, PendingTask, PendingTaskExecution, ResidencyPurpose, TemporaryPrimaryHold,
    UiResidency,
};
use crate::features::command::{ModuleCommand, RoutedCommand};
use crate::features::identity::{IdentityAccess, IdentityRole};
use crate::features::moderation::{
    ModerationCommandPort, ModerationExecutionPort, ModerationPrimaryHold,
    ModerationResultExecution, ModerationResultTask, ModerationService, ModerationStart,
    ModerationTaskPort, ModerationVotePort, ModerationVoteWork, is_moderation_vote_message,
};
use crate::interfaces::chat as command;
use crate::observation::chat::{
    ChatIdentity, ChatTimeline, ChatTimelineSnapshot, ChatTimelineSubscriber, NoChatRefreshError,
};
use crate::runtime::scheduler::FormalTaskEnqueueOutcome;
use crate::runtime::ui::InputCertainty;
use crate::ui::routines::{
    ExecuteModeration, ModerationEffect, ModerationUiAction, UiResidencyOutcome,
};

struct ModerationVoteContext {
    running: Arc<AtomicBool>,
    chat_timeline: ChatTimeline,
    baseline_timeout: Duration,
}

impl ModerationVoteContext {
    fn open(self) -> Result<AppModerationVotePort> {
        // Subscribe before requesting a fresh frame so nothing after the baseline is lost.
        // This queue is independent of ordinary command delivery and other votes.
        let subscriber = self
            .chat_timeline
            .subscribe_chat(ChatIdentity::PrimaryHall)?;
        let baseline = self.fresh_snapshot()?;
        AppModerationVotePort::from_baseline(self, subscriber, baseline)
    }

    fn fresh_snapshot(&self) -> Result<ChatTimelineSnapshot> {
        let deadline = Instant::now() + self.baseline_timeout;
        self.fresh_snapshot_until(deadline)?
            .ok_or_else(|| anyhow::anyhow!("等待共享聊天管理投票基线超时"))
    }

    fn fresh_snapshot_until(&self, deadline: Instant) -> Result<Option<ChatTimelineSnapshot>> {
        if !self.running.load(AtomicOrdering::SeqCst) {
            anyhow::bail!("运行已停止，已结束管理投票");
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let mut refresh = self.chat_timeline.request_refresh()?;
        loop {
            if !self.running.load(AtomicOrdering::SeqCst) {
                anyhow::bail!("运行已停止，已结束管理投票");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match self
                .chat_timeline
                .wait_for_snapshot(refresh, remaining.min(Duration::from_millis(50)))
            {
                Ok(Some(snapshot)) => {
                    if let Some(next) = self
                        .chat_timeline
                        .refresh_after_other_hall_panel(&ChatIdentity::PrimaryHall, &snapshot)?
                    {
                        refresh = next;
                        continue;
                    }
                    if snapshot.chat != ChatIdentity::PrimaryHall {
                        refresh = self.chat_timeline.request_refresh()?;
                        continue;
                    }
                    return Ok(Some(snapshot));
                }
                Ok(None) => {}
                Err(error) if error.downcast_ref::<NoChatRefreshError>().is_some() => {
                    // No chat is a transient view gap, not an OCR failure. Rearm
                    // the capture fence without extending the caller's deadline.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() && self.running.load(AtomicOrdering::SeqCst) {
                        refresh = self.chat_timeline.request_refresh()?;
                        thread::sleep(remaining.min(Duration::from_millis(50)));
                    }
                }
                Err(error) => return Err(error).context("管理投票共享 OCR 基线失败"),
            }
        }
    }
}

struct AppModerationVotePort {
    context: ModerationVoteContext,
    subscriber: ChatTimelineSubscriber,
    baseline: ChatTimelineSnapshot,
    baseline_sequence: u64,
    failed: bool,
}

impl AppModerationVotePort {
    fn from_baseline(
        context: ModerationVoteContext,
        subscriber: ChatTimelineSubscriber,
        baseline: ChatTimelineSnapshot,
    ) -> Result<Self> {
        if baseline.chat != ChatIdentity::PrimaryHall {
            anyhow::bail!("管理投票共享 OCR 基线不在一级大厅");
        }
        subscriber.bind_chat(baseline.chat.clone(), baseline.visual_session)?;
        let baseline_sequence = baseline
            .messages
            .iter()
            .map(|message| message.id.bubble_sequence.get())
            .max()
            .unwrap_or(0);
        Ok(Self {
            context,
            subscriber,
            baseline,
            baseline_sequence,
            failed: false,
        })
    }

    fn read_votes(&mut self) -> Result<Vec<String>> {
        let mut votes = Vec::new();
        for snapshot in self
            .subscriber
            .read()
            .context("管理投票共享 OCR 读取失败")?
        {
            if crate::observation::chat::other_hall_panel(&self.baseline.chat, &snapshot.chat) {
                continue;
            }
            if snapshot.chat != self.baseline.chat {
                continue;
            }
            if snapshot.captured_at <= self.baseline.captured_at {
                continue;
            }
            if snapshot.visual_session != self.baseline.visual_session {
                if snapshot.messages.is_empty() {
                    // Wait for a reliable restored baseline before accepting votes.
                    continue;
                }
                // A rebuilt view of the same chat gets new bubble IDs. Rebase
                // before counting so restored historical votes are not replayed.
                log::info!(
                    "管理投票共享 OCR 视图重建，更新基线: session={:?} -> {:?}",
                    self.baseline.visual_session,
                    snapshot.visual_session
                );
                self.baseline_sequence = snapshot
                    .messages
                    .iter()
                    .map(|message| message.id.bubble_sequence.get())
                    .max()
                    .unwrap_or(0);
                self.baseline = snapshot;
                continue;
            }
            // Keep repeated observations and OCR revisions: the vote service requires
            // stable samples. Business acknowledgement must not consume these votes.
            votes.extend(
                snapshot
                    .messages
                    .into_iter()
                    .filter(|message| {
                        message.id.bubble_sequence.get() > self.baseline_sequence
                            && message.message_type == "pink"
                            && is_moderation_vote_message(&message.text)
                    })
                    .map(|message| message.text),
            );
        }
        Ok(votes)
    }
}

impl ModerationVotePort for AppModerationVotePort {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while self.is_running() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    fn is_running(&self) -> bool {
        !self.failed && self.context.running.load(AtomicOrdering::SeqCst)
    }

    fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>> {
        let deadline = Instant::now() + self.context.baseline_timeout;
        self.poll_visible_friend_messages_until(deadline)
    }

    fn poll_visible_friend_messages_until(&mut self, deadline: Instant) -> Result<Vec<String>> {
        let result = (|| {
            // An unchanged view may be fingerprint-cached by the observer. Request
            // genuine fresh OCR samples for the service's stable-vote threshold.
            let votes = self.read_votes()?;
            if !votes.is_empty() || Instant::now() >= deadline {
                return Ok(votes);
            }
            if self.context.fresh_snapshot_until(deadline)?.is_none() {
                return Ok(Vec::new());
            }
            self.read_votes()
        })();
        // Real capture failures end the vote; an expected deadline expiry does not.
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn finish(&mut self) {}
}

struct ModerationTaskContext {
    running: Arc<AtomicBool>,
    formal_tasks: Option<FormalTaskClient>,
    source_command: RoutedCommand,
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
        match tasks.enqueue_with_command_source(
            PendingTask::ModerationResult(task),
            &self.source_command,
        )? {
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
                self.push_pending_task_with_command_source(
                    PendingTask::ModerationResult(task),
                    parsed,
                )?;
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
                self.spawn_moderation_vote(work, parsed)?;
                Ok(true)
            }
        }
    }

    fn spawn_moderation_vote(
        &self,
        work: ModerationVoteWork,
        source: &RoutedCommand,
    ) -> Result<()> {
        let mut workers = self
            .business
            .moderation_workers
            .lock()
            .map_err(|_| anyhow::anyhow!("管理投票线程句柄锁已损坏"))?;
        let vote_context = ModerationVoteContext {
            running: Arc::clone(&self.lifecycle.running),
            chat_timeline: self.ui.chat_timeline.clone(),
            baseline_timeout: Duration::from_millis(
                self.lifecycle.config.timing.decision.timeout_ms,
            ),
        };
        let task_context = ModerationTaskContext {
            running: Arc::clone(&self.lifecycle.running),
            formal_tasks: self.business.formal_tasks.clone(),
            source_command: source.clone(),
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

    fn shared_vote_input(
        text: &str,
        color: &str,
        y: i32,
    ) -> crate::observation::chat::RecognizedChatMessage {
        crate::observation::chat::RecognizedChatMessage {
            text: text.into(),
            sender: None,
            raw_sender: None,
            message_type: color.into(),
            block: crate::ui::geometry::Rect::new(0, y, 100, 10),
        }
    }

    fn shared_vote_context(shared: &ChatTimeline) -> ModerationVoteContext {
        ModerationVoteContext {
            running: Arc::new(AtomicBool::new(true)),
            chat_timeline: shared.clone(),
            baseline_timeout: Duration::ZERO,
        }
    }

    fn shared_vote_port(shared: &ChatTimeline) -> AppModerationVotePort {
        let subscriber = shared.subscribe_chat(ChatIdentity::PrimaryHall).unwrap();
        let baseline = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![shared_vote_input("[旧]：@同", "pink", 10)],
            )
            .unwrap();
        AppModerationVotePort::from_baseline(shared_vote_context(shared), subscriber, baseline)
            .unwrap()
    }

    #[test]
    fn moderation_vote_open_rearms_after_secondary_panel_baseline() {
        let shared = ChatTimeline::new();
        let publisher = shared.clone();
        let publish_frames = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while publisher.pending_refresh().unwrap().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "initial vote refresh was not requested"
                );
                thread::sleep(Duration::from_millis(2));
            }
            publisher
                .publish(
                    Instant::now(),
                    ChatIdentity::SecondaryHall,
                    vec![shared_vote_input("panel transition", "blue", 10)],
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while publisher.pending_refresh().unwrap().is_none() {
                assert!(Instant::now() < deadline, "vote refresh was not rearmed");
                thread::sleep(Duration::from_millis(2));
            }
            publisher
                .publish_no_chat(Instant::now(), "transient view gap")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while publisher.pending_refresh().unwrap().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "vote refresh was not rearmed after no-chat"
                );
                thread::sleep(Duration::from_millis(2));
            }
            publisher
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![shared_vote_input("baseline", "blue", 10)],
                )
                .unwrap();
        });
        let context = ModerationVoteContext {
            running: Arc::new(AtomicBool::new(true)),
            chat_timeline: shared.clone(),
            baseline_timeout: Duration::from_secs(2),
        };
        let mut port = context.open().unwrap();
        publish_frames.join().unwrap();
        assert_eq!(port.baseline.chat, ChatIdentity::PrimaryHall);

        shared
            .publish(
                Instant::now(),
                ChatIdentity::SecondaryHall,
                vec![shared_vote_input("panel @同意", "pink", 20)],
            )
            .unwrap();
        assert!(port.read_votes().unwrap().is_empty());
        for _ in 0..2 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        shared_vote_input("baseline", "blue", 10),
                        shared_vote_input("[甲]：@同意", "pink", 30),
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            port.read_votes().unwrap(),
            vec!["[甲]：@同意", "[甲]：@同意"]
        );
    }

    #[test]
    fn shared_votes_retain_pink_samples_and_revisions_without_consuming_business() {
        let shared = ChatTimeline::new();
        let business = shared.subscribe().unwrap();
        let mut port = shared_vote_port(&shared);
        let first = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    shared_vote_input("[旧]：@同", "pink", 10),
                    shared_vote_input("[甲]：@同", "pink", 30),
                ],
            )
            .unwrap();
        // Correct the new bubble before the vote consumer runs; the old bubble's
        // correction is still baseline, and blue votes never count.
        for _ in 0..2 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        shared_vote_input("[旧]：@同意", "pink", 10),
                        shared_vote_input("[甲]：@同意", "pink", 30),
                        shared_vote_input("[乙]：@不同意", "blue", 50),
                    ],
                )
                .unwrap();
        }
        // The vote remains observable after it has scrolled off screen.
        shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![])
            .unwrap();
        assert_eq!(
            port.poll_visible_friend_messages().unwrap(),
            vec!["[甲]：@同意", "[甲]：@同意"]
        );
        assert!(port.read_votes().unwrap().is_empty());
        assert!(shared.is_business_pending(&first.messages[1].id).unwrap());
        assert_eq!(business.read().unwrap().len(), 5);
    }

    #[test]
    fn shared_vote_rebinds_same_chat_without_replaying_restored_votes() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        for _ in 0..2 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        shared_vote_input("[旧]：@同", "pink", 10),
                        shared_vote_input("[甲]：@同意", "pink", 30),
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            port.poll_visible_friend_messages().unwrap(),
            vec!["[甲]：@同意", "[甲]：@同意"]
        );
        let previous_session = port.baseline.visual_session;
        shared
            .publish_no_chat(Instant::now(), "temporary panel transition")
            .unwrap();
        shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![])
            .unwrap();
        assert!(port.read_votes().unwrap().is_empty());
        let restored = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    shared_vote_input("[旧]：@同", "pink", 10),
                    shared_vote_input("[甲]：@同意", "pink", 30),
                ],
            )
            .unwrap();
        // A correction of a restored historical vote remains part of the baseline.
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    shared_vote_input("[旧]：@同", "pink", 10),
                    shared_vote_input("[甲]：@不同意", "pink", 30),
                ],
            )
            .unwrap();
        assert!(port.read_votes().unwrap().is_empty());
        assert_ne!(previous_session, port.baseline.visual_session);
        assert_eq!(port.baseline.visual_session, restored.visual_session);
        assert!(port.is_running());
        // New votes already queued after the restored baseline retain their samples.
        for _ in 0..2 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        shared_vote_input("[旧]：@同", "pink", 10),
                        shared_vote_input("[甲]：@不同意", "pink", 30),
                        shared_vote_input("[乙]：@不同意", "pink", 50),
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            port.poll_visible_friend_messages().unwrap(),
            vec!["[乙]：@不同意", "[乙]：@不同意"]
        );
        assert!(port.is_running());
    }

    #[test]
    fn shared_vote_waits_through_long_foreign_state_until_primary_returns() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        shared
            .publish_no_chat(Instant::now(), "temporary panel transition")
            .unwrap();
        for _ in 0..32 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PublicChannel,
                    vec![shared_vote_input("[甲]：@同意", "pink", 10)],
                )
                .unwrap();
        }
        assert!(port.read_votes().unwrap().is_empty());
        assert!(port.is_running());

        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![shared_vote_input("[旧]：@同", "pink", 10)],
            )
            .unwrap();
        assert!(port.read_votes().unwrap().is_empty());
        for _ in 0..2 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        shared_vote_input("[旧]：@同", "pink", 10),
                        shared_vote_input("[甲]：@同意", "pink", 30),
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            port.read_votes().unwrap(),
            vec!["[甲]：@同意", "[甲]：@同意"]
        );
        assert!(port.is_running());
    }

    #[test]
    fn active_vote_reacquisition_uses_the_vote_deadline_not_baseline_timeout() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        port.context.baseline_timeout = Duration::from_millis(1);
        let publisher = shared.clone();
        let publish_frames = thread::spawn(move || {
            for _ in 0..32 {
                wait_for_vote_refresh(&publisher);
                publisher
                    .publish(Instant::now(), ChatIdentity::PublicChannel, vec![])
                    .unwrap();
            }
            wait_for_vote_refresh(&publisher);
            publisher
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![shared_vote_input("restored baseline", "blue", 10)],
                )
                .unwrap();
        });
        let vote_deadline = Instant::now() + Duration::from_secs(2);

        assert!(
            port.poll_visible_friend_messages_until(vote_deadline)
                .unwrap()
                .is_empty()
        );
        publish_frames.join().unwrap();
        assert!(port.is_running());
    }

    #[test]
    fn shared_vote_stream_failures_end_vote_instead_of_allowing_timeout() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        shared.report_failure("OCR unavailable").unwrap();
        assert!(port.poll_visible_friend_messages().is_err());
        assert!(!port.is_running());
        // Only this vote stops; the application-wide running flag is untouched.
        assert!(port.context.running.load(AtomicOrdering::SeqCst));
    }

    fn wait_for_vote_refresh(shared: &ChatTimeline) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if shared.pending_refresh().unwrap().is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "vote did not request a fresh frame"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn shared_vote_refresh_recovers_from_typed_no_chat_without_replaying_votes() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        port.context.baseline_timeout = Duration::from_secs(2);
        let worker = thread::spawn(move || {
            let result = port.poll_visible_friend_messages();
            (port, result)
        });
        wait_for_vote_refresh(&shared);
        shared
            .publish_no_chat(Instant::now(), "temporary chat gap")
            .unwrap();
        wait_for_vote_refresh(&shared);
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![shared_vote_input("[旧]：@同意", "pink", 10)],
            )
            .unwrap();
        let (mut port, result) = worker.join().unwrap();
        assert!(result.unwrap().is_empty());
        assert!(port.is_running());
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    shared_vote_input("[旧]：@同意", "pink", 10),
                    shared_vote_input("[甲]：@不同意", "pink", 30),
                ],
            )
            .unwrap();
        assert_eq!(
            port.poll_visible_friend_messages().unwrap(),
            vec!["[甲]：@不同意"]
        );
        assert!(port.is_running());
    }

    #[test]
    fn shared_vote_refresh_retry_preserves_real_failure_and_waits_through_transitions() {
        for transition in [false, true] {
            let shared = ChatTimeline::new();
            let mut port = shared_vote_port(&shared);
            port.context.baseline_timeout = Duration::from_secs(2);
            let worker = thread::spawn(move || {
                let result = port.poll_visible_friend_messages();
                (port, result)
            });
            wait_for_vote_refresh(&shared);
            shared
                .publish_no_chat(Instant::now(), "same failure text")
                .unwrap();
            wait_for_vote_refresh(&shared);
            if transition {
                for _ in 0..32 {
                    shared
                        .publish(Instant::now(), ChatIdentity::PublicChannel, vec![])
                        .unwrap();
                    wait_for_vote_refresh(&shared);
                }
                shared
                    .publish(
                        Instant::now(),
                        ChatIdentity::PrimaryHall,
                        vec![shared_vote_input("restored baseline", "blue", 10)],
                    )
                    .unwrap();
            } else {
                // Identical reason text must not turn a genuine OCR failure into a retry.
                shared.report_failure("same failure text").unwrap();
            }
            let (port, result) = worker.join().unwrap();
            if transition {
                assert!(result.unwrap().is_empty());
                assert!(port.is_running());
            } else {
                assert!(result.is_err());
                assert!(!port.is_running());
            }
            assert!(port.context.running.load(AtomicOrdering::SeqCst));
        }
    }

    #[test]
    fn shared_vote_refresh_retry_remains_responsive_to_runtime_stop() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        port.context.baseline_timeout = Duration::from_secs(2);
        let running = Arc::clone(&port.context.running);
        let worker = thread::spawn(move || {
            let result = port.poll_visible_friend_messages();
            (port, result)
        });
        wait_for_vote_refresh(&shared);
        shared
            .publish_no_chat(Instant::now(), "temporary chat gap")
            .unwrap();
        wait_for_vote_refresh(&shared);
        let stopped_at = Instant::now();
        running.store(false, AtomicOrdering::SeqCst);
        let (port, result) = worker.join().unwrap();
        assert!(result.is_err());
        assert!(!port.is_running());
        assert!(stopped_at.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shared_vote_repeated_no_chat_keeps_original_refresh_deadline() {
        let shared = ChatTimeline::new();
        let mut port = shared_vote_port(&shared);
        port.context.baseline_timeout = Duration::from_millis(150);
        let running = Arc::clone(&port.context.running);
        let worker = thread::spawn(move || {
            let result = port.poll_visible_friend_messages();
            (port, result)
        });
        let test_deadline = Instant::now() + Duration::from_secs(2);
        let mut gaps = 0;
        while !worker.is_finished() && Instant::now() < test_deadline {
            if shared.pending_refresh().unwrap().is_some() {
                shared
                    .publish_no_chat(Instant::now(), "chat remains hidden")
                    .unwrap();
                gaps += 1;
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let finished_on_deadline = worker.is_finished();
        // Clean up a regression that accidentally extends the deadline on each gap.
        if !finished_on_deadline {
            running.store(false, AtomicOrdering::SeqCst);
        }
        let (port, result) = worker.join().unwrap();
        assert!(
            finished_on_deadline,
            "no-chat retries extended the refresh deadline"
        );
        assert!(gaps >= 1, "test must exercise a no-chat refresh");
        assert!(result.unwrap().is_empty());
        assert!(port.is_running());
    }

    #[test]
    fn shared_vote_open_rejects_stale_baseline_and_cancelled_runtime() {
        let shared = ChatTimeline::new();
        shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![])
            .unwrap();
        assert!(shared_vote_context(&shared).open().is_err());
        let context = shared_vote_context(&shared);
        context.running.store(false, AtomicOrdering::SeqCst);
        assert!(context.open().is_err());
    }

    #[test]
    fn shared_vote_baseline_binding_preserves_already_queued_new_votes() {
        let shared = ChatTimeline::new();
        let subscriber = shared.subscribe().unwrap();
        let refresh = shared.request_refresh().unwrap();
        let baseline = shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![])
            .unwrap();
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![shared_vote_input("[甲]：@不同意", "pink", 10)],
            )
            .unwrap();
        assert_eq!(
            shared
                .wait_for_snapshot(refresh, Duration::ZERO)
                .unwrap()
                .unwrap()
                .captured_at,
            baseline.captured_at
        );
        let mut port = AppModerationVotePort::from_baseline(
            shared_vote_context(&shared),
            subscriber,
            baseline,
        )
        .unwrap();
        assert_eq!(
            port.poll_visible_friend_messages().unwrap(),
            vec!["[甲]：@不同意"]
        );
    }

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
    fn read_only_moderation_conversion_keeps_source_and_hides_internal_context() {
        use crate::debug_read_only::{ReadOnlyObserver, ReadOnlyState};
        for color in ["blue", "pink"] {
            let service = test_moderation_service();
            let identity = moderation_identity();
            let mut source = moderation_routed("管理员");
            source.message_type = color.into();
            source.observation.message_text = Some("管理员：@拉黑123456789".into());
            let Some(ModerationStart::Ready(task)) =
                prepare_direct_blacklist(&service, &identity, &source).unwrap()
            else {
                panic!("direct task required");
            };
            let task = PendingTask::ModerationResult(task);
            let (payload, captured_at) =
                super::super::formal_task::read_only_task_parameters_with_source(
                    &task,
                    Some(&source),
                );
            assert_eq!(captured_at, source.observation.captured_at);
            assert_eq!(payload["kind"], "moderationResult");
            assert_eq!(payload["parametersOmitted"], "private moderation context");
            assert_eq!(payload["conversion"]["commandFamily"], "moderation");
            assert_eq!(payload["sourceCommand"]["messageType"], color);
            assert_eq!(payload["sourceCommand"]["nickname"], "管理员");
            let observer = ReadOnlyObserver::default();
            observer.set_enabled_state(true, ReadOnlyState::Push);
            observer.record_trigger(17, payload, captured_at);
            let evidence = observer.read_entry("readonly-1").unwrap();
            assert_eq!(evidence["taskId"], 17);
            assert_eq!(evidence["task"]["sourceCommand"]["nickname"], "管理员");
            if color == "blue" {
                assert_eq!(
                    evidence["task"]["sourceCommand"]["originalText"],
                    "管理员：@拉黑123456789"
                );
                assert_eq!(evidence["missingReason"], "exact_source_frame_not_retained");
            } else {
                assert!(evidence["task"]["sourceCommand"]["originalText"].is_null());
                assert!(!evidence.to_string().contains("123456789"));
                assert_eq!(evidence["missingReason"], "private_source_frame_omitted");
            }
        }
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
