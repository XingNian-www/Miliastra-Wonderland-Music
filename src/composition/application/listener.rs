use super::secondary_chat::{
    SecondaryHallCommandTracker, SecondaryListenerRoundState, SecondaryObservationOutcome,
};
use super::*;
use crate::observation::chat::{ChatIdentity, ChatScope, SourceId};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const CHAT_LISTENER_MODE_STATE_SUFFIX: &str = ".chat-listener-mode.json";
const CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CHAT_LISTENER_UNRESOLVED_UI_GRACE: Duration = Duration::from_secs(5);
const CONFIG_RELOAD_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CONFIG_RELOAD_UI_FALLBACK_GRACE: Duration = Duration::from_secs(15);
/// 等观察帧的切片长度：让主循环能在大约 25ms 内感知共享聊天发布并提前回顶部派发。
const FRAME_WAIT_SLICE: Duration = Duration::from_millis(25);

/// 排查日志用：折叠空白并截断，避免日志里塞入超长文本。
/// 命令锁锚点：OCR 消息用屏幕 y，外部消息用服务端序号。
///
/// 外部消息的 `block` 是哨兵坐标，拿它当屏幕锚点会让「同文本即同一条」
/// 对所有已记录命令成立，从而整批吞掉真实命令。
///
/// **只接受屏幕来源的 id。** 外部消息的原始服务端序号不存在于 id 里
/// （`id.bubble_sequence` 是去重键的 FNV 哈希，不单调，不能当锚点），
/// 所以外部消息必须自己带着 `source_sequence` 走到
/// `ChatTimelineMessage::lock_anchor()`，不能经这里推算。
fn lock_anchor_for(
    id: &crate::observation::chat::ObservedChatMessageId,
    screen_y: i32,
) -> crate::observation::chat::LockAnchor {
    use crate::observation::chat::LockAnchor;
    debug_assert!(
        id.is_from_screen(),
        "lock_anchor_for 只适用于屏幕消息；外部消息请用 ChatTimelineMessage::lock_anchor()"
    );
    LockAnchor::Screen { y: screen_y }
}

/// 日志里的聊天正文：先脱敏，再折叠空白与截断。
///
/// 脱敏必须在截断**之前**：歌单导入命令的分享链接可能很长，先截断会把
/// `@导入网易歌单` 前缀留住、把链接截掉一半，但脱敏判定是按整串匹配的，
/// 截断后可能不再命中，反而泄漏了链接片段。
///
/// 二级/歌单窗口路径一直用 `privacy::redacted_chat_text`；一级路径原先只折叠
/// 不脱敏，导致同一句 `@导入QQ歌单<链接>` 在一级会把完整链接写进日志文件。
fn compact_for_log(text: &str) -> String {
    let compact = crate::privacy::redacted_chat_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() > 120 {
        let mut truncated: String = compact.chars().take(120).collect();
        truncated.push('…');
        truncated
    } else {
        compact
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListenerReadyEvidence {
    PrimaryScan,
    SecondaryCurrentHall,
    GameLocation,
}

fn chat_listener_mode_path(playback_state_path: &Path) -> PathBuf {
    let mut file_name = playback_state_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("playback-state"))
        .to_os_string();
    file_name.push(CHAT_LISTENER_MODE_STATE_SUFFIX);
    playback_state_path.with_file_name(file_name)
}

fn listener_mode_matches_ui(
    mode: ChatListenerMode,
    temporary_primary: bool,
    ui_kind: UiStateKind,
) -> bool {
    matches!(
        (listener_residency(mode, temporary_primary), ui_kind),
        (UiResidency::Primary, UiStateKind::Primary)
            | (UiResidency::SecondaryCurrentHall, UiStateKind::Secondary)
    )
}

fn scan_frame_is_fresh(captured_at: Instant, not_before: Option<Instant>) -> bool {
    not_before.is_none_or(|boundary| captured_at >= boundary)
}

#[allow(dead_code)]
fn primary_rescan_can_publish(stable_kind: Option<UiStateKind>) -> bool {
    stable_kind == Some(UiStateKind::Primary)
}

fn automatic_startup_task(config: &AppConfig, kind: UiStateKind) -> Option<StartupTask> {
    if !config.startup.enabled {
        return None;
    }
    match kind {
        UiStateKind::GameGate if config.startup.enter_game => {
            Some(StartupTask::start_game(StartupSource::AUTOMATIC_UI))
        }
        UiStateKind::Overworld if config.startup.enter_wonderland => {
            Some(StartupTask::enter_wonderland(StartupSource::AUTOMATIC_UI))
        }
        _ => None,
    }
}

fn game_location_probe_blocks_residency(observation: Option<&UiStateObservation>) -> bool {
    matches!(observation, Some(UiStateObservation::Classified(state))
        if matches!(state.classification().kind(), UiStateKind::GameGate | UiStateKind::Overworld)
            || state.classification().label() == "unknown:game_probe")
}

fn chat_listener_mode_dispatch(parsed: &RoutedCommand) -> Option<ChatListenerModeCommand> {
    if parsed.permission_required.is_some() {
        return None;
    }
    let ModuleCommand::Administration(command) = &parsed.command else {
        return None;
    };
    let AdministrationDispatch::ChatListenerMode(command) = command.dispatch() else {
        return None;
    };
    Some(command)
}

fn listener_ready_after_recheck(
    evidence: Option<ListenerReadyEvidence>,
    snapshot: &ChatListenerSnapshot,
    task_busy: bool,
    paused: bool,
    running: bool,
) -> bool {
    if !running
        || paused
        || task_busy
        || snapshot.pending_mode.is_some()
        || snapshot.unread_task_pending
    {
        return false;
    }
    match evidence {
        Some(ListenerReadyEvidence::PrimaryScan) => {
            snapshot.mode == ChatListenerMode::Primary
                || (snapshot.mode == ChatListenerMode::Secondary && snapshot.temporary_primary)
        }
        Some(ListenerReadyEvidence::SecondaryCurrentHall) => {
            snapshot.mode == ChatListenerMode::Secondary
                && !snapshot.temporary_primary
                && !snapshot.initial_unread_clear
                && !snapshot.hall_round_required
        }
        Some(ListenerReadyEvidence::GameLocation) => true,
        None => false,
    }
}

/// 大厅归属闸门：一级值守只消费主大厅，二级值守只消费二级大厅。
///
/// 消费门控：这条消息该不该由当前值守模式处理。
///
/// **只看消息范围（`ChatScope`），不看屏幕分桶键（`ChatIdentity`）。**
///
/// 这道闸门存在的理由：一级区与二级区可能是同一个大厅，共享观察里的跨帧去重
/// 结构（`command_locks`、气泡键、业务认领）都按 `ChatIdentity` 分桶，
/// 两个桶互不相识，同一条大厅消息会被各派发一次。互斥成立在消费处。
///
/// 但「同屏去重」与「消息属于哪个容器」是两件事：
///
/// - OCR 的一级区/二级区确实是**两个屏幕桶**，需要按模式互斥；
/// - 外部聊天源没有屏幕，它的大厅消息借 `SecondaryHall` 桶只是为了共用去重结构，
///   并不代表「BOT 正看着二级界面」。
///
/// 旧实现按 `chat` 判断，导致用户把值守切到一级后，外部源的大厅命令被整批丢弃
/// （日志：`值守模式=一级值守 不消费 SecondaryHall 观察`）。
fn hall_consumes_scope(
    mode: ChatListenerMode,
    scope: &ChatScope,
    source: &SourceId,
    chat: Option<&ChatIdentity>,
) -> bool {
    if !scope.is_hall() {
        // 私聊与大厅无关，不受值守模式约束。
        return true;
    }
    // 屏幕互斥只对屏幕来源成立。
    //
    // 外部源没有屏幕：同一条消息只会经由一个来源到达，不存在「一级区与二级区
    // 各派发一次」的问题。把它纳入按模式互斥，会让用户切到一级后外部命令被整批丢弃。
    if !source.is_ocr() {
        return true;
    }
    // 走到这里一定是 OCR 帧，屏幕桶必然存在（`publish` 恒填 Some）。
    // `None` 会在下面落进 `_ => true`，那只是给「外部帧误走这里」留的兜底，
    // 不是一条正常路径。
    match chat {
        // OCR 的屏幕桶：按模式互斥，维持原有的同屏去重语义。
        Some(ChatIdentity::PrimaryHall) => mode == ChatListenerMode::Primary,
        Some(ChatIdentity::SecondaryHall) => mode == ChatListenerMode::Secondary,
        _ => true,
    }
}

fn replacement_ready_allowed(listener_ready: bool, startup_ready: bool) -> bool {
    listener_ready && startup_ready
}

fn unresolved_ui_recovery_due(
    unresolved_since: Option<Instant>,
    now: Instant,
    retry_after: Instant,
    pending_mode: bool,
    task_busy: bool,
) -> bool {
    !pending_mode
        && !task_busy
        && now >= retry_after
        && unresolved_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CHAT_LISTENER_UNRESOLVED_UI_GRACE
        })
}

fn secondary_hall_recovery_allowed(now: Instant, retry_after: Instant) -> bool {
    // 待处理重载可能因外部播放或不确定状态长期阻塞。恢复本身作为正式任务执行，
    // 可安全延迟停机，不会让好友值守器停在当前大厅之外。
    now >= retry_after
}

/// 上一次已推送的界面状态键。外层的 None 表示"还没推送过任何东西"。
static LAST_UI_TRANSITION: std::sync::Mutex<Option<UiTransitionKey>> = std::sync::Mutex::new(None);

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiTransitionKey {
    /// 分类尚未稳定。过渡态本身算**一个**状态：
    /// 一直在抖但始终没稳定时，不重复推送。
    Unstable,
    Stable(UiStateKind),
}

/// 判断这次观测是否构成一次**界面状态迁移**；返回 true 表示应当推送。
///
/// 只用**稳定类型**做判据：标签里还带着聊天标记计数，而计数每帧都在变，
/// 跟着它推会把事件流变成轮询，正是事件通道要消灭的东西。
fn take_ui_state_transition(kind: Option<UiStateKind>) -> bool {
    let key = match kind {
        Some(kind) => UiTransitionKey::Stable(kind),
        None => UiTransitionKey::Unstable,
    };
    let Ok(mut last) = LAST_UI_TRANSITION.lock() else {
        return false;
    };
    if *last == Some(key) {
        return false;
    }
    *last = Some(key);
    true
}

/// 监控页"界面"状态的中文显示名。
///
/// 只按"当前是什么界面"给结论，不在界面状态里复述命中了什么颜色/哪个模板
/// （这些判据保留在 /debug/ui-state 与 ui.state 事件的证据字段里，供排障核对）。
/// 入参是分类标签（如 `primary:marker blue=1 yellow=0 pink=0`、`panel:friend_list`、
/// `transition:overworld 1/2`），只取冒号前的状态族做映射。
fn ui_state_display_label(label: &str) -> String {
    let family = label.split(':').next().unwrap_or(label);
    let name = match family {
        "primary" => "一级大厅",
        "secondary" => "二级界面",
        "main_menu" => "千星主菜单",
        "game_gate" => "大门界面",
        "overworld" => "主世界",
        "loading" => "加载界面",
        "panel" => "模态面板",
        "unknown" => "未知界面",
        "transition" => "界面切换中",
        _ => return label.to_string(),
    };
    name.to_string()
}

/// 事件载荷的文本上限：事件是给控制器看的通知，不是完整日志，
/// 超长文本会把 256 条的重放缓冲挤满，让真正重要的事件被淘汰。
const MAX_EVENT_TEXT_CHARS: usize = 512;

fn truncate_for_event(text: &str) -> String {
    if text.chars().count() <= MAX_EVENT_TEXT_CHARS {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(MAX_EVENT_TEXT_CHARS).collect();
    truncated.push('…');
    truncated
}

/// 归一化聊天文本：只做**不丢信息**的修正，且**空白一律丢弃**。
///
/// - 全角 ASCII 折半角：放大实测截图确认，游戏里的冒号本来就是半角，
///   是 OCR 偶尔读成全角；折半角是把它改回屏幕上的真实字形，属于纠错。
/// - 空白全部丢弃：实测确认 OCR 的空白**两个方向都会错**——同一位置有时多读
///   一个空格（`@天子 没头脑`），有时整个漏掉（`@守望天空 只推荐`）。
///   既然空白不可信，保留它只会让同一句话时而带空格时而不带，
///   下游比较反而更不稳定；统一丢弃后文本才是确定且可逐字比较的。
fn normalize_chat_text(text: &str) -> String {
    // 唯一实现放在 crate::text：命令锁也必须复用同一份，否则同/全角抖动会互相打不中。
    crate::text::normalize_chat_text(text)
}

/// 已推送过的气泡身份（有界）。
///
/// 用**气泡身份**而不是文本相等判断"是不是同一条消息"：同一条消息被 OCR
/// 二次识别时，说话人可能迟到、标点可能全角半角、表情可能被读成乱码，
/// 这些都不是一条新消息。
static REMEMBERED_BUBBLES: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// 去重记忆的条数上限。丢掉最旧的只影响去重广度，不会让状态变得不正确。
const REMEMBERED_BUBBLE_LIMIT: usize = 256;

fn bubble_key(id: &crate::observation::chat::ObservedChatMessageId) -> String {
    use crate::observation::chat::ChatIdentity;
    let chat = match &id.chat {
        ChatIdentity::PrimaryHall => "primary_hall".to_string(),
        ChatIdentity::SecondaryHall => "secondary_hall".to_string(),
        ChatIdentity::Friend(name) => format!("friend:{name}"),
        ChatIdentity::PublicChannel => "public".to_string(),
        ChatIdentity::StrangerMessages => "strangers".to_string(),
        ChatIdentity::Unknown => "unknown".to_string(),
    };
    format!(
        "{}|{chat}|{}",
        id.visual_session.get(),
        id.bubble_sequence.get()
    )
}

/// 决定一条气泡是否应当出现在事件里：**每条气泡只推一次**。
///
/// 为什么不做事后补正：OCR 对特殊字符的抖动（半角/全角标点、表情被读成乱码、
/// 说话人迟一拍）与真实的"读得更准"在文本层面**无法可靠区分**。任何基于长度或
/// 相似度的补正规则都会把事件频率交回给 OCR 稳定性，正是要消灭的东西。
/// 因此这里选择少推：一条气泡产生且仅产生一个事件。这是本模块的明确取舍。
fn take_bubble_update(key: &str) -> bool {
    let Ok(mut seen) = REMEMBERED_BUBBLES.lock() else {
        // 锁中毒时选择推送而不是静默：宁可多推一条，也不要把游戏输入吞掉。
        return true;
    };
    if seen.iter().any(|known| known == key) {
        return false;
    }
    seen.push_back(key.to_string());
    while seen.len() > REMEMBERED_BUBBLE_LIMIT {
        seen.pop_front();
    }
    true
}

/// 只在调试模式下调用：把本会被丢弃的聊天观测发布到事件流，不执行任何命令。
///
/// 这样外部 AI 在驱动游戏的同时还能收到游戏内的输入，不必轮询；
/// 而"调试模式不跑自动化"这条安全边界原封不动。
fn publish_debug_chat_observations(dispatches: &[ChatObservationDispatch]) {
    let bus = crate::debug_events::bus();
    for dispatch in dispatches {
        match dispatch {
            ChatObservationDispatch::Primary { messages, .. } => {
                // 判"是不是新消息"用**气泡身份**：不用 is_new（实测会把同一屏每帧都
                // 标成新的，44 秒 13 条重复事件），也不用文本相等（会被 OCR 抖动击穿：
                // `:` vs `：`、说话人迟一拍）。
                let mut fresh = Vec::new();
                for item in messages {
                    if !take_bubble_update(&bubble_key(&item.id)) {
                        continue;
                    }
                    // 推送的文本仍然做一次归一化：客户端拿到的是可直接比较的规范串，
                    // 但这不影响事件频率（频率只由气泡身份决定）。
                    fresh.push(serde_json::json!({
                        "bubble": item.id.bubble_sequence.get(),
                        "type": item.message.message_type,
                        "text": truncate_for_event(&normalize_chat_text(&item.message.text)),
                    }));
                }
                if !fresh.is_empty() {
                    bus.publish(
                        "game.chat",
                        serde_json::json!({"scope": "primary", "messages": fresh}),
                    );
                }
            }
            ChatObservationDispatch::Secondary { observation, .. } => {
                bus.publish(
                    "game.chat",
                    serde_json::json!({
                        "scope": "secondary",
                        "messageType": observation.message_type,
                        "friendName": observation.friend_name,
                        "acceptsTurtleQuestions": observation.accepts_turtle_questions,
                        "messages": observation
                            .messages
                            .iter()
                            .map(|item| serde_json::json!({
                                "text": truncate_for_event(&item.text),
                                "sender": item.sender,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                );
            }
            ChatObservationDispatch::Gap(gap) => {
                // 观测缺口也必须让 AI 知道：否则它会拿不完整的状态当完整状态用。
                bus.publish(
                    "game.observation_gap",
                    serde_json::json!({"detail": format!("{gap:?}")}),
                );
            }
        }
    }
}

// 待处理重载可能因外部播放或不确定状态长期阻塞，因此不将其作为替代进程启动恢复的闸门：
// 子进程必须先完成启动并发布就绪，看门狗才能按正常进程处理。
fn replacement_startup_retry_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_actions_missing: bool,
    task_engine_idle: bool,
    now: Instant,
    retry_after: Instant,
) -> bool {
    config_reload_child
        && !child_ready
        && startup_actions_missing
        && task_engine_idle
        && now >= retry_after
}

#[allow(clippy::too_many_arguments)]
fn replacement_startup_fallback_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_gate_active: bool,
    startup_actions_available: bool,
    task_engine_idle: bool,
    paused: bool,
    now: Instant,
    unverified_since: Option<Instant>,
) -> bool {
    config_reload_child
        && !child_ready
        && !startup_gate_active
        && startup_actions_available
        && task_engine_idle
        && !paused
        && unverified_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CONFIG_RELOAD_UI_FALLBACK_GRACE
        })
}

pub(super) fn load_persisted_chat_listener_mode(
    playback_state_path: &Path,
) -> Result<Option<ChatListenerMode>> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取聊天值守模式状态失败: {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("解析聊天值守模式状态失败: {}", path.display()))
        .map(Some)
}

pub(super) fn persist_chat_listener_mode(
    playback_state_path: &Path,
    mode: ChatListenerMode,
) -> Result<()> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = serde_json::to_vec(&mode).context("序列化聊天值守模式状态失败")?;
    crate::adapters::file_store::write_atomic(&path, &bytes, "聊天值守模式状态")
}

use crate::features::administration::{
    AdministrationCommandContext, AdministrationDispatch, ImmediateAdministrationOutcome,
};

struct HttpHallDetector {
    hall_ui: HallUi,
}

impl http::HttpHallPort for HttpHallDetector {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>> {
        let outcome = self
            .hall_ui
            .submit_read(ReadHallInfo)
            .context("提交大厅截图检测 UI 事务")?
            .wait()
            .context("等待大厅截图检测 UI 事务")?;
        if let ReadHallInfoEffect::Failed(failure) = outcome.effect() {
            return Err(anyhow!("大厅截图检测失败：{failure}"));
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            return Err(anyhow!("大厅截图已生成，但一级驻留恢复失败：{failure}"));
        }
        outcome
            .screenshot()
            .ok_or_else(|| anyhow!("大厅检测未返回截图"))
    }
}

fn acknowledge_unrecognized_primary_message(
    chat_timeline: &crate::observation::chat::ChatTimeline,
    observed: &PrimaryObservedMessage,
    captured_at: Instant,
    baseline_primed: bool,
) -> Result<()> {
    if baseline_primed || observed.message.message_type != "pink" {
        chat_timeline.acknowledge_ignored(
            &observed.id,
            captured_at,
            &observed.message.text,
            None,
            None,
        )?;
    } else {
        // 仅粉色首屏历史永久消费；蓝色首屏的未识别文本仍可由有效修订交付。
        chat_timeline.acknowledge_business(&observed.id)?;
    }
    Ok(())
}

/// 一级命令的收尾：`write_lock` 只在真正派发成功点为 true；被禁用/被跳过的路径
/// 只确认不写锁。蓝色首屏允许执行，不再用启动 seed 把它永久跳过。
///
/// 缺陷 6：`message_text` / `message_y` 任一缺失就**不写锁**并 warn，绝不用
/// `unwrap_or_default` 退化成空串/0——空文本会静默不写锁，y=0 则会命中任何
/// `y <= 28` 的同文本气泡，把无关命令错误抑制掉。
fn acknowledge_primary_command(
    chat_timeline: &crate::observation::chat::ChatTimeline,
    command: &RoutedCommand,
    write_lock: bool,
) -> Result<()> {
    let Some(message_id) = command.observation.message_id.as_ref() else {
        return Ok(());
    };
    let acknowledged = if write_lock {
        // 用被派发消息自身捕获的文本与锚点，绝不回头查当前 tracker。
        match (
            command
                .observation
                .message_text
                .as_deref()
                .filter(|text| !text.trim().is_empty()),
            command.observation.lock_anchor,
        ) {
            (Some(text), Some(anchor)) => {
                chat_timeline.acknowledge_command_at(message_id, text, anchor)?
            }
            (text, anchor) => {
                log::warn!(
                    "命令锁未写入：观察缺少可信文本或锚点（chat={:?}, text={:?}, anchor={anchor:?}, id={message_id:?}），本轮只确认不写锁",
                    message_id.chat,
                    text.map(redacted_chat_text),
                );
                chat_timeline.acknowledge_business(message_id)?
            }
        }
    } else {
        chat_timeline.acknowledge_business(message_id)?
    };
    if !acknowledged {
        log::debug!("一级命令完成处理时消息已滚出当前画面: id={message_id:?}");
    }
    Ok(())
}

/// 二级/好友命令派发的唯一提交点：只有真正作为命令派发成功才写命令锁。
/// 普通闲聊、被禁用/未路由/被邀请去重的命令只确认（不写锁）。
fn complete_secondary_claim(
    claim: crate::observation::chat::SharedBusinessClaim,
    command_dispatched: bool,
) -> Result<()> {
    if command_dispatched {
        claim.complete_as_command()
    } else {
        claim.complete_ignored()
    }
}

impl ApplicationRuntime {
    fn poll_playlist_import_completions(&self) -> Result<()> {
        for messages in self
            .business
            .playlist_application
            .poll_import_completions(Instant::now())?
        {
            let rendered: Vec<String> = messages
                .iter()
                .flat_map(|reply| self.render_playlist_reply(reply))
                .collect();
            let messages = rendered.iter().map(String::as_str).collect::<Vec<_>>();
            self.reply_batch(&messages, 0)?;
        }
        Ok(())
    }

    fn persist_listener_mode_best_effort(&self, mode: ChatListenerMode) {
        if let Err(error) =
            persist_chat_listener_mode(&self.lifecycle.config.state.playback_state_path, mode)
        {
            log::error!("持久化聊天值守模式失败: {error:#}");
        }
    }

    pub(super) fn persist_current_chat_listener_mode(&self) {
        match self.business.business.chat_listener_snapshot() {
            Ok(snapshot) => self.persist_listener_mode_best_effort(snapshot.mode),
            Err(error) => log::error!("读取聊天值守模式用于持久化失败: {error}"),
        }
    }

    fn complete_chat_listener_mode(&self, mode: ChatListenerMode) -> Result<()> {
        self.business.business.complete_chat_listener_mode(mode)?;
        self.persist_listener_mode_best_effort(mode);
        Ok(())
    }

    pub(super) fn fail_chat_listener_mode_to_primary(&self) -> Result<()> {
        self.business
            .business
            .fail_chat_listener_mode_to_primary()?;
        self.persist_listener_mode_best_effort(ChatListenerMode::Primary);
        Ok(())
    }

    pub(super) fn clear_hall_countdown_cache_for_new_visual_session(
        &self,
        reason: &str,
    ) -> Result<bool> {
        let cleared = self.business.business.clear_hall_countdown_cache()?;
        self.reset_chat_observation_baseline(reason)?;
        if cleared {
            log::info!("{reason}，已清理大厅倒计时缓存，等待本次大厅检测重新确认");
        }
        Ok(cleared)
    }

    pub(super) fn reset_chat_observation_baseline(&self, reason: &str) -> Result<()> {
        self.ui
            .chat_baseline_primed
            .store(false, AtomicOrdering::SeqCst);
        self.ui.chat_timeline.note_baseline(reason)?;
        let visual_session = self.ui.chat_observations.begin_visual_session()?;
        log::info!(
            "{reason}，聊天观察进入新视觉会话：仅粉色首屏建立历史基线，其他颜色继续交付: {}",
            visual_session.get()
        );
        Ok(())
    }

    pub(super) fn warn_if_screen_size_mismatch(&self) -> Result<()> {
        let frame = match self.ui.game_ui.capture() {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("启动时未能截图，扫描循环将等待目标窗口恢复: {error:#}");
                return Ok(());
            }
        };
        if self.lifecycle.config.screen.warn_on_size_mismatch
            && (frame.width() != self.lifecycle.config.screen.expected_width
                || frame.height() != self.lifecycle.config.screen.expected_height)
        {
            log::warn!(
                "截图尺寸为 {}x{}，预期 {}x{}，程序继续运行",
                frame.width(),
                frame.height(),
                self.lifecycle.config.screen.expected_width,
                self.lifecycle.config.screen.expected_height
            );
        }
        Ok(())
    }

    pub(super) fn start_http_server(&mut self) -> Result<()> {
        if !self.lifecycle.config.http.enabled {
            return Ok(());
        }
        if self.lifecycle.http_server.is_some() {
            return Err(anyhow!("HTTP/Web 面板已经启动"));
        }
        let player_runtime = self
            .playback
            .player_runtime
            .as_ref()
            .ok_or_else(|| anyhow!("播放器运行时尚未启动"))?
            .handle();
        let formal_tasks = Arc::new(
            self.business
                .formal_tasks
                .clone()
                .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?,
        );
        // 调试沙盒复用与公开 HTTP 完全相同的播放控制入队路径，不另造旁路：
        // 这样"沙盒里能做的"和"API 能做的"永远一致，不会各自漂移。
        // 模拟聊天注入需要真身的链路句柄：timeline 决定消息进哪条队列，
        // identity 决定按什么角色授权，external 决定决策窗口走快路径还是 OCR。
        let simulation_external = self.lifecycle.external_chat.clone();
        let simulation = http_facade::ChatSimulation {
            timeline: self.ui.chat_timeline.clone(),
            identity: self.lifecycle.live_configs.identity.clone(),
            external_active: Arc::new(move || simulation_external.supplies_hall_messages()),
        };
        let debug_command_port: Arc<dyn http::HttpCommandPort> =
            Arc::new(http_facade::ApplicationHttpCommandFacade::with_simulation(
                formal_tasks.clone(),
                self.business.custom_workflow.clone(),
                simulation,
            ));
        let config_store = self
            .lifecycle
            .config_store
            .clone()
            .ok_or_else(|| anyhow!("配置中心存储尚未初始化"))?;
        let http_config = self.lifecycle.config.http.clone();
        let mut http_state = http::HttpSharedState::new(
            http::HttpInterfaceConfig::new(
                http_config,
                self.lifecycle.config.screen.clone(),
                self.lifecycle.config.templates.clone(),
                self.lifecycle.config.moderation.clone(),
                self.lifecycle.config.startup.clone(),
                self.lifecycle.config.invite.clone(),
                self.lifecycle.config.timing.clone(),
                self.lifecycle.config.custom_workflows.clone(),
            ),
            self.lifecycle.monitor.clone(),
            self.ui.latest_frame.clone(),
            config_store,
            self.lifecycle.live_configs.clone(),
            http::HttpApplicationPorts::new(
                Arc::new(http_facade::ApplicationHttpCommandFacade::with_simulation(
                    formal_tasks.clone(),
                    self.business.custom_workflow.clone(),
                    http_facade::ChatSimulation {
                        timeline: self.ui.chat_timeline.clone(),
                        identity: self.lifecycle.live_configs.identity.clone(),
                        external_active: {
                            let external = self.lifecycle.external_chat.clone();
                            Arc::new(move || external.supplies_hall_messages())
                        },
                    },
                )),
                formal_tasks.clone(),
                formal_tasks,
                Arc::new(HttpHallDetector {
                    hall_ui: self.ui.hall_ui.clone(),
                }),
                Arc::new(http_facade::ApplicationHttpPlayerFacade::new(
                    PlayerRuntimeBackend::new(player_runtime),
                    self.playback.player_search.clone(),
                    self.playback.native_playback.clone(),
                    self.playback.playback_application.play_mode_handle(),
                )),
                Arc::new(http_facade::ApplicationHttpLoginFacade::new(
                    self.playback.login_helper.clone(),
                )),
                Arc::new(http_facade::ApplicationHttpAiFacade::with_memory(
                    self.business.ai.clone(),
                    self.business.ai_chat_memory.clone(),
                    {
                        // 重跑清洗：闭包捕获可克隆的句柄，模型配置走实时快照，
                        // 这样面板请求时用的是当前生效的模型而不是装配时那份。
                        let memory = self.business.ai_chat_memory.clone();
                        let openai = self.business.ai_chat_openai.clone();
                        let connection = self.business.ai_connection.clone();
                        let timeout = self.business.ai_request_timeout;
                        let live_configs = self.lifecycle.live_configs.clone();
                        Arc::new(move |actor: &str| {
                            let config = live_configs.snapshot().ai_chat.clone();
                            crate::composition::application::ApplicationRuntime::rebuild_facts_with(
                                &memory,
                                &openai,
                                &connection,
                                timeout,
                                &config,
                                actor,
                            )
                        })
                    },
                )),
            ),
        );
        http_state.set_debug_port(Arc::new(http_facade::ApplicationHttpDebugFacade::new(
            self.ui
                .ui_runtime
                .as_ref()
                .ok_or_else(|| anyhow!("UI运行时尚未启动"))?
                .handle(),
            self.ui.ocr.clone(),
            &self.lifecycle.config.state.playback_state_path,
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .playback_queue_snapshot()
                        .map(|queue| queue.len())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .clear_playback_queue()
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .remove_playback_queue(crate::features::playback::QueueRemoval::Front)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let task_engine = self.business.task_engine.clone();
                move |task_id| {
                    task_engine
                        .cancel_formal(task_id)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            debug_command_port,
        )?));
        self.lifecycle.http_reload_blockers = http_state.active_reload_blockers.clone();
        self.lifecycle.http_reload_draining = http_state.reload_draining.clone();
        let server = http::start(http_state)?;
        self.lifecycle.http_server = Some(server);
        Ok(())
    }

    pub(super) fn start_hotkeys(&self) -> Result<hotkeys::HotkeyRuntime> {
        let task_engine = self.business.task_engine.clone();
        let shutdown = self.lifecycle.shutdown.clone();
        hotkeys::start(
            &self.lifecycle.config.hotkeys,
            Arc::clone(&self.lifecycle.running),
            Arc::clone(&self.lifecycle.paused),
            Arc::new(move || shutdown.request_user_exit()),
            Arc::new(move || {
                if let Err(error) = task_engine.wake() {
                    log::debug!("热键切换暂停状态后唤醒任务引擎失败: {error}");
                }
            }),
        )
    }

    fn activate_reload_startup_fallback(&self, reason: &str) -> Result<bool> {
        if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_none()
            || crate::config_reload_child_ready()
        {
            return Ok(false);
        }
        if !self.lifecycle.reload_startup.required_actions().is_empty() {
            return Ok(false);
        }
        if !self.require_reload_startup_automation()? {
            return Ok(false);
        }
        log::warn!("配置重载替代进程无法复用业务界面，启用启动自动化回退: {reason}");
        Ok(true)
    }

    fn retry_reload_startup_if_needed(
        &self,
        config_reload_child: bool,
        retry_after: &mut Instant,
    ) -> Result<()> {
        let missing = !self.lifecycle.reload_startup.missing_actions().is_empty();
        let now = Instant::now();
        if !replacement_startup_retry_due(
            config_reload_child,
            crate::config_reload_child_ready(),
            missing,
            self.business.task_engine.is_idle()?,
            now,
            *retry_after,
        ) {
            return Ok(());
        }
        match self.enqueue_reload_startup_tasks_if_needed() {
            Ok(true) => log::warn!("配置重载替代进程 startup 尚未完成，已按冷却重新排队缺失动作"),
            Ok(false) => {}
            Err(error) => log::error!("配置重载替代进程重排启动自动化失败: {error:#}"),
        }
        *retry_after = now + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        Ok(())
    }

    fn maybe_activate_reload_startup_after_ui_grace(
        &self,
        config_reload_child: bool,
        task_engine_idle: bool,
        unverified_since: &mut Option<Instant>,
        retry_after: &mut Instant,
        reason: &str,
    ) -> Result<()> {
        let now = Instant::now();
        let paused = self.lifecycle.paused.load(AtomicOrdering::SeqCst);
        let startup_gate_active = !self.lifecycle.reload_startup.required_actions().is_empty();
        let startup_actions_available =
            !ReloadStartupActions::from_startup_config(&self.lifecycle.config).is_empty();
        if config_reload_child
            && !crate::config_reload_child_ready()
            && !startup_gate_active
            && startup_actions_available
            && task_engine_idle
            && !paused
        {
            let since = *unverified_since.get_or_insert(now);
            if replacement_startup_fallback_due(
                config_reload_child,
                crate::config_reload_child_ready(),
                startup_gate_active,
                startup_actions_available,
                task_engine_idle,
                paused,
                now,
                Some(since),
            ) && self.activate_reload_startup_fallback(reason)?
            {
                *unverified_since = None;
                *retry_after = now + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
            }
        } else {
            *unverified_since = None;
        }
        Ok(())
    }

    fn poll_hall_expiry(
        &self,
        pause_applied: &mut bool,
        notice_sent: &mut bool,
        check_after: &mut Instant,
    ) {
        if Instant::now() < *check_after {
            return;
        }
        let retry_delay = match self.maybe_pause_expired_hall(pause_applied, notice_sent) {
            Ok(applied) => {
                *pause_applied = applied;
                if !applied {
                    *notice_sent = false;
                }
                Duration::from_secs(1)
            }
            Err(error) => {
                log::error!("大厅到期暂停或提示失败，将重试: {error:#}");
                Duration::from_secs(5)
            }
        };
        *check_after = Instant::now() + retry_delay;
    }

    pub(super) fn run_scan_loop(
        &mut self,
        shared_subscriber: crate::observation::chat::ChatTimelineSubscriber,
    ) -> Result<()> {
        let mut completion_subscriber = self
            .ui
            .chat_observations
            .subscribe_completion_advances()
            .context("订阅聊天观察完成推进")?;
        let scan_result = self.run_scan_loop_inner(&mut completion_subscriber, &shared_subscriber);
        let final_forward_result = self.forward_completion_advances(&mut completion_subscriber);
        match scan_result {
            Err(error) => {
                if let Err(forward_error) = final_forward_result {
                    log::error!("扫描循环退出时转发观察完成推进失败: {forward_error:#}");
                }
                Err(error)
            }
            Ok(()) => final_forward_result,
        }
    }

    // 共享 OCR 重构后，这些扫描状态只写不读；保留 `allow` 以消除噪音，待确认无后续用途再删。
    #[allow(unused_variables, unused_mut, unused_assignments)]
    fn run_scan_loop_inner(
        &mut self,
        completion_subscriber: &mut CompletionAdvanceSubscriber,
        shared_subscriber: &crate::observation::chat::ChatTimelineSubscriber,
    ) -> Result<()> {
        let template_args = self.ui.chat_templates.clone();
        let canvas = Canvas {
            width: self.lifecycle.config.screen.expected_width,
            height: self.lifecycle.config.screen.expected_height,
            resize: true,
        };
        let ui_handle = self
            .ui
            .ui_runtime
            .as_ref()
            .context("UI runtime 在扫描循环启动前已停止")?
            .handle();
        let initial_live_config = self.lifecycle.live_configs.snapshot();
        let mut frame_demand_ms = initial_live_config.timing.loop_idle_ms.max(1);
        let mut frame_demand = FrameDemand::new(Duration::from_millis(frame_demand_ms))
            .context("创建聊天观察帧需求")?;
        let mut frame_subscription: Option<FrameDemandSubscription> = None;
        let mut last_scan_boundary: Option<Instant> = None;
        let mut last_fingerprint: Option<ChangeFingerprint> = None;
        let mut last_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.fallback_ms);
        let mut last_change_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.change_cooldown_ms);
        let mut suppress_change_until = Instant::now();
        let mut force_scan_after: Option<Instant> = None;
        let mut force_scan_reason: Option<&'static str> = None;
        let mut primary_visible = false;
        let mut game_location_baseline_reset = false;
        let mut secondary_friend_bubble_fingerprint: Option<ChangeFingerprint> = None;
        let mut secondary_hall_bubble_sequence: Option<Vec<SecondaryHallBubble>> = None;
        let mut secondary_hall_command_tracker = SecondaryHallCommandTracker::default();
        let mut secondary_identity: Option<SecondaryChatIdentity> = None;
        let mut listener_residency_retry_after = Instant::now();
        let mut automatic_startup_pending = false;
        let mut automatic_startup_retry_after = Instant::now();
        let config_reload_child = std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some();
        let mut reload_startup_retry_after = Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        let mut replacement_ui_unverified_since = config_reload_child.then(Instant::now);
        let mut unresolved_ui_since: Option<Instant> = None;
        let mut target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
        let mut target_missing = false;
        let mut hall_expiry_pause_applied = false;
        let mut hall_expiry_notice_sent = false;
        let mut hall_expiry_check_after = Instant::now();
        let mut shared_backlog = std::collections::VecDeque::new();
        log::info!("自动化扫描已启动");
        while self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            self.poll_hall_expiry(
                &mut hall_expiry_pause_applied,
                &mut hall_expiry_notice_sent,
                &mut hall_expiry_check_after,
            );
            let live_config = self.lifecycle.live_configs.snapshot();
            if automatic_startup_pending
                && self.ui.coordinator.scan_may_run()
                && self.business.task_engine.is_idle()?
            {
                automatic_startup_pending = false;
                automatic_startup_retry_after =
                    Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
            }
            self.retry_reload_startup_if_needed(
                config_reload_child,
                &mut reload_startup_retry_after,
            )?;
            let next_frame_demand_ms = live_config.timing.loop_idle_ms.max(1);
            if next_frame_demand_ms != frame_demand_ms {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("主循环间隔变化时撤销旧观察帧需求失败: {error}");
                }
                frame_demand = FrameDemand::new(Duration::from_millis(next_frame_demand_ms))
                    .context("更新聊天观察帧需求")?;
                frame_demand_ms = next_frame_demand_ms;
            }
            self.consume_shared_chat_snapshots(shared_subscriber, &mut shared_backlog)?;
            self.forward_completion_advances(completion_subscriber)?;
            let loop_started = Instant::now();
            self.update_monitor_operational_state();
            self.tick_entertainment();
            let Some(scan_lease) = self.ui.coordinator.try_acquire_scan() else {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("正式任务占用 UI 时撤销观察帧需求失败: {error}");
                }
                self.invalidate_latest_frame();
                primary_visible = false;
                last_fingerprint = None;
                force_scan_after = None;
                force_scan_reason = None;
                secondary_friend_bubble_fingerprint = None;
                secondary_hall_bubble_sequence = None;
                secondary_hall_command_tracker.reset();
                secondary_identity = None;
                unresolved_ui_since = None;
                self.maybe_idle_exit()?;
                self.maybe_reload_config_when_idle();
                if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                    continue;
                }
                sleep(
                    self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)),
                );
                continue;
            };
            // Keep the frame, OCR and observation dispatch in one registered scan.
            // Direct UI routines stay protected; release before housekeeping or idle waits.
            let scan_boundary = scan_lease.not_before();
            if scan_boundary != last_scan_boundary {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("扫描新鲜度边界变化时撤销旧观察帧需求失败: {error}");
                }
                self.invalidate_latest_frame();
                primary_visible = false;
                game_location_baseline_reset = false;
                last_fingerprint = None;
                force_scan_after = None;
                force_scan_reason = None;
                secondary_friend_bubble_fingerprint = None;
                secondary_hall_bubble_sequence = None;
                secondary_hall_command_tracker.reset();
                secondary_identity = None;
                unresolved_ui_since = None;
                suppress_change_until = Instant::now();
                last_scan_boundary = scan_boundary;
            }
            let mut scan_lease = Some(scan_lease);
            if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
                drop(scan_lease.take());
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("暂停值守时撤销观察帧需求失败: {error}");
                }
                unresolved_ui_since = None;
                self.maybe_idle_exit()?;
                self.maybe_reload_config_when_idle();
                if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                    continue;
                }
                sleep(
                    self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)),
                );
                continue;
            }

            if frame_subscription.is_none() {
                frame_subscription = Some(
                    ui_handle
                        .declare_frame_demand(frame_demand)
                        .context("向 UI runtime 声明聊天观察帧需求")?,
                );
            }

            let mut listener_ready_evidence = None;
            let frame_started = Instant::now();
            // 等观察帧的同时，每 FRAME_WAIT_SLICE 检查共享聊天是否已发布新快照：
            // 有就提前结束等待、回循环顶部派发，避免把 OCR→派发拖满一整轮。
            let frame_deadline = frame_started + CONFIG_RELOAD_IDLE_CHECK_INTERVAL;
            let mut woken_by_snapshot = false;
            let mut frame_outcome: Result<Option<Frame>> = Ok(None);
            loop {
                match shared_subscriber.has_pending() {
                    Ok(true) => {
                        woken_by_snapshot = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        frame_outcome = Err(error);
                        break;
                    }
                }
                let now = Instant::now();
                if now >= frame_deadline {
                    break;
                }
                let slice = (frame_deadline - now).min(FRAME_WAIT_SLICE);
                match receive_observation_frame(
                    frame_subscription
                        .as_ref()
                        .expect("frame subscription initialized above"),
                    &canvas,
                    slice,
                ) {
                    Ok(None) => {}
                    other => {
                        frame_outcome = other;
                        break;
                    }
                }
            }
            match frame_outcome {
                Ok(Some(frame)) => {
                    if !scan_frame_is_fresh(frame.captured_at, scan_boundary) {
                        log::debug!("丢弃扫描新鲜度边界之前捕获的旧观察帧");
                        continue;
                    }
                    if let Ok(mut latest_frame) = self.ui.latest_frame.lock() {
                        latest_frame.store(Arc::clone(&frame.image));
                    } else {
                        log::error!("主扫描画面缓存锁已损坏");
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    log::debug!(target: "timing",
                        "观察帧交付: wait={}ms age={}ms",
                        frame_ms,
                        frame.captured_at.elapsed().as_millis()
                    );
                    if target_missing {
                        log::info!("目标窗口已恢复，重置截图退避");
                        self.reset_chat_observation_baseline("目标窗口恢复")?;
                        target_missing = false;
                    }
                    target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
                    let ui_started = Instant::now();
                    let ui_state_result: Result<(String, Option<UiStateKind>)> =
                        match frame.ui_state.clone() {
                            Some(UiStateObservation::Classified(state)) => {
                                Ok((state.to_string(), state.stable_kind()))
                            }
                            Some(UiStateObservation::Failed { reason, .. }) => {
                                Err(anyhow!(reason.to_string()))
                            }
                            None => Err(anyhow!(
                                "UI runtime 未附带统一模板状态观察，拒绝局部重复判定"
                            )),
                        };
                    match &ui_state_result {
                        Ok((ui_state, kind)) => {
                            // 界面状态迁移推送到事件流：迁移图应当能被"听"出来，
                            // 而不是靠轮询 /monitor 再逐个比对。
                            if take_ui_state_transition(*kind) {
                                crate::debug_events::bus().publish(
                                    "ui.state",
                                    serde_json::json!({
                                        "kind": kind.map(|kind| kind.name()),
                                        "stable": kind.is_some(),
                                        "label": ui_state,
                                    }),
                                );
                            }
                            self.lifecycle.monitor.publish(MonitorEvent::UiState(
                                ui_state_display_label(ui_state.as_str()),
                            ))
                        }
                        Err(_) => self
                            .lifecycle
                            .monitor
                            .publish(MonitorEvent::UiState("界面检测失败".to_string())),
                    }
                    let ui_ms = elapsed_ms(ui_started);
                    // 值守切换自愈：pending_mode 置位后必须有人在正式队列里执行
                    // SetChatListenerMode，否则它会永久悬挂——既切不到目标模式，
                    // 又因为 pending_mode.is_some() 挡住后续一切驻留恢复与再次切换。
                    // 外部聊天源、HTTP 面板都会先 request 再入队；这里兜住任何
                    // "只置位未入队"的路径（包括入队失败后没撤销的情况）。
                    self.reconcile_pending_listener_mode()?;
                    // 外部聊天源的默认值守：等**第一次真正停在一级聊天界面**时
                    // 切一次二级。放在这里而不是握手时，是因为握手发生在游戏还没
                    // 进界面的时候，那时按界面门控必然被跳过。
                    if let Ok((state, Some(UiStateKind::Primary))) = &ui_state_result {
                        self.maybe_switch_to_secondary(state.as_str())?;
                    }
                    let listener_snapshot = self.business.business.chat_listener_snapshot()?;
                    let command_executing = !self.business.task_engine.is_idle()?;
                    let mut residency_recovery_attempted = false;
                    let unresolved_ui_reason = match &ui_state_result {
                        Ok((ui_state, None)) => Some(ui_state.clone()),
                        Err(error) => Some(format!("状态分类失败: {error:#}")),
                        Ok((_, Some(_))) => None,
                    };
                    match &ui_state_result {
                        Ok((_, Some(kind @ (UiStateKind::GameGate | UiStateKind::Overworld)))) => {
                            unresolved_ui_since = None;
                            if !game_location_baseline_reset {
                                self.reset_chat_observation_baseline("已确认处于大门或主世界")?;
                                game_location_baseline_reset = true;
                            }
                            if !command_executing
                                && listener_snapshot.pending_mode.is_none()
                                && Instant::now() >= automatic_startup_retry_after
                                && let Some(task) = automatic_startup_task(&live_config, *kind)
                            {
                                log::info!("界面已稳定，排队执行自动进入: {}", task.label());
                                self.push_pending_task(PendingTask::Startup(task))?;
                                automatic_startup_pending = true;
                                residency_recovery_attempted = true;
                            } else if automatic_startup_task(&live_config, *kind).is_none() {
                                listener_ready_evidence = Some(ListenerReadyEvidence::GameLocation);
                            }
                        }
                        Ok((ui_state, Some(ui_kind))) => {
                            unresolved_ui_since = None;
                            game_location_baseline_reset = false;
                            let mode_matches = listener_mode_matches_ui(
                                listener_snapshot.mode,
                                listener_snapshot.temporary_primary,
                                *ui_kind,
                            );
                            let can_recover_residency = !mode_matches
                                && listener_snapshot.pending_mode.is_none()
                                && !command_executing
                                && *ui_kind != UiStateKind::Unknown
                                && Instant::now() >= listener_residency_retry_after;
                            if can_recover_residency {
                                let target = listener_residency(
                                    listener_snapshot.mode,
                                    listener_snapshot.temporary_primary,
                                );
                                log::warn!(
                                    "聊天值守逻辑模式与当前界面不一致，尝试恢复驻留: mode={} ui={} target={target:?}",
                                    listener_snapshot.mode.label(),
                                    ui_state
                                );
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery("恢复持久化聊天值守驻留"),
                                ) {
                                    Ok(()) => {
                                        log::info!("聊天值守驻留恢复完成: target={target:?}")
                                    }
                                    Err(error) => log::warn!(
                                        "聊天值守驻留恢复失败，保留逻辑模式并稍后重试: {error:#}"
                                    ),
                                }
                                listener_residency_retry_after =
                                    Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                residency_recovery_attempted = true;
                            }
                        }
                        _ if unresolved_ui_reason.is_some() => {
                            let ui_state = unresolved_ui_reason
                                .as_deref()
                                .expect("unresolved UI reason is present");
                            let now = Instant::now();
                            let unresolved_since = *unresolved_ui_since.get_or_insert(now);
                            if !game_location_probe_blocks_residency(frame.ui_state.as_ref())
                                && unresolved_ui_recovery_due(
                                    Some(unresolved_since),
                                    now,
                                    listener_residency_retry_after,
                                    listener_snapshot.pending_mode.is_some(),
                                    command_executing,
                                )
                            {
                                let target = listener_residency(
                                    listener_snapshot.mode,
                                    listener_snapshot.temporary_primary,
                                );
                                log::warn!(
                                    "界面持续无法验证，尝试恢复聊天值守驻留: mode={} ui={} target={target:?}",
                                    listener_snapshot.mode.label(),
                                    ui_state
                                );
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery(
                                        "恢复持续未知界面的聊天值守驻留",
                                    ),
                                ) {
                                    Ok(()) => log::info!(
                                        "持续未知界面的聊天值守驻留恢复完成: target={target:?}"
                                    ),
                                    Err(error) => log::warn!(
                                        "持续未知界面的聊天值守驻留恢复失败，稍后重试: {error:#}"
                                    ),
                                }
                                listener_residency_retry_after =
                                    Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                unresolved_ui_since = Some(Instant::now());
                                residency_recovery_attempted = true;
                            }
                        }
                        _ => {
                            unreachable!("unresolved UI reason must cover every non-stable result")
                        }
                    }
                    if residency_recovery_attempted {
                        primary_visible = false;
                        last_fingerprint = None;
                        secondary_friend_bubble_fingerprint = None;
                        secondary_hall_bubble_sequence = None;
                        secondary_hall_command_tracker.reset();
                        secondary_identity = None;
                        log::debug!("驻留恢复后废弃当前旧观察帧，下一轮重新验证");
                    } else {
                        match ui_state_result {
                            // 主菜单虽然是叠在大厅上的浮层，但它会盖住聊天区，
                            // 所以和 GameGate/Overworld 一样按"不在聊天界面"处理，
                            // 顺手丢弃旧的残留观察，菜单关闭后重新验证（§44.10）。
                            // 加载界面同理：此时根本没有聊天区，旧的观察必须作废（§55）。
                            Ok((
                                _,
                                Some(
                                    UiStateKind::GameGate
                                    | UiStateKind::Overworld
                                    | UiStateKind::MainMenu
                                    | UiStateKind::Loading
                                    | UiStateKind::Panel,
                                ),
                            )) => {
                                primary_visible = false;
                                last_fingerprint = None;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence = None;
                                secondary_hall_command_tracker.reset();
                                secondary_identity = None;
                            }
                            Ok((ui_state, None)) => {
                                log::debug!("界面仍在过渡，暂停聊天扫描: {}", ui_state);
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms state={} scanned=false",
                                    elapsed_ms(loop_started),
                                    frame_ms,
                                    ui_ms,
                                    ui_state
                                );
                            }
                            Ok((ui_state, Some(ui_kind)))
                                if listener_snapshot.mode == ChatListenerMode::Secondary
                                    && !listener_snapshot.temporary_primary =>
                            {
                                primary_visible = false;
                                last_fingerprint = None;
                                let secondary_started = Instant::now();
                                let allow_hall_recovery = secondary_hall_recovery_allowed(
                                    Instant::now(),
                                    listener_residency_retry_after,
                                );
                                let outcome = if ui_kind == UiStateKind::Secondary {
                                    Some(self.run_secondary_listener_round(
                                        &frame.image,
                                        SecondaryListenerRoundState {
                                            last_friend_bubble:
                                                &mut secondary_friend_bubble_fingerprint,
                                            hall_bubble_sequence:
                                                &mut secondary_hall_bubble_sequence,
                                            hall_command_tracker:
                                                &mut secondary_hall_command_tracker,
                                            identity: &mut secondary_identity,
                                        },
                                        allow_hall_recovery,
                                    )?)
                                } else if command_executing {
                                    log::debug!(
                                        "二级值守任务临时离开二级界面，等待任务状态机恢复: {}",
                                        ui_state
                                    );
                                    None
                                } else {
                                    log::warn!(
                                        "二级值守当前不在二级聊天界面: {}，保留逻辑模式并等待驻留恢复",
                                        ui_state
                                    );
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                    None
                                };
                                let scanned = outcome.is_some_and(|outcome| outcome.scanned);
                                if outcome.is_some_and(|outcome| outcome.verified) {
                                    listener_ready_evidence =
                                        Some(ListenerReadyEvidence::SecondaryCurrentHall);
                                }
                                if outcome.is_some_and(|outcome| outcome.recovery_requested) {
                                    listener_residency_retry_after =
                                        Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                }
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms secondary={}ms state={} scanned={}",
                                    elapsed_ms(loop_started),
                                    frame_ms,
                                    ui_ms,
                                    elapsed_ms(secondary_started),
                                    ui_state,
                                    scanned
                                );
                            }
                            Ok((_ui_state, Some(UiStateKind::Primary))) => {
                                primary_visible = true;
                                if listener_snapshot.mode == ChatListenerMode::Primary {
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                }
                                // Recognition is supplied independently, even while this navigation loop is blocked.
                                if self.ui.chat_timeline.latest()?.is_some_and(|snapshot| {
                                    snapshot.chat.as_ref() == Some(&ChatIdentity::PrimaryHall)
                                }) {
                                    listener_ready_evidence =
                                        Some(ListenerReadyEvidence::PrimaryScan);
                                }
                            }
                            Ok((ui_state, Some(UiStateKind::Secondary))) => {
                                primary_visible = false;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence = None;
                                secondary_hall_command_tracker.reset();
                                secondary_identity = None;
                                log::debug!("当前不是一级聊天界面，跳过聊天扫描: {}", ui_state);
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms state={} scanned=false",
                                    elapsed_ms(loop_started),
                                    frame_ms,
                                    ui_ms,
                                    ui_state
                                );
                                last_fingerprint = None;
                            }
                            Ok((ui_state, Some(UiStateKind::Unknown))) => {
                                primary_visible = false;
                                last_fingerprint = None;
                                log::debug!("界面状态仍为过渡态，暂停聊天扫描: {}", ui_state);
                            }
                            Err(error) => {
                                primary_visible = false;
                                log::error!("界面状态检测失败: {error:#}");
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms state=ui_error scanned=false",
                                    elapsed_ms(loop_started),
                                    frame_ms,
                                    ui_ms
                                );
                            }
                        }
                    }
                }
                Ok(None) => {
                    // A receive timeout is a normal polling tick, not a capture failure.
                    drop(scan_lease.take());
                    self.maybe_reload_config_when_idle();
                    if !woken_by_snapshot && self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                        self.maybe_activate_reload_startup_after_ui_grace(
                            config_reload_child,
                            self.business.task_engine.is_idle()?,
                            &mut replacement_ui_unverified_since,
                            &mut reload_startup_retry_after,
                            "观察帧长期未发布",
                        )?;
                    }
                    continue;
                }
                Err(error) => {
                    drop(scan_lease.take());
                    self.invalidate_latest_frame();
                    unresolved_ui_since = None;
                    if let Some(subscription) = frame_subscription.take()
                        && let Err(cancel_error) = subscription.cancel()
                    {
                        log::warn!("截图失败后撤销观察帧需求失败: {cancel_error}");
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    if !target_missing {
                        self.abort_entertainment_for_context_loss("目标游戏窗口已关闭或不可用");
                    }
                    self.lifecycle
                        .monitor
                        .publish(MonitorEvent::UiState("目标窗口不可用".to_string()));
                    primary_visible = false;
                    last_fingerprint = None;
                    secondary_friend_bubble_fingerprint = None;
                    secondary_hall_bubble_sequence = None;
                    secondary_hall_command_tracker.reset();
                    secondary_identity = None;
                    let observed_window_detection_generation =
                        self.ui.window_detection_signal.generation()?;
                    let target_missing_wait = self.reload_aware_wait(target_missing_backoff);
                    log::warn!(
                        "截图失败，{}秒后重试: {error:#}",
                        target_missing_wait.as_secs()
                    );
                    log::info!(target: "timing",
                        "主循环阶段耗时: total={}ms frame={}ms state=capture_error retry={}ms",
                        elapsed_ms(loop_started),
                        frame_ms,
                        target_missing_wait.as_millis()
                    );
                    target_missing = true;
                    self.maybe_idle_exit()?;
                    self.maybe_reload_config_when_idle();
                    if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                        continue;
                    }
                    if self.activate_reload_startup_fallback("目标窗口不可用")? {
                        replacement_ui_unverified_since = None;
                        reload_startup_retry_after =
                            Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
                    }
                    let window_retry_at = Instant::now() + target_missing_wait;
                    // 保持窗口捕获退避，同时继续检查大厅截止时间。
                    let window_reset = loop {
                        self.poll_hall_expiry(
                            &mut hall_expiry_pause_applied,
                            &mut hall_expiry_notice_sent,
                            &mut hall_expiry_check_after,
                        );
                        let now = Instant::now();
                        if !self.lifecycle.running.load(AtomicOrdering::SeqCst)
                            || now >= window_retry_at
                        {
                            break false;
                        }
                        let wait = window_retry_at
                            .saturating_duration_since(now)
                            .min(hall_expiry_check_after.saturating_duration_since(now));
                        if self
                            .ui
                            .window_detection_signal
                            .wait_for_change(observed_window_detection_generation, wait)?
                        {
                            break true;
                        }
                    };
                    if window_reset {
                        log::info!("收到窗口检测重置请求，立即重试并重置截图退避");
                        target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
                    } else {
                        target_missing_backoff =
                            next_target_missing_backoff(target_missing_backoff);
                    }
                    continue;
                }
            }
            if primary_visible
                && self.business.task_engine.is_idle()?
                && self.maybe_warn_hall_expiring()?
            {
                suppress_change_until = Instant::now()
                    + Duration::from_millis(live_config.timing.command.post_settle_ms);
                force_scan_after = Some(suppress_change_until);
                force_scan_reason = Some("hall-expiring");
                last_fingerprint = None;
                last_ocr_at = Instant::now();
            }
            drop(scan_lease.take());
            self.forward_completion_advances(completion_subscriber)?;
            self.maybe_idle_exit()?;
            self.maybe_reload_config_when_idle();
            self.poll_playlist_import_completions()?;
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                continue;
            }
            let ready_snapshot = self.business.business.chat_listener_snapshot()?;
            let ready_task_busy = !self.business.task_engine.is_idle()?;
            let listener_ready = listener_ready_after_recheck(
                listener_ready_evidence,
                &ready_snapshot,
                ready_task_busy,
                self.lifecycle.paused.load(AtomicOrdering::SeqCst),
                self.lifecycle.running.load(AtomicOrdering::SeqCst),
            );
            let startup_ready = self.lifecycle.reload_startup.is_satisfied();
            if replacement_ready_allowed(listener_ready, startup_ready)
                && crate::mark_config_reload_child_ready()?
            {
                if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some() {
                    log::info!("配置重载替代进程已 ready");
                }
            } else if listener_ready {
                // 必需的启动工作仍在运行或已经失败。仅验证屏幕不能确认退出码 77 的交接已就绪。
                replacement_ui_unverified_since = None;
            } else {
                self.maybe_activate_reload_startup_after_ui_grace(
                    config_reload_child,
                    !ready_task_busy,
                    &mut replacement_ui_unverified_since,
                    &mut reload_startup_retry_after,
                    "业务界面长期无法完成验证",
                )?;
            }
            sleep(self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)));
        }

        if let Some(subscription) = frame_subscription
            && let Err(error) = subscription.cancel()
        {
            log::warn!("扫描循环结束时撤销观察帧需求失败: {error}");
        }

        Ok(())
    }

    fn consume_shared_chat_snapshots(
        &mut self,
        subscriber: &crate::observation::chat::ChatTimelineSubscriber,
        backlog: &mut std::collections::VecDeque<crate::observation::chat::ChatTimelineSnapshot>,
    ) -> Result<()> {
        // 暂停值守：**必须照常取走快照并丢弃**，不能直接跳过读取。
        //
        // 发布侧不看暂停标志（观察线程仍在按 loop_idle_ms 推帧），所以只要不取，
        // 订阅者队列就会一路涨到 4096 上限。撞上限时发布侧会把订阅者标成失败并清空
        // 队列，下一次 read 直接报错——错误沿消费循环→扫描循环→主循环冒到 main，
        // 看门狗于是重启整个进程。表现就是「暂停两三个小时后程序自己退出」。
        //
        // 取走并丢弃等于「暂停期间的消息确实不处理」，符合暂停语义；恢复后
        // 靠 OCR 冷基线重建观察状态。积压一并清掉，避免恢复瞬间暴发式补跑历史命令。
        if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
            let discarded = subscriber.read()?.len().saturating_add(backlog.len());
            backlog.clear();
            if discarded > 0 {
                log::debug!("值守已暂停，丢弃 {discarded} 帧共享聊天观察，恢复后按冷基线重建");
            }
            return Ok(());
        }
        let snapshots = subscriber.read()?;
        if backlog.len().saturating_add(snapshots.len()) > 4096 {
            return Err(anyhow!(
                "共享聊天业务积压超过 4096 帧，停止值守以避免静默丢失"
            ));
        }
        backlog.extend(snapshots);
        if backlog.is_empty() {
            return Ok(());
        }
        // 大厅归属闸门按逻辑值守模式判定。本批次取一次模式：批次中途发生模式切换时，
        // 沿用既有顺序语义，不在这里新增一致性机制。
        let listener_mode = self.business.business.chat_listener_snapshot()?.mode;
        while !backlog.is_empty() {
            // 普通派发与独占起点原子协调；期间仍读取共享流并保留业务积压。
            let Some(_business_dispatch) =
                self.ui.chat_observations.try_begin_business_dispatch()?
            else {
                // 临时暂缓（backlog 保留，窗口结束后继续），走 debug 防刷屏。
                log::debug!(
                    "命令暂缓：独占聊天观察进行中，积压 {} 帧，窗口结束后继续",
                    backlog.len()
                );
                break;
            };
            let snapshot = backlog.pop_front().expect("nonempty business backlog");
            self.consume_shared_chat_snapshot(snapshot, listener_mode)?;
        }
        Ok(())
    }

    fn consume_shared_chat_snapshot(
        &mut self,
        snapshot: crate::observation::chat::ChatTimelineSnapshot,
        listener_mode: ChatListenerMode,
    ) -> Result<()> {
        let frame = self
            .ui
            .chat_observations
            .begin_frame(snapshot.captured_at)?;
        let result = (|| {
            // 二级大厅与主观察可能是同一个大厅，跨身份去重互不相识，因此按模式互斥消费：
            // 一级值守不消费二级大厅观察，二级值守不消费主大厅观察。
            // 这里只拦命令消费（不认领、不派发）；共享观察的发布、帧缓存与只读事件保持原样。
            if !hall_consumes_scope(
                listener_mode,
                &snapshot.scope,
                &snapshot.source,
                snapshot.chat.as_ref(),
            ) {
                if snapshot
                    .messages
                    .iter()
                    .any(|message| command::looks_like_command(&message.text))
                {
                    log::info!(
                        "命令未执行：值守模式={} 不消费 {:?} 观察（含命令样式消息）",
                        listener_mode.label(),
                        snapshot.chat
                    );
                }
                log::debug!(
                    "{}不消费{:?}观察，仅保留只读发布",
                    listener_mode.label(),
                    snapshot.chat
                );
                return Ok(());
            }
            // 分派桶：OCR 直接用屏幕桶，外部源按业务容器归到二级分派。
            //
            // 外部消息的 `chat` 恒为 None——它没有屏幕位置。二级分派分支内部
            // 一律用 `scope` 判 is_hall 与 friend_name，所以把外部帧交给它是对的。
            // 若哪天有人在那条分支里新增按 `chat` 判断的逻辑，外部私聊会被误伤；
            // 新增逻辑时请继续用 scope。
            let screen_bucket = match snapshot.chat.clone() {
                Some(bucket) => bucket,
                None => ChatIdentity::SecondaryHall,
            };
            match screen_bucket {
                ChatIdentity::PrimaryHall => {
                    let messages = snapshot
                        .messages
                        .into_iter()
                        .map(|message| {
                            let is_new = self
                                .ui
                                .chat_timeline
                                .is_business_pending_at(&message.id, snapshot.captured_at)?;
                            Ok(PrimaryObservedMessage {
                                id: message.id,
                                is_new,
                                message: ChatMessage {
                                    sender: message.sender,
                                    text: message.text,
                                    block: message.block,
                                    message_type: message.message_type,
                                },
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    self.dispatch_chat_observations(vec![ChatObservationDispatch::Primary {
                        frame,
                        messages,
                    }])?;
                }
                ref chat @ (ChatIdentity::SecondaryHall | ChatIdentity::Friend(_)) => {
                    // 业务容器看 scope，屏幕桶只看 chat。
                    //
                    // 外部聊天源的大厅消息被映射成 SecondaryHall 只是为了复用它去重结构，
                    // 真正的「是不是大厅 / 是哪个好友」由 scope 回答。这里以前用 chat
                    // 推导，等于让业务判定依赖屏幕位置——正是本轮要拆掉的耦合。
                    let is_hall = snapshot.scope.is_hall();
                    let accepts_turtle_questions = is_hall
                        && self.commands_enabled()?
                        && self.business.business.turtle_soup_accepts_questions()?;
                    // 好友名以业务容器为准：`Private.peer` 是身份判定的权威来源
                    // （外部源用 UID，OCR 用昵称，都由 `ChatScope::from_identity`
                    // 与 `external_chat_scope` 各自保证）。
                    //
                    // 第二个分支理论上不可达（`Friend` 的 scope 恒为 `Private`），
                    // 保留它作为映射漂移时的兜底：宁可退回屏幕桶里的名字，
                    // 也不要静默变成空串让下游以为「没有好友」。
                    let friend_name = match (&snapshot.scope, chat) {
                        (ChatScope::Private { peer }, _) => peer.to_string(),
                        (_, ChatIdentity::Friend(name)) => name.to_string(),
                        _ => String::new(),
                    };
                    for message in snapshot.messages {
                        if !self
                            .ui
                            .chat_timeline
                            .is_business_pending_at(&message.id, snapshot.captured_at)?
                        {
                            if command::looks_like_command(&message.text)
                                && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                            {
                                log::info!(
                                    "命令未执行：二级消息未判为新增追加 state={:?} chat={:?} text={}",
                                    self.ui.chat_timeline.business_delivery_state(&message.id)?,
                                    chat,
                                    compact_for_log(&message.text)
                                );
                            }
                            continue;
                        }
                        let revision_sender = message.sender.clone();
                        let revision_raw_sender = message.raw_sender.clone();
                        let sender = message
                            .raw_sender
                            .as_deref()
                            .map(parse_secondary_sender_identity)
                            .filter(|identity| !identity.effective_name().trim().is_empty())
                            .map(|identity| identity.effective_name().to_string())
                            // clone 而不是 move：下面还要从 message 取 lock_anchor()
                            // 与 text，部分移动会让它们不可用。
                            .or_else(|| message.sender.clone())
                            .filter(|sender| !sender.trim().is_empty());
                        if is_hall {
                            let classification = self.classify_secondary_business_message(
                                &message.text,
                                accepts_turtle_questions,
                            )?;
                            // 身份门禁看「有没有可授权身份」，不是「有没有昵称」。
                            //
                            // ChatBridge 协议保证每条用户消息都带 UID；没有 UID 的
                            // 不是用户消息（系统/观测帧），本就不该进这里。
                            // 旧实现只判 sender.is_none()，会把「有 UID、昵称未到」的
                            // 外部消息当作无身份拒掉——这是「接管后命令完全无效」的
                            // 第一道硬闸。
                            //
                            // MUST_READ §2 的「Unknown 不授权」仍然成立：
                            // 昵称与 UID 都没有才是 Unknown。
                            // 身份判定只有一处实现（features::command），避免两处漂移。
                            if classification.requires_identity
                                && !crate::features::command::has_known_identity(
                                    sender.as_deref(),
                                    message.account.as_ref(),
                                )
                            {
                                if command::looks_like_command(&message.text)
                                    && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                                {
                                    log::info!(
                                        "命令未执行：二级消息缺少可用身份（无 UID 也无昵称） text={}",
                                        compact_for_log(&message.text)
                                    );
                                }
                                continue;
                            }
                        }
                        let id = message.id.clone();
                        // 锚点跟着来源走：OCR 用屏幕 y，外部源用服务端序号。
                        // 外部消息的 block 是哨兵坐标，拿它当屏幕锚点会让任何
                        // 已记录条目都命中，同文本命令被整批吞掉。
                        // 必须在 message 被部分移动前取出来。
                        let lock_anchor = message.lock_anchor();
                        // Claim atomically against discard suppression and other
                        // queued revisions before dispatching business effects.
                        let Some(claim) = self.ui.chat_timeline.claim_business_at(
                            &id,
                            snapshot.captured_at,
                            &message.text,
                            lock_anchor,
                        )?
                        else {
                            if command::looks_like_command(&message.text)
                                && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                            {
                                log::info!(
                                    "命令未执行：二级命令认领失败（已被基线/已消费或修订否决）state={:?} text={}",
                                    self.ui.chat_timeline.business_delivery_state(&message.id)?,
                                    compact_for_log(&message.text)
                                );
                            }
                            continue;
                        };
                        // AI 触发需要正文与类型：dispatch 会移动 message，先留存。
                        let ai_candidate = crate::observation::chat::scan::ChatMessage {
                            message_type: message.message_type.clone(),
                            block: message.block,
                            text: message.text.clone(),
                            sender: message.sender.clone(),
                        };
                        // 外部源昵称可能缺失但 UID 必有：AI 触发要用它兜底。
                        let ai_account = message.account.clone();
                        // 正文现在是纯正文，昵称必须从 sender 字段带过去；
                        // sender 稍后会被移动进 observation，先克隆一份。
                        let ai_sender = sender.clone();
                        let outcome = self.dispatch_chat_observations(vec![
                            ChatObservationDispatch::Secondary {
                                frame,
                                observation: SecondaryChatObservation {
                                    message_type: message.message_type,
                                    friend_name: friend_name.clone(),
                                    accepts_turtle_questions,
                                    messages: vec![SecondaryObservedMessage {
                                        id: id.clone(),
                                        text: message.text,
                                        sender,
                                        account: message.account,
                                    }],
                                },
                            },
                        ])?;
                        // 关键词 AI 聊天：判据与一级路径完全一致——**路由失败之后**才
                        // 判定，而不是提前看「像不像命令」。`looks_like_command` 只是
                        // 样式启发，一条形似命令但没被任何模块认领的消息，在一级会去
                        // 尝试 AI 触发，在二级不该被判死。
                        if is_hall && outcome.unrecognized {
                            self.enqueue_ai_chat_keyword(
                                &id,
                                &ai_candidate,
                                ai_sender.as_deref(),
                                ai_account.as_ref(),
                                snapshot.captured_at,
                                lock_anchor,
                            )?;
                        }
                        // 缺陷 1：只有真正作为命令派发成功才写锁；
                        // 普通闲聊/被禁用/未路由只 ack，绝不能挤掉真命令条目。
                        if outcome.unrecognized {
                            claim.complete_unrecognized(
                                revision_sender.as_deref(),
                                revision_raw_sender.as_deref(),
                            )?;
                        } else {
                            complete_secondary_claim(claim, outcome.command_dispatched)?;
                        }
                    }
                }
                ChatIdentity::Unknown
                | ChatIdentity::PublicChannel
                | ChatIdentity::StrangerMessages => {
                    // 这些身份从不派发业务：Unknown 是身份没认出来，PublicChannel/
                    // StrangerMessages 不是玩家大厅。大厅里「等待身份修订」的消息
                    // 保持未确认，等 OCR 修订到达后再走大厅分支。
                    //
                    // 但必须留痕：这里是**永久 ACK**，一旦确认同一 id 不再交付。
                    // 二级面板昵称 OCR 失败时（高亮行读不出名字 → Unknown），屏上的
                    // `@点歌晴天` 会被这样静默吞掉，用户和日志都看不到任何证据。
                    for message in snapshot.messages {
                        if command::looks_like_command(&message.text)
                            && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                        {
                            log::info!(
                                "命令未执行：{:?} 身份不派发业务，永久丢弃 text={}",
                                snapshot.chat,
                                compact_for_log(&message.text)
                            );
                        }
                        self.ui.chat_timeline.acknowledge_business(&message.id)?;
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.ui.chat_observations.complete_without_messages(frame),
            Err(error) => {
                self.ui
                    .chat_observations
                    .record_terminal_failure(frame, format!("{error:#}"))?;
                Err(error)
            }
        }
    }

    fn forward_completion_advances(
        &self,
        subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Result<()> {
        loop {
            match self
                .ui
                .chat_observations
                .read_completion_advance(subscriber)?
            {
                Some(ObservationRead::Item { value, .. }) => self
                    .business
                    .business_events
                    .submit(BusinessEvent::CompletionAdvance(Arc::unwrap_or_clone(
                        value,
                    )))
                    .context("向业务运行时提交观察完成推进")?,
                Some(ObservationRead::Gap(gap)) => self
                    .business
                    .business_events
                    .submit(BusinessEvent::CompletionGap(gap))
                    .context("向业务运行时提交观察完成流缺口")?,
                None => return Ok(()),
            }
        }
    }

    pub(super) fn dispatch_chat_observations(
        &mut self,
        dispatches: Vec<ChatObservationDispatch>,
    ) -> Result<SecondaryObservationOutcome> {
        if crate::debug_mode::enabled() {
            // 调试模式下本进程不执行自动化——但"不执行"不等于"看不见"。
            // 外部 AI 恰恰是在这个模式下驱动游戏的，它必须知道游戏里说了什么、
            // 观测有没有漏。所以这里把原本要丢弃的观测**原样推送**出去：
            // 本进程当观察者，AI 当执行者。行为边界不变，仍然不执行任何自动化。
            publish_debug_chat_observations(&dispatches);
            let command_like: Vec<String> = dispatches
                .iter()
                .flat_map(|dispatch| match dispatch {
                    ChatObservationDispatch::Primary { messages, .. } => messages
                        .iter()
                        .map(|message| message.message.text.clone())
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .filter(|text| command::looks_like_command(text))
                .collect();
            if command_like.is_empty() {
                log::debug!("调试模式已启用，丢弃游戏内聊天命令与问题，不执行自动化");
            } else {
                log::info!(
                    "命令未执行：调试/完全控制模式启用，本进程不执行游戏内命令 text={}",
                    compact_for_log(&command_like.join(" | "))
                );
            }
            return Ok(SecondaryObservationOutcome::default());
        }
        let mut secondary = SecondaryObservationOutcome::default();
        for dispatch in dispatches {
            match dispatch {
                ChatObservationDispatch::Primary { frame, messages } => {
                    let messages = messages.into_iter().collect::<Vec<_>>();
                    self.handle_scan_messages(frame.captured_at(), messages)?;
                }
                ChatObservationDispatch::Secondary { frame, observation } => {
                    let outcome = self.process_secondary_chat_observation(frame, observation)?;
                    secondary.command_dispatched |= outcome.command_dispatched;
                    secondary.unrecognized |= outcome.unrecognized;
                }
                ChatObservationDispatch::Gap(gap) => {
                    self.ui
                        .chat_baseline_primed
                        .store(false, AtomicOrdering::SeqCst);
                    log::warn!(
                        "聊天观察流出现缺口，下一屏重建观察状态（仅粉色建立历史基线）: kind={:?} missing={:?}..={:?}",
                        gap.kind,
                        gap.missing_from,
                        gap.missing_through
                    );
                }
            }
        }
        Ok(secondary)
    }

    pub(super) fn mapped_actor_name(&self, ocr_name: &str) -> String {
        self.lifecycle.live_configs.identity.display_name(ocr_name)
    }

    pub(super) fn canonical_actor_name(&self, ocr_name: &str) -> String {
        self.lifecycle
            .live_configs
            .identity
            .canonical_name(ocr_name)
    }

    /// 歌单归属键的候选列表，按优先级：UID > 身份映射 UUID > 昵称。
    ///
    /// UID 最高是因为它能跨改名稳定定位；身份映射次之（用户显式声明同一人）；
    /// 昵称兜底。读取时依次尝试，写入时用第一个可用的。
    /// 歌单归属只认身份 UUID：没有登记身份映射的玩家没有歌单。
    ///
    /// 返回空表示"无歌单"，调用方应提示用户先配置身份映射，
    /// 而不是退化成按昵称归属（昵称会改、会重名、OCR 会读错）。
    pub(super) fn playlist_owner_candidates(
        &self,
        subject: &str,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
    ) -> Vec<String> {
        // 与权限判定同源：有 UID 就只认 UID，没有才按游戏内昵称。
        //
        // 原来这里直接调 `resolve(ocr_name)`（只按 nickname 精确匹配）。
        // 外部聊天源的私聊 peer 是**数字 UID**，传进去必然落空，
        // 已登记的玩家被判成未登记——线上表现为大厅能导入歌单、
        // 好友私聊提示「该玩家未在身份映射登记」。
        let hint = match account {
            Some(account) => crate::features::identity::IdentityHint::Account {
                game: &account.game,
                uid: &account.uid,
                nickname: Some(subject),
            },
            None => crate::features::identity::IdentityHint::Nickname(subject),
        };
        let identity = self
            .lifecycle
            .live_configs
            .identity
            .resolve_hint(hint)
            .map(|resolved| resolved.id.to_string());
        crate::features::playlist::owner_key::candidates(identity.as_deref())
    }

    /// 写入用的归属键：候选中优先级最高的那个。
    /// 写入用的归属键；没有身份映射时为 `None`（调用方应拒绝操作）。
    pub(super) fn playlist_owner_key(
        &self,
        subject: &str,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
    ) -> Option<String> {
        self.playlist_owner_candidates(subject, account)
            .into_iter()
            .next()
    }

    fn map_command_actor(&self, mut command: RoutedCommand) -> RoutedCommand {
        // 有账号时按 UID 反查真实名字。
        //
        // `username` 在外部聊天源下可能是 UID（私聊容器的 peer 取的是 to_uid，
        // 也就是收件人＝BOT 自己），拿去当昵称查必然落空、原样返回一串数字，
        // 于是大厅通告会念出「900000001邀请BOT前往大厅」。
        // 帐号里的 uid 才是真正的发送者。
        let actor = match command.account.as_ref() {
            Some(account) => {
                let names = self
                    .lifecycle
                    .live_configs
                    .identity
                    .friend_row_names_for_account(&account.game, &account.uid);
                // 反查不到时退回原本的昵称路径（OCR 场景 account 为 None，走 else 分支）。
                names
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.mapped_actor_name(&command.username))
            }
            None => self.mapped_actor_name(&command.username),
        };
        match &mut command.command {
            ModuleCommand::Invite(invite) => invite.display_name = actor,
            ModuleCommand::Moderation(moderation) => moderation.requester = actor,
            _ => {}
        }
        command
    }

    fn handle_scan_messages(
        &mut self,
        captured_at: Instant,
        observed_messages: Vec<PrimaryObservedMessage>,
    ) -> Result<()> {
        let active_entertainment = self.business.business.active_entertainment()?;
        let accepts_turtle_questions = self.business.business.turtle_soup_accepts_questions()?;
        let access_config = self.lifecycle.live_configs.snapshot();
        let command_router = ChatCommandRouter::with_access(
            &self.business.custom_workflow,
            &self.lifecycle.live_configs.identity,
            &access_config.command_access,
        );
        if observed_messages.is_empty() {
            log::debug!("没有找到聊天标志，本轮不更新命令锁");
            return Ok(());
        }

        let baseline_primed = self.ui.chat_baseline_primed.load(AtomicOrdering::SeqCst);
        let mut visible_turtle_questions = Vec::new();
        let mut parsed = Vec::new();
        for observed in &observed_messages {
            let message = &observed.message;
            let sample_as_question = accepts_turtle_questions
                && message.message_type == "blue"
                && !message.text.is_empty();
            let command_like = command::looks_like_command(&message.text);
            if !observed.is_new && !sample_as_question {
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    let state = self
                        .ui
                        .chat_timeline
                        .business_delivery_state(&observed.id)?;
                    log::info!(
                        "命令未执行：未判为新增追加 state={:?}（Pending=仅排队未派发/Claimed=已认领/IgnoredRevision=等待有效修订/Fenced=交付栅栏/NotTracked=未入账，通常为基线或视图不连续）type={} text={}",
                        state,
                        message.message_type,
                        compact_for_log(&message.text)
                    );
                }
                continue;
            }
            log::debug!("处理一级观察消息: id={:?}", observed.id);
            if message.text.is_empty() {
                if observed.is_new {
                    acknowledge_unrecognized_primary_message(
                        &self.ui.chat_timeline,
                        observed,
                        captured_at,
                        baseline_primed,
                    )?;
                }
                continue;
            }

            let observation = CommandObservation {
                frame_id: None,
                captured_at: Some(captured_at),
                message_id: Some(observed.id.clone()),
                // 命令锁只认被派发消息自身的数据：这里连同快照一起捕获文本与锚点。
                // 锚点跟着来源走，外部消息用序号而不是哨兵坐标。
                message_text: Some(message.text.clone()),
                lock_anchor: Some(lock_anchor_for(&observed.id, message.block.y)),
            };
            // 正文已由扫描层切成纯正文，昵称在 message.sender 里。
            let envelope = command::parse_command_envelope(
                &message.text,
                &message.message_type,
                message.sender.as_deref(),
                observation,
            );
            let routed = envelope
                .as_ref()
                .and_then(|envelope| command_router.route(envelope, active_entertainment));

            // 关键词 AI 聊天：不是 @ 命令，因此在路由失败后单独判定。
            // 与命令互斥：一条消息要么是命令，要么是闲聊触发，不会被处理两次。
            if routed.is_none() && observed.is_new {
                self.enqueue_ai_chat_keyword(
                    &observed.id,
                    &message,
                    // 昵称已由扫描层从正文剥出。
                    message.sender.as_deref(),
                    None,
                    captured_at,
                    lock_anchor_for(&observed.id, message.block.y),
                )?;
            }

            if sample_as_question && routed.is_none() {
                if let Some(question) = turtle_soup::parse_question_message(&message.text, None) {
                    visible_turtle_questions.push(question);
                }
            }
            if !observed.is_new {
                // 海龟汤提问采样开启时 sample_as_question 对所有非空蓝消息为真，会走到此处；
                // 若并非新增追加，则命令不会执行，需留痕。
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    log::info!(
                        "命令未执行：海龟汤提问采样中且本轮未判为新增追加 type={} text={}",
                        message.message_type,
                        compact_for_log(&message.text)
                    );
                }
                continue;
            }
            let Some(parsed_command) = routed else {
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    let reason = if envelope.is_none() {
                        "无法解析为命令信封（发送者/前缀或结构化点歌不成立）"
                    } else {
                        "信封可解析但无模块认领（或保留的决策语法被正常分发排除）"
                    };
                    log::info!(
                        "命令未执行：{} type={} text={}",
                        reason,
                        message.message_type,
                        compact_for_log(&message.text)
                    );
                }
                acknowledge_unrecognized_primary_message(
                    &self.ui.chat_timeline,
                    observed,
                    captured_at,
                    baseline_primed,
                )?;
                continue;
            };
            let parsed_command = self.map_command_actor(parsed_command);
            if let ModuleCommand::Playlist(command) = &parsed_command.command {
                let owner = self.canonical_actor_name(&parsed_command.username);
                let application = &self.business.playlist_application;
                if !application.has_command_context(&owner, command, Instant::now())? {
                    log::info!(
                        "命令未执行：歌单命令缺少当前窗口上下文 text={}",
                        compact_for_log(&message.text)
                    );
                    acknowledge_unrecognized_primary_message(
                        &self.ui.chat_timeline,
                        observed,
                        captured_at,
                        baseline_primed,
                    )?;
                    continue;
                }
            }
            if let ModuleCommand::Invite(invite) = &parsed_command.command
                && !self.business.business.invite_should_accept(invite.seq)?
            {
                let seq = invite.seq.expect("unsequenced invites are always accepted");
                log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed_command.raw);
                self.ui.chat_timeline.acknowledge_business(&observed.id)?;
                continue;
            }
            if matches!(
                parsed_command.command,
                ModuleCommand::Playlist(crate::features::playlist::PlaylistCommand::Import { .. })
            ) {
                log::debug!("解析到歌单导入命令（分享链接已隐藏）");
            } else {
                log::debug!("解析命令: {}", parsed_command.raw);
            }
            parsed.push(parsed_command);
        }

        // 问题只按大厅画面的可见顺序做跨帧稳定采样；不与命令互相排序或回映射。
        let questions = self
            .business
            .business
            .filter_turtle_soup_primary_questions(visible_turtle_questions, false)?;
        self.ui
            .chat_baseline_primed
            .store(true, AtomicOrdering::SeqCst);

        for routed in parsed {
            if self.command_is_disabled(&routed)? {
                // 文案必须反映真实原因：这里既可能是全局命令开关关闭，
                // 也可能是该命令的最低角色/单条启用策略拒绝。写成「识别已禁用」
                // 会把人引去查全局开关，而实际原因是权限。
                log::info!(
                    "命令未执行：权限或启用策略拒绝（全局开关或该命令的最低角色） text={}",
                    routed.raw
                );
                // 被禁用不是“执行成功”，只确认不写锁。
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, false)?;
                continue;
            }
            if self.enqueue_chat_listener_command(&routed)? {
                // 真正派发成功：写锁作为持久墓碑，重建后同一屏命令不再派发。
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
                continue;
            }
            if self.apply_immediate_administration(&routed, false)? {
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
                continue;
            }
            let pending = PendingCommand {
                lock_key: command::lock_key(&routed),
                routed: routed.clone(),
            };
            self.enqueue_pending_command(pending)?;
            acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
        }
        if self.commands_enabled()? {
            for question in questions {
                self.enqueue_turtle_soup_question(question, captured_at)?;
            }
        }
        Ok(())
    }

    /// 关键词触发的 AI 聊天：把命中的闲聊转成一次 `AiChatCommand::Ask`。
    ///
    /// 不是命令：不写成功命令锁、不参与命令路由；只受冷却与开关约束。
    fn enqueue_ai_chat_keyword(
        &self,
        message_id: &crate::observation::chat::ObservedChatMessageId,
        message: &crate::observation::chat::scan::ChatMessage,
        sender: Option<&str>,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
        captured_at: Instant,
        lock_anchor: crate::observation::chat::LockAnchor,
    ) -> Result<()> {
        let config = self.lifecycle.live_configs.snapshot().ai_chat.clone();
        // 大厅消息没有独立 sender 字段，昵称在文本行首，与命令信封共用同一套切分。
        //
        // 拆不出昵称时退回账号 UID：外部聊天源的昵称可能为 null（API 文档 §5），
        // 但 UID 必填。只按昵称判断会让这类消息整条静默失效——它不是 Unknown，
        // 只是缺一个展示名。正文此时本来就是纯正文（投递层没补前缀）。
        // 发送者来源按可信度排序：显式 sender（外部源/二级面板）> 正文行首（一级 OCR）
        // > 账号 UID（昵称缺失时的兜底）。
        let split = sender
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| (name.to_string(), message.text.as_str()))
            .or_else(|| {
                crate::interfaces::chat::split_sender_line(&message.text, &message.message_type)
            })
            .or_else(|| account.map(|account| (account.uid.clone(), message.text.as_str())));
        // 全部门槛收敛在 features::ai_chat::keyword_trigger：原来每个不满足的分支都
        // 直接 return Ok(()) 且不打日志，用户只看到「完全静默」。收敛成一条不可
        // 静默的日志，也避免以后新增门槛时又悄悄吞掉触发。
        let (sender, question) = match crate::features::ai_chat::keyword_trigger(
            &config,
            self.business.ai.enabled(),
            &message.message_type,
            split,
        ) {
            crate::features::ai_chat::KeywordTrigger::Ask { sender, question } => {
                // 权限判定放在触发之后：先确认这句话确实是在叫机器人，
                // 再判断叫的人有没有资格，避免把普通闲聊记成「越权」。
                let role = self
                    .lifecycle
                    .live_configs
                    .identity
                    .role_of_sender(&sender, account);
                if !config.min_access.allows(role) {
                    log::info!(
                        "AI 聊天未触发：需要{}才能触发 sender={} role={:?}",
                        config.min_access.label(),
                        sender,
                        role
                    );
                    return Ok(());
                }
                (sender, question)
            }
            crate::features::ai_chat::KeywordTrigger::Skip(skip) => {
                // 用 info 而不是 debug：logging.level 默认 info（LoggingConfig::default），
                // debug 级日志在默认配置下根本不落盘，排查时还是「什么都看不到」。
                // 只在真正收到聊天消息时打印，不会淹没日志。
                log::info!(
                    "AI 聊天未触发：{} type={} text={}",
                    skip.reason(),
                    message.message_type,
                    compact_for_log(&message.text)
                );
                return Ok(());
            }
        };
        log::info!(
            "AI 聊天触发: sender={sender} question={}",
            compact_for_log(&question)
        );
        let mut routed = RoutedCommand::console(
            &config.keyword,
            &message.text,
            ModuleCommand::AiChat(crate::features::ai_chat::AiChatCommand::Ask { question }),
        );
        routed.username = sender.to_string();
        routed.message_type = message.message_type.clone();
        routed.authority = CommandAuthority::HallMember;
        // 身份必须与触发期判定**同源**。
        //
        // 触发期用的是 `role_of_sender(sender, account)`——有 UID 就按 UID 判；
        // 而 `RoutedCommand::console` 构造出的 `account` 恒为 None，
        // 如果这里只回填 `role_of(&sender)`（仅按昵称），
        // 那么「UID 认出 Owner、昵称却是随机 OCR 备注名」的发送者会
        // 触发期放行、执行期 role=None，命令被静默丢弃。
        routed.account = account.cloned();
        routed.role = self
            .lifecycle
            .live_configs
            .identity
            .role_of_sender(&sender, routed.account.as_ref());
        routed.observation = crate::features::command::CommandObservation {
            frame_id: None,
            captured_at: Some(captured_at),
            message_id: Some(message_id.clone()),
            message_text: Some(message.text.clone()),
            lock_anchor: Some(lock_anchor),
        };
        let pending = PendingCommand {
            lock_key: crate::interfaces::chat::lock_key(&routed),
            routed,
        };
        self.enqueue_pending_command(pending)?;
        Ok(())
    }

    /// 全局命令开关只约束「大厅普通成员」；好友与已映射角色不受影响。
    ///
    /// AI 转述的命令也必须过这一关，否则管理员关掉命令后，
    /// 真人 `@点歌` 不执行、AI 转述的同一条却照跑。
    pub(super) fn command_is_disabled(&self, command: &RoutedCommand) -> Result<bool> {
        // 控制台来源仅由已鉴权HTTP/内部任务构造，游戏颜色解析不能创建此来源。
        if command.message_type == "控制台" {
            return Ok(false);
        }
        let snapshot = self.lifecycle.live_configs.snapshot();
        let (_, policy) = crate::features::command_access::resolve_with_active(
            &snapshot.command_access,
            command,
            self.business.business.active_entertainment()?,
        );
        // 与路由期同一判定：有账号按 UID，否则按昵称。只按昵称会让外部源的
        // 管理员命令「路由放行、执行拒绝」——昵称是随机 OCR 匹配键，查不到 UID 绑定。
        let role = self
            .lifecycle
            .live_configs
            .identity
            .role_of_sender(&command.username, command.account.as_ref());
        let actor = crate::features::command_access::actor_permission(role);
        Ok(!policy.allows(actor)
            || (!self.commands_enabled()?
                && command.authority == CommandAuthority::HallMember
                && role.is_none()))
    }

    fn enqueue_chat_listener_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        let Some(command) = chat_listener_mode_dispatch(parsed) else {
            return Ok(false);
        };
        match command {
            ChatListenerModeCommand::Status => {
                let snapshot = self.business.business.chat_listener_snapshot()?;
                let pending = snapshot
                    .pending_mode
                    .map(|mode| format!("，等待切换{}", mode.label()))
                    .unwrap_or_default();
                let message = format!("值守模式状态: {}{}", snapshot.mode.label(), pending);
                log::info!("{}", message);
                self.lifecycle
                    .monitor
                    .publish(MonitorEvent::Command(format!(
                        "{} -> {}",
                        parsed.user_command, message
                    )));
                crate::debug_read_only::record_command_immediate(
                    parsed,
                    "chatListenerStatus",
                    message,
                );
            }
            ChatListenerModeCommand::Primary | ChatListenerModeCommand::Secondary => {
                let target = match command {
                    ChatListenerModeCommand::Primary => ChatListenerMode::Primary,
                    ChatListenerModeCommand::Secondary => ChatListenerMode::Secondary,
                    ChatListenerModeCommand::Status => unreachable!(),
                };
                let (queued, snapshot) =
                    self.business.business.request_chat_listener_mode(target)?;
                if !queued {
                    log::info!(
                        "值守模式切换已处于当前或等待状态，跳过: current={} pending={:?}",
                        snapshot.mode.label(),
                        snapshot.pending_mode
                    );
                    return Ok(true);
                }
                self.record_command_activity(command_observed_at(parsed))?;
                if let Err(error) = self.push_pending_task_with_command_source(
                    PendingTask::SetChatListenerMode { target },
                    parsed,
                ) {
                    self.business
                        .business
                        .cancel_chat_listener_mode_request(target)?;
                    return Err(error);
                }
                log::info!("值守模式切换已加入待处理队列: {}", target.label());
            }
        }
        Ok(true)
    }

    pub(super) fn enqueue_turtle_soup_question(
        &self,
        question: turtle_soup::TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<()> {
        self.record_command_activity(observed_at)?;
        log::info!("海龟汤提问已加入正式输入队列: nickname={}", question.player);
        self.push_pending_task(PendingTask::TurtleSoupQuestion {
            question: Box::new(question),
            observed_at,
        })
    }

    pub(super) fn abort_entertainment_for_context_loss(&self, reason: &str) {
        if let Err(error) = self.business.business.abort_turtle_soup(reason) {
            log::error!("无法中止海龟汤会话: {error:#}");
        }
        match self.business.undercover_game.abort() {
            Ok(true) => log::warn!("谁是卧底已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧谁是卧底牌局: {error:#}"),
        }
        match self.business.card_games.abort() {
            Ok(true) => log::warn!("牌局已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧牌局: {error:#}"),
        }
        match self.business.business.abort_idiom_chain() {
            Ok(true) => log::warn!("成语接龙已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧成语接龙会话: {error:#}"),
        }
    }

    fn tick_entertainment(&self) {
        let scheduler_idle = match self.business.task_engine.snapshot() {
            Ok(snapshot) => snapshot.is_idle(),
            Err(error) => {
                log::error!("无法读取业务调度状态，娱乐计时保持暂停: {error}");
                false
            }
        };
        let clock_active = !self.lifecycle.paused.load(AtomicOrdering::SeqCst) && scheduler_idle;
        if let Err(error) = self
            .business
            .business
            .refresh_turtle_soup_deadline(Instant::now(), clock_active)
        {
            log::error!("无法同步海龟汤期限: {error:#}");
        }
        let card_game_outcome = match self
            .business
            .card_games
            .poll_timed_outcome(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进牌局回合计时: {error:#}");
                None
            }
        };
        if let Some(outcome) = card_game_outcome {
            let key = outcome.key();
            let effect = self.business.card_games.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::CardGameEffect(effect)) {
                log::error!("牌局计时结果入队失败: {error:#}");
                if let Err(cancel_error) = self.business.card_games.cancel_effect(key) {
                    log::error!("牌局计时结果入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
        let undercover_outcome = match self
            .business
            .undercover_game
            .poll_timed_outcome(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进谁是卧底计时: {error:#}");
                None
            }
        };
        if let Some(outcome) = undercover_outcome {
            let key = outcome.key();
            let effect = self.business.undercover_game.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::UndercoverEffect(effect)) {
                log::error!("谁是卧底计时消息入队失败: {error:#}");
                if let Err(cancel_error) = self.business.undercover_game.cancel_effect(key) {
                    log::error!("谁是卧底计时消息入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
    }

    /// 二级/好友命令的派发入口。返回值是**是否真正作为命令派发成功**——只有 true 才允许
    /// 写命令锁；被禁用、邀请去重、未路由都返回 false（只 ack）。
    pub(super) fn submit_secondary_command(&self, parsed: RoutedCommand) -> Result<bool> {
        let parsed = self.map_command_actor(parsed);
        if self.enqueue_chat_listener_command(&parsed)? {
            return Ok(true);
        }
        if self.command_is_disabled(&parsed)? {
            // 同上一级：区分「全局开关」与「该命令权限不够」，否则会误导排查方向。
            log::info!(
                "命令未执行：二级命令被权限或启用策略拒绝 text={}",
                parsed.raw
            );
            return Ok(false);
        }
        if let ModuleCommand::Invite(invite) = &parsed.command
            && !self.business.business.invite_should_accept(invite.seq)?
        {
            let seq = invite.seq.expect("unsequenced invites are always accepted");
            log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed.raw);
            return Ok(false);
        }
        if self.apply_immediate_administration(&parsed, true)? {
            return Ok(true);
        }
        self.enqueue_pending_command(PendingCommand {
            lock_key: command::lock_key(&parsed),
            routed: parsed,
        })?;
        Ok(true)
    }

    fn enqueue_pending_command(&self, pending: PendingCommand) -> Result<()> {
        // 可信管理员拉黑先接管待决租约，再进入正式队列；旧结果不能通过去重阻塞它。
        if self.enqueue_direct_blacklist(&pending.routed)? {
            self.record_command_activity(command_observed_at(&pending.routed))?;
            return Ok(());
        }
        if self.pending_contains_command(&pending.routed)? {
            log::info!("命令已在待处理队列，本轮跳过: {}", pending.routed.raw);
            return Ok(());
        }
        self.record_command_activity(command_observed_at(&pending.routed))?;
        log::info!("命令已加入待处理队列: {}", pending.routed.raw);
        self.push_pending_task(PendingTask::Command(Box::new(pending)))
    }

    fn apply_immediate_administration(
        &self,
        parsed: &RoutedCommand,
        propagate_log_error: bool,
    ) -> Result<bool> {
        if parsed.permission_required.is_some() {
            return Ok(false);
        }
        let ModuleCommand::Administration(command) = &parsed.command else {
            return Ok(false);
        };
        let context = AdministrationCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let mut port = self.immediate_administration_port();
        let handled = matches!(
            self.business.administration_application.apply_immediate(
                &context,
                command,
                propagate_log_error,
                &mut port,
            )?,
            ImmediateAdministrationOutcome::Handled
        );
        if handled {
            crate::debug_read_only::record_command_immediate(
                parsed,
                "immediateAdministration",
                "immediate administration completed".to_string(),
            );
        }
        Ok(handled)
    }

    pub(super) fn execute_console_chat_task(&mut self, text: String, prefix: String) -> Result<()> {
        let message = format!("{}{}", prefix, text);
        self.reply(&message)
    }

    /// 值守切换自愈：pending_mode 有值但正式队列里没有对应任务时补入队。
    ///
    /// 正常情况下 request + enqueue 由同一个生产者完成；但只要中途入队失败、
    /// 队列被撤销，或某个调用点只 request 不入队，pending_mode 就会永久悬挂：
    /// 目标模式切不过去，且 `pending_mode.is_some()` 会挡住后续所有驻留恢复，
    /// 表现为「卡在一级值守，怎么都进不了二级」。
    ///
    /// 去重键保证这里重复调用是安全的：队列里已有同模式任务时直接返回。
    /// 外部聊天源启用时，第一次真正停在一级聊天界面就切到二级。
    ///
    /// 只做一次：用 `claim_initial_secondary_switch` 原子认领，之后用户手动
    /// 切回一级不会再被顶回二级。
    ///
    /// 准入条件除了「一级界面」，还要求外部源已接管、当前不是二级、没有正在
    /// 进行的切换，避免与启动自动化抢 UI 事务。
    fn maybe_switch_to_secondary(&self, ui_state: &str) -> Result<()> {
        if !self.lifecycle.external_chat.supplies_hall_messages() {
            return Ok(());
        }
        let snapshot = self.business.business.chat_listener_snapshot()?;
        if snapshot.mode == ChatListenerMode::Secondary || snapshot.pending_mode.is_some() {
            return Ok(());
        }
        if !self
            .lifecycle
            .external_chat
            .claim_initial_secondary_switch()
        {
            return Ok(());
        }
        log::info!("外部聊天源首次遇到一级聊天界面，自动切换二级值守: ui={ui_state}");
        if let Err(error) = self
            .business
            .business
            .request_chat_listener_mode(ChatListenerMode::Secondary)
        {
            log::warn!("外部聊天源默认切换二级失败（保持一级）: {error}");
        }
        Ok(())
    }
    fn reconcile_pending_listener_mode(&self) -> Result<()> {
        let snapshot = self.business.business.chat_listener_snapshot()?;
        let Some(target) = snapshot.pending_mode else {
            return Ok(());
        };
        if snapshot.mode == target {
            // 已经切到了目标模式，pending 只是残留：直接收尾。
            self.business.business.complete_chat_listener_mode(target)?;
            log::info!("值守模式已是{}，清理残留的切换请求", target.label());
            return Ok(());
        }
        let key = crate::runtime::scheduler::FormalTaskDedupKey::new(format!(
            "chat_listener_mode_{}",
            target.label()
        ));
        if self.business.task_engine.contains_formal_dedup_key(&key)? {
            return Ok(());
        }
        log::warn!(
            "值守切换请求没有对应任务（pending={}），补入队",
            target.label()
        );
        if let Err(error) = self.push_pending_task(PendingTask::SetChatListenerMode { target }) {
            // 入队失败就撤销请求，避免下个周期继续空转。
            if let Err(cancel_error) = self
                .business
                .business
                .cancel_chat_listener_mode_request(target)
            {
                log::error!("撤销值守切换请求失败: {cancel_error}");
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn execute_set_chat_listener_mode(
        &mut self,
        target: ChatListenerMode,
    ) -> Result<()> {
        self.abort_entertainment_for_context_loss("聊天值守模式即将切换");
        let residency = match target {
            ChatListenerMode::Primary => UiResidency::Primary,
            ChatListenerMode::Secondary => UiResidency::SecondaryCurrentHall,
        };
        if self
            .establish_ui_residency(residency, ResidencyPurpose::ListenerModeSwitch)
            .is_ok()
        {
            self.complete_chat_listener_mode(target)?;
            log::info!("聊天值守模式已切换为{}", target.label());
            return Ok(());
        }

        self.fail_chat_listener_mode_to_primary()?;
        let _ = self.establish_ui_residency(
            UiResidency::Primary,
            ResidencyPurpose::IndependentRecovery("值守切换失败回退一级"),
        );
        Err(anyhow!("切换{}失败，已回退一级值守", target.label()))
    }
}

#[cfg(test)]
mod tests {
    /// 回归：AI 聊天的触发关键词必须作用在「去掉前导分隔符」的正文上。
    ///
    /// split_sender_line 只切在分隔符**之前**，body 仍带着那个分隔符：
    ///   [2#BOT|勿加]:橘子你是什么模型？  ->  (2#BOT|勿加, :橘子你是什么模型？)
    /// 命令路径会先 trim 掉 : / ： 再解析，AI 聊天分支原来没做，
    /// 于是 Prefix 模式下 strip_prefix(橘子) 永远失败——表现为
    /// 「AI 聊天完全无法触发」，而日志里只看到「命令未执行」。
    #[test]
    fn ai_chat_trigger_text_drops_the_sender_separator() {
        use crate::interfaces::chat::split_sender_line;
        let raw = "[2#BOT|勿加]:橘子你是什么模型？";
        let (sender, body) = split_sender_line(raw, "blue").expect("bracket sender");
        assert_eq!(sender, "2#BOT|勿加");
        // split_sender_line 只切在分隔符**之前**，body 仍带着冒号。
        assert_eq!(body, ":橘子你是什么模型？");
        let mut config = crate::features::ai_chat::AiChatConfig::default();
        config.keyword = "橘子".to_string();
        // 归一化下沉到 match_trigger 内部：命令路径与闲聊路径看到同一段正文。
        // 因此带冒号的原始 body 也必须命中，正文是「你是什么模型？」。
        assert_eq!(
            config.match_trigger(body).as_deref(),
            Some("你是什么模型？"),
            "带前导分隔符的 body 必须命中：归一化是 match_trigger 的职责"
        );
        // 已经 trim 过的正文结果一致（幂等）。
        let trimmed = body.trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
        assert_eq!(
            config.match_trigger(trimmed).as_deref(),
            Some("你是什么模型？")
        );
    }

    /// 回归（端到端）：真实 OCR 文本「[昵称]:橘子xxx」必须能一路命中触发。
    ///
    /// 覆盖从聊天行到判定结果的全部真实环节：split_sender_line 切昵称、
    /// match_trigger 去分隔符并剥关键词、keyword_trigger 过全部门槛。
    /// 这条路径以前因为 body 带 ':' 而永远返回 None，玩家看到的就是 AI 完全不回。
    #[test]
    fn bracketed_hall_line_with_keyword_triggers_end_to_end() {
        use crate::features::ai_chat::{
            AiChatConfig, KeywordTrigger, TriggerSkip, keyword_trigger,
        };
        use crate::interfaces::chat::split_sender_line;

        let config = AiChatConfig {
            enabled: true,
            keyword: "橘子".to_string(),
            ..AiChatConfig::default()
        };

        // 玩家发的那句话：昵称在方括号里，正文前有半角冒号。
        let line = "[路人甲]:橘子你是什么模型？";
        let split = split_sender_line(line, "blue");
        assert_eq!(
            keyword_trigger(&config, true, "blue", split),
            KeywordTrigger::Ask {
                sender: "路人甲".to_string(),
                // 正文原样交给模型，关键词保留在句首。
                question: "橘子你是什么模型？".to_string(),
            }
        );

        // 全角昵称括号 + 冒号（OCR 两种字形都可能出现）。
        let line = "【路人甲】:橘子在吗";
        let split = split_sender_line(line, "blue");
        assert_eq!(
            keyword_trigger(&config, true, "blue", split),
            KeywordTrigger::Ask {
                sender: "路人甲".to_string(),
                question: "橘子在吗".to_string(),
            }
        );

        // 一条命令式的行不会被关键词吞掉：它不带关键词，正常落回关键词未命中。
        let split = split_sender_line("[路人甲]:@点歌晴天", "blue");
        assert_eq!(
            keyword_trigger(&config, true, "blue", split),
            KeywordTrigger::Skip(TriggerSkip::KeywordMiss)
        );
    }

    /// 监控页"界面"只显示中文界面名，不复述命中的颜色/模板判据；
    /// 判据细节仍保留在 /debug/ui-state 与 ui.state 事件证据里。
    #[test]
    fn ui_state_display_label_maps_state_families_without_probe_details() {
        assert_eq!(
            super::ui_state_display_label("primary:marker blue=1 yellow=0 pink=0"),
            "一级大厅"
        );
        assert_eq!(super::ui_state_display_label("primary:friend"), "一级大厅");
        assert_eq!(super::ui_state_display_label("secondary:chat"), "二级界面");
        assert_eq!(
            super::ui_state_display_label("secondary:qianxing_map"),
            "二级界面"
        );
        assert_eq!(
            super::ui_state_display_label("secondary:qianxing_panel"),
            "二级界面"
        );
        assert_eq!(super::ui_state_display_label("main_menu"), "千星主菜单");
        assert_eq!(super::ui_state_display_label("game_gate"), "大门界面");
        assert_eq!(super::ui_state_display_label("overworld"), "主世界");
        assert_eq!(
            super::ui_state_display_label("overworld:world_criterion"),
            "主世界"
        );
        assert_eq!(super::ui_state_display_label("loading"), "加载界面");
        assert_eq!(
            super::ui_state_display_label("panel:friend_list"),
            "模态面板"
        );
        assert_eq!(super::ui_state_display_label("unknown"), "未知界面");
        assert_eq!(
            super::ui_state_display_label("unknown:game_probe"),
            "未知界面"
        );
        assert_eq!(
            super::ui_state_display_label("transition:overworld 1/2"),
            "界面切换中"
        );
        assert_eq!(
            super::ui_state_display_label("transition:unknown"),
            "界面切换中"
        );
        // 未识别的新标签原样透出，不静默吞掉。
        assert_eq!(
            super::ui_state_display_label("something_new"),
            "something_new"
        );
    }

    /// 界面状态迁移只在**稳定类型**变化时推送。
    /// 标签里还带着聊天标记计数，而计数每帧都在变——跟着它推等于自造轮询。
    #[test]
    fn ui_state_transition_only_fires_on_stable_kind_change() {
        use crate::runtime::ui::UiStateKind;
        // 第一次调用的返回值取决于全局起点，这里只断言**重复**行为。
        let _ = super::take_ui_state_transition(Some(UiStateKind::Secondary));
        assert!(
            !super::take_ui_state_transition(Some(UiStateKind::Secondary)),
            "同一稳定类型重复观测不得再产生事件"
        );
        assert!(
            super::take_ui_state_transition(None),
            "稳定 -> 过渡 是一条真实迁移"
        );
        assert!(
            !super::take_ui_state_transition(None),
            "过渡态内部不断抖动时不得重复推送"
        );
        assert!(
            super::take_ui_state_transition(Some(UiStateKind::Primary)),
            "过渡 -> 另一个稳定类型 又是一条迁移"
        );
        assert!(!super::take_ui_state_transition(Some(UiStateKind::Primary)));
        assert!(super::take_ui_state_transition(Some(
            UiStateKind::Overworld
        )));
    }

    /// OCR 抖动不得产生新消息。断言来自真机实测加上一个必须正视的事实：
    /// **OCR 对特殊字符会抖动**——半角/全角标点、表情被读成乱码、说话人迟一拍，
    /// 都曾让同一条气泡产生独立事件。
    #[test]
    fn chat_jitter_does_not_produce_new_messages() {
        assert_eq!(
            super::normalize_chat_text("[珐露珊]："),
            super::normalize_chat_text("[珐露珊]:"),
            "半角/全角标点属于排版差异，不是内容变化"
        );
        assert_eq!(
            super::normalize_chat_text("  天子 :  你好  "),
            "天子:你好",
            "空白一律丢弃：OCR 的空白两个方向都会错，保留它只会让比较更不稳定"
        );
        assert_eq!(
            super::normalize_chat_text("[天子]:@女(禁言7天) 标题党是台好车"),
            "[天子]:@女(禁言7天)标题党是台好车",
            "实测中真实存在的空格也要丢弃：同一位置 OCR 时有时无"
        );

        let key = "jitter-probe|primary_hall|9001";
        assert!(super::take_bubble_update(key), "第一次看到这条气泡必须推送");
        // 同一条气泡的**任何**再次识别都不得产生新事件，包括长短变化的抖动。
        for jitter in [
            "[珐露珊]:",
            "[珐露珊]：",
            "[珐露珊]::",
            "[]:这在干啥？",
            "[天子]:这在干啥？",
            "[]:这在干啥?",
            "🙂",
            "",
        ] {
            assert!(
                !super::take_bubble_update(key),
                "同一条气泡的再次识别不得产生新事件: {jitter:?}"
            );
        }
        assert!(
            super::take_bubble_update("jitter-probe|primary_hall|9002"),
            "另一条气泡必须照常推送"
        );
    }

    use crate::features::command::{
        CommandAuthority, CommandEnvelope, CommandObservation, ModuleCommand, RoutedCommand,
    };
    use crate::features::custom_workflow::{
        CustomWorkflowConfig, CustomWorkflowService, WorkflowDefaults,
    };
    use crate::features::identity::{
        IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
    };
    use crate::features::playback::PlaybackCommand;
    use crate::interfaces::chat::ChatCommandRouter;

    use super::{
        CHAT_LISTENER_UNRESOLVED_UI_GRACE, CONFIG_RELOAD_UI_FALLBACK_GRACE, ChatListenerMode,
        ListenerReadyEvidence, hall_consumes_scope, listener_mode_matches_ui,
        listener_ready_after_recheck, load_persisted_chat_listener_mode, lock_anchor_for,
        persist_chat_listener_mode, primary_rescan_can_publish, replacement_ready_allowed,
        replacement_startup_fallback_due, replacement_startup_retry_due, scan_frame_is_fresh,
        secondary_hall_recovery_allowed, unresolved_ui_recovery_due,
    };
    use crate::geometry::Rect;
    use crate::observation::chat::{
        BubbleSequence, ChatIdentity, ChatMessage, ChatScope, ChatTimeline, ObservedChatMessageId,
        PrimaryObservedMessage, RecognizedChatMessage, SourceId, VisualSessionId,
    };
    use std::time::{Duration, Instant};

    fn listener_snapshot(
        mode: ChatListenerMode,
    ) -> crate::runtime::chat_listener::ChatListenerSnapshot {
        crate::runtime::chat_listener::ChatListenerSnapshot {
            mode,
            pending_mode: None,
            temporary_primary: false,
            initial_unread_clear: false,
            unread_task_pending: false,
            hall_round_required: false,
        }
    }

    #[test]
    fn automatic_entry_requires_the_matching_stable_location_and_switch() {
        use crate::config::AppConfig;
        use crate::features::startup::StartupTaskKind;
        use crate::runtime::ui::UiStateKind;

        let mut config = AppConfig::default();
        config.startup.enabled = true;
        config.startup.enter_game = true;
        config.startup.enter_wonderland = false;
        assert_eq!(
            super::automatic_startup_task(&config, UiStateKind::GameGate)
                .unwrap()
                .kind(),
            StartupTaskKind::StartGame,
        );
        assert!(super::automatic_startup_task(&config, UiStateKind::Overworld).is_none());
        config.startup.enter_wonderland = true;
        assert_eq!(
            super::automatic_startup_task(&config, UiStateKind::Overworld)
                .unwrap()
                .kind(),
            StartupTaskKind::EnterWonderland,
        );
        for kind in [
            UiStateKind::Primary,
            UiStateKind::Secondary,
            UiStateKind::Unknown,
        ] {
            assert!(super::automatic_startup_task(&config, kind).is_none());
        }
        config.startup.enabled = false;
        assert!(super::automatic_startup_task(&config, UiStateKind::GameGate).is_none());
        assert!(super::automatic_startup_task(&config, UiStateKind::Overworld).is_none());
    }

    #[test]
    fn observing_a_game_location_can_finish_reload_without_automatic_entry() {
        let state = listener_snapshot(ChatListenerMode::Secondary);
        assert!(listener_ready_after_recheck(
            Some(ListenerReadyEvidence::GameLocation),
            &state,
            false,
            false,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::GameLocation),
            &state,
            true,
            false,
            true,
        ));
    }

    #[test]
    fn game_location_confirmation_prevents_generic_residency_recovery() {
        use crate::runtime::ui::{UiStateClassification, UiStateKind, UiStateTracker};
        let mut tracker = UiStateTracker::new(2);
        for (frame, kind, label) in [
            (1, UiStateKind::Unknown, "unknown:game_probe"),
            (2, UiStateKind::GameGate, "game_gate"),
            (3, UiStateKind::Overworld, "overworld"),
        ] {
            let observation = tracker.observe(frame, UiStateClassification::new(kind, label));
            assert!(super::game_location_probe_blocks_residency(Some(
                &observation
            )));
        }
        let unknown = tracker.observe(
            4,
            UiStateClassification::new(UiStateKind::Unknown, "unknown"),
        );
        assert!(!super::game_location_probe_blocks_residency(Some(&unknown)));
    }

    #[test]
    fn listener_mode_dispatch_preserves_router_permissions() {
        let identity = IdentityAccess::new(IdentityConfig {
            mappings: vec![IdentityMapping {
                nickname: "管理员".to_string(),
                id: uuid::Uuid::from_u128(1),
                role: IdentityRole::Admin,
                note: String::new(),
                accounts: Vec::new(),
            }],
        });
        let workflows = CustomWorkflowService::new(
            CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let router = ChatCommandRouter::with_identity(&workflows, &identity);
        for (username, message_type, permitted) in [
            ("访客", "blue", false),
            ("管理员", "blue", true),
            ("访客", "pink", true),
        ] {
            for (text, expected) in [
                ("@值守模式 一级", super::ChatListenerModeCommand::Primary),
                ("@值守模式 二级", super::ChatListenerModeCommand::Secondary),
                ("@值守模式 状态", super::ChatListenerModeCommand::Status),
            ] {
                let envelope = CommandEnvelope::new(
                    format!("{username}：{text}"),
                    username,
                    message_type,
                    text,
                    CommandObservation::default(),
                )
                .unwrap();
                let routed = router.route(&envelope, None).expect("listener command");
                assert_eq!(routed.permission_required.is_none(), permitted);
                assert_eq!(
                    super::chat_listener_mode_dispatch(&routed),
                    permitted.then_some(expected),
                    "username={username} source={message_type} command={text}"
                );
            }
        }
    }

    #[test]
    fn chat_listener_mode_persistence_round_trips_without_using_the_database_file() {
        let root = std::env::temp_dir().join(format!(
            "miliastra-chat-listener-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let playback_state_path = root.join("playback.sqlite3");

        persist_chat_listener_mode(&playback_state_path, ChatListenerMode::Secondary).unwrap();
        assert_eq!(
            load_persisted_chat_listener_mode(&playback_state_path).unwrap(),
            Some(ChatListenerMode::Secondary)
        );
        assert!(
            root.join("playback.sqlite3.chat-listener-mode.json")
                .is_file()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listener_ready_requires_the_selected_logical_mode_to_match_the_ui() {
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Primary,
            false,
            crate::runtime::ui::UiStateKind::Primary
        ));
        assert!(!listener_mode_matches_ui(
            ChatListenerMode::Primary,
            false,
            crate::runtime::ui::UiStateKind::Secondary
        ));
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            false,
            crate::runtime::ui::UiStateKind::Secondary
        ));
        assert!(!listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            false,
            crate::runtime::ui::UiStateKind::Primary
        ));
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            true,
            crate::runtime::ui::UiStateKind::Primary
        ));
    }

    #[test]
    fn scan_freshness_rejects_pre_boundary_frames_without_discarding_stable_frames() {
        let boundary = Instant::now();
        let old = boundary.checked_sub(Duration::from_millis(1)).unwrap();
        let fresh = boundary + Duration::from_millis(1);
        assert!(!scan_frame_is_fresh(old, Some(boundary)));
        assert!(scan_frame_is_fresh(boundary, Some(boundary)));
        assert!(scan_frame_is_fresh(fresh, Some(boundary)));
        assert!(scan_frame_is_fresh(old, None));
        assert!(!scan_frame_is_fresh(
            fresh,
            Some(fresh + Duration::from_millis(1))
        ));
    }

    #[test]
    fn primary_rescan_rejects_a_fresh_frame_that_left_primary_ui() {
        assert!(primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Primary
        )));
        assert!(!primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Secondary
        )));
        assert!(!primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Unknown
        )));
        assert!(!primary_rescan_can_publish(None));
    }

    #[test]
    fn listener_ready_requires_fresh_verified_work_and_a_final_idle_recheck() {
        let primary = listener_snapshot(ChatListenerMode::Primary);
        assert!(listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            false,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            None, &primary, false, false, true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            true,
            false,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            true,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn replacement_ready_waits_for_required_startup_success() {
        assert!(replacement_ready_allowed(true, true));
        assert!(!replacement_ready_allowed(true, false));
        assert!(!replacement_ready_allowed(false, true));
    }

    #[test]
    fn secondary_ready_requires_a_completed_current_hall_round() {
        let mut secondary = listener_snapshot(ChatListenerMode::Secondary);
        assert!(listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));

        secondary.initial_unread_clear = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
        secondary.initial_unread_clear = false;
        secondary.hall_round_required = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
        secondary.hall_round_required = false;
        secondary.unread_task_pending = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
    }

    #[test]
    fn unresolved_ui_recovery_waits_for_grace_and_runtime_idle() {
        let now = Instant::now();
        let old = now.checked_sub(CHAT_LISTENER_UNRESOLVED_UI_GRACE).unwrap();
        assert!(unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(now),
            now,
            now,
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now + Duration::from_millis(1),
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            true,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            false,
            true,
        ));
    }

    #[test]
    fn secondary_hall_recovery_uses_only_its_cooldown() {
        let now = Instant::now();
        assert!(secondary_hall_recovery_allowed(now, now));
        assert!(!secondary_hall_recovery_allowed(
            now,
            now + Duration::from_secs(1),
        ));
    }

    #[test]
    fn primary_listening_does_not_consume_secondary_hall_observations() {
        // 一级值守只看主大厅的**屏幕桶**。二级大厅与主观察可能是同一个大厅，
        // 同屏可见的二级几何是同一个大厅的另一个身份；消费它就会重复派发同一条命令。
        let ocr = SourceId::ocr();
        assert!(hall_consumes_scope(
            ChatListenerMode::Primary,
            &ChatScope::hall(),
            &ocr,
            Some(&ChatIdentity::PrimaryHall)
        ));
        assert!(!hall_consumes_scope(
            ChatListenerMode::Primary,
            &ChatScope::hall(),
            &ocr,
            Some(&ChatIdentity::SecondaryHall)
        ));
        // 外部源借用的 SecondaryHall 桶没有屏幕语义：一级值守也必须消费，
        // 否则用户切到一级后外部大厅命令会被整批丢弃。
        assert!(hall_consumes_scope(
            ChatListenerMode::Primary,
            &ChatScope::hall_with_id(7),
            &SourceId::external("chatbridge"),
            Some(&ChatIdentity::SecondaryHall)
        ));
        // 私聊与值守模式无关。
        assert!(hall_consumes_scope(
            ChatListenerMode::Primary,
            &ChatScope::private("Alice"),
            &ocr,
            Some(&ChatIdentity::Friend("Alice".into()))
        ));
    }

    #[test]
    fn secondary_listening_does_not_consume_primary_hall_commands() {
        // 二级值守只看二级大厅的屏幕桶。主大厅快照（含审核巡检临时一级驻留期间的帧）
        // 不在它的消费范围内，命令派发留给一级值守。
        let ocr = SourceId::ocr();
        assert!(hall_consumes_scope(
            ChatListenerMode::Secondary,
            &ChatScope::hall(),
            &ocr,
            Some(&ChatIdentity::SecondaryHall)
        ));
        assert!(!hall_consumes_scope(
            ChatListenerMode::Secondary,
            &ChatScope::hall(),
            &ocr,
            Some(&ChatIdentity::PrimaryHall)
        ));
    }

    #[test]
    fn hall_gate_leaves_friend_and_unknown_identities_untouched() {
        // 好友未读、未知、公共频道与陌生人聊天都不是大厅，两种值守模式下语义都不变。
        for mode in [ChatListenerMode::Primary, ChatListenerMode::Secondary] {
            for chat in [
                ChatIdentity::Friend("最好的京子".into()),
                ChatIdentity::Unknown,
                ChatIdentity::PublicChannel,
                ChatIdentity::StrangerMessages,
            ] {
                assert!(
                    hall_consumes_scope(
                        mode,
                        &ChatScope::from_identity(&chat),
                        &SourceId::ocr(),
                        Some(&chat),
                    ),
                    "{} 不应改变 {chat:?} 的消费语义",
                    mode.label()
                );
            }
        }
    }

    #[test]
    fn replacement_startup_retry_is_limited_to_idle_pre_ready_children() {
        let now = Instant::now();
        assert!(replacement_startup_retry_due(
            true, false, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            false, false, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true, true, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true, false, false, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true,
            false,
            true,
            true,
            now,
            now + Duration::from_millis(1),
        ));
    }

    #[test]
    fn replacement_ui_fallback_waits_for_a_stable_pre_ready_failure() {
        let now = Instant::now();
        let old = now.checked_sub(CONFIG_RELOAD_UI_FALLBACK_GRACE).unwrap();
        assert!(replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            false,
            now,
            Some(now),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            true,
            true,
            true,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            false,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            true,
            now,
            Some(old),
        ));
    }

    #[test]
    fn missing_chat_listener_mode_defaults_to_none() {
        let path = std::env::temp_dir()
            .join(format!(
                "miliastra-chat-listener-missing-{}",
                uuid::Uuid::new_v4()
            ))
            .join("playback.sqlite3");
        assert_eq!(load_persisted_chat_listener_mode(&path).unwrap(), None);
    }

    fn observed_message(sequence: u64, text: &str, is_new: bool) -> PrimaryObservedMessage {
        PrimaryObservedMessage {
            id: ObservedChatMessageId::new(
                VisualSessionId::new(1),
                ChatIdentity::PrimaryHall,
                BubbleSequence::new(sequence),
            ),
            message: ChatMessage {
                message_type: "blue".to_string(),
                block: Rect::new(0, sequence as i32 * 20, 10, 10),
                text: text.to_string(),
                sender: None,
            },
            is_new,
        }
    }

    // ==== 命令锁接线测试（真实格式 "[昵称]:@命令"）====

    fn chat_input(text: &str, y: i32) -> RecognizedChatMessage {
        RecognizedChatMessage {
            text: text.into(),
            sender: None,
            raw_sender: None,
            message_type: "blue".into(),
            block: Rect::new(0, y, 100, 10),
        }
    }

    fn primary_route(message_id: ObservedChatMessageId) -> RoutedCommand {
        let anchor = lock_anchor_for(&message_id, 40);
        RoutedCommand {
            matched: "状态".into(),
            raw: "状态".into(),
            user_command: "@状态".into(),
            message_type: "blue".into(),
            username: "甲".into(),
            account: None,
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            command: ModuleCommand::Playback(PlaybackCommand::Status),
            observation: CommandObservation {
                frame_id: None,
                captured_at: Some(Instant::now()),
                message_id: Some(message_id),
                message_text: Some("[甲]:@状态".to_string()),
                lock_anchor: Some(anchor),
            },
        }
    }

    #[test]
    fn blue_unrecognized_revision_routes_once_including_startup() {
        for baseline_primed in [false, true] {
            let shared = ChatTimeline::new();
            let first = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![chat_input("[甲]:@点哥 晴天", 40)],
                )
                .unwrap();
            let message = &first.messages[0];
            let observed = PrimaryObservedMessage {
                id: message.id.clone(),
                is_new: true,
                message: ChatMessage {
                    text: message.text.clone(),
                    sender: message.sender.clone(),
                    message_type: "blue".into(),
                    block: message.block,
                },
            };
            let envelope = crate::interfaces::chat::parse_command_envelope(
                &message.text,
                "blue",
                None,
                CommandObservation::default(),
            )
            .unwrap();
            assert!(
                ChatCommandRouter::without_custom_workflow()
                    .route(&envelope, None)
                    .is_none()
            );
            super::acknowledge_unrecognized_primary_message(
                &shared,
                &observed,
                first.captured_at,
                baseline_primed,
            )
            .unwrap();
            let corrected = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![chat_input("[甲]:@点歌 晴天", 40)],
                )
                .unwrap();
            let message = &corrected.messages[0];
            assert_eq!(message.id, observed.id);
            assert_eq!(
                shared
                    .is_business_pending_at(&message.id, corrected.captured_at)
                    .unwrap(),
                true
            );
            {
                let envelope = crate::interfaces::chat::parse_command_envelope(
                    &message.text,
                    "blue",
                    message.sender.as_deref(),
                    CommandObservation {
                        message_id: Some(message.id.clone()),
                        message_text: Some(message.text.clone()),
                        lock_anchor: Some(lock_anchor_for(&message.id, message.block.y)),
                        ..CommandObservation::default()
                    },
                )
                .unwrap();
                let parsed = ChatCommandRouter::without_custom_workflow()
                    .route(&envelope, None)
                    .unwrap();
                super::acknowledge_primary_command(&shared, &parsed, true).unwrap();
                let repeated = shared
                    .publish(
                        Instant::now(),
                        ChatIdentity::PrimaryHall,
                        vec![chat_input("[甲]:@点歌 晴天", 40)],
                    )
                    .unwrap();
                assert!(
                    !shared
                        .is_business_pending_at(&message.id, repeated.captured_at)
                        .unwrap()
                );
            }
        }
    }

    /// 一级 write_lock 分支：真正派发成功写锁；被禁用只 ack 不写锁。
    #[test]
    fn primary_write_lock_only_after_real_dispatch() {
        let shared = ChatTimeline::new();
        let frame = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![chat_input("[甲]:@状态", 40)],
            )
            .unwrap();
        let message = frame.messages[0].clone();
        let command = primary_route(message.id.clone());
        super::acknowledge_primary_command(&shared, &command, true).unwrap();
        assert_eq!(
            shared.command_lock_texts_for_test(&ChatIdentity::PrimaryHall),
            vec!["[甲]:@状态".to_string()],
            "真正派发成功必须写锁"
        );
        assert!(!shared.is_business_pending(&message.id).unwrap());

        let shared = ChatTimeline::new();
        let frame = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![chat_input("[甲]:@状态", 40)],
            )
            .unwrap();
        let message = frame.messages[0].clone();
        let command = primary_route(message.id.clone());
        super::acknowledge_primary_command(&shared, &command, false).unwrap();
        assert!(
            !shared.command_lock_bucket_exists_for_test(&ChatIdentity::PrimaryHall),
            "被禁用命令只 ack 不写锁"
        );
        assert!(!shared.is_business_pending(&message.id).unwrap());
    }

    /// 缺陷 6：缺文本或缺 y 时不得用 unwrap_or_default 退化成空串/0（0 会命中任何
    /// y<=28 的同文本气泡）；必须 warn 并只确认不写锁。
    #[test]
    fn primary_write_lock_skips_missing_text_or_y() {
        for (text, y) in [(None, Some(40)), (Some("[甲]:@状态"), None)] {
            let shared = ChatTimeline::new();
            let frame = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![chat_input("[甲]:@状态", 40)],
                )
                .unwrap();
            let message = frame.messages[0].clone();
            let mut command = primary_route(message.id.clone());
            command.observation.message_text = text.map(str::to_string);
            command.observation.lock_anchor = y.map(|y| lock_anchor_for(&message.id, y));
            super::acknowledge_primary_command(&shared, &command, true).unwrap();
            assert!(
                !shared.command_lock_bucket_exists_for_test(&ChatIdentity::PrimaryHall),
                "缺失 text/y 不得写锁 (text={text:?}, y={y:?})"
            );
            assert!(
                !shared.is_business_pending(&message.id).unwrap(),
                "缺失时仍应确认，避免消息卡在 pending"
            );
        }
    }

    #[test]
    fn blue_startup_is_candidate_and_success_lock_survives_view_rebuild() {
        let shared = ChatTimeline::new();
        let mut input = chat_input("[甲]:@状态", 40);
        input.message_type = "blue".into();
        let frame = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![input.clone()],
            )
            .unwrap();
        let message = &frame.messages[0];
        let observed = PrimaryObservedMessage {
            id: message.id.clone(),
            is_new: message.is_new,
            message: ChatMessage {
                text: message.text.clone(),
                sender: message.sender.clone(),
                message_type: "blue".into(),
                block: message.block,
            },
        };
        assert!(observed.is_new, "blue startup must enter command handling");
        assert!(shared.is_business_pending(&message.id).unwrap());
        assert!(
            shared
                .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
                .is_empty(),
            "capture alone cannot write a success lock"
        );
        let command = primary_route(message.id.clone());
        super::acknowledge_primary_command(&shared, &command, true).unwrap();
        shared
            .publish_no_chat(
                Instant::now(),
                "transient view rebuild",
                crate::observation::chat::ViewAbsence::Transient,
            )
            .unwrap();
        let rebuilt = shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![input])
            .unwrap();
        assert!(
            !shared.is_business_pending(&rebuilt.messages[0].id).unwrap(),
            "already successful startup command cannot run again"
        );
    }

    /// 缺陷 1 反例：二级大厅真命令派发成功写锁；随后 5 条普通闲聊被 claim 也不得写锁、
    /// 不得把真命令条目挤出；重建后该命令不得重跑。
    #[test]
    fn secondary_chatter_claims_do_not_evict_dispatched_command_lock() {
        let shared = ChatTimeline::new();
        let chat = ChatIdentity::SecondaryHall;
        let mut inputs = vec![chat_input("[甲]:@点歌 起风了", 100)];
        let first = shared
            .publish(Instant::now(), chat.clone(), inputs.clone())
            .unwrap();
        let command = first.messages[0].clone();
        let claim = shared
            .claim_business_at(
                &command.id,
                first.captured_at,
                &command.text,
                command.lock_anchor(),
            )
            .unwrap()
            .expect("secondary command should be claimable");
        super::complete_secondary_claim(claim, true).unwrap();
        assert_eq!(
            shared.command_lock_texts_for_test(&chat),
            vec!["[甲]:@点歌起风了".to_string()],
            "真正派发成功的二级命令必须写锁"
        );

        for index in 0..5 {
            inputs.push(chat_input(
                &format!("[路人{index}]:今天天气不错"),
                135 + index as i32 * 35,
            ));
            let frame = shared
                .publish(Instant::now(), chat.clone(), inputs.clone())
                .unwrap();
            let chatter = frame.messages.last().unwrap().clone();
            assert!(shared.is_business_pending(&chatter.id).unwrap());
            let claim = shared
                .claim_business_at(
                    &chatter.id,
                    frame.captured_at,
                    &chatter.text,
                    chatter.lock_anchor(),
                )
                .unwrap()
                .expect("chatter should be claimable");
            super::complete_secondary_claim(claim, false).unwrap();
        }
        assert_eq!(
            shared.command_lock_texts_for_test(&chat),
            vec!["[甲]:@点歌起风了".to_string()],
            "普通闲聊不得写锁，更不得把真命令条目按 FIFO 挤出"
        );

        shared
            .publish_no_chat(
                Instant::now(),
                "view rebuilt",
                crate::observation::chat::ViewAbsence::Transient,
            )
            .unwrap();
        let rebuilt = shared
            .publish(
                Instant::now(),
                chat,
                vec![chat_input("[甲]:@点歌 起风了", 100)],
            )
            .unwrap();
        assert!(
            !rebuilt.messages[0].is_new,
            "重建后已执行的二级命令不得重跑"
        );
        assert!(!shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
    }
}
