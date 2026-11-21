//! 日志标签、事件文本与展示观测去重。
//!
//! 从 `listener.rs` 拆出：这里只回答「人读到什么」——日志里的中文标签、
//! 事件载荷的截断与归一化，以及两条**展示去重**状态：
//!
//! - [`UiTransitionTracker`]：界面状态迁移只在稳定类型变化时推送一次；
//! - [`RememberedBubbleTracker`]：同一气泡只推一次调试事件。
//!
//! 两者原先都是进程级 `static Mutex`，让所有单测共享同一份全局状态、无法并行
//! （换机器跑就偶发失败，也没法验证「第一次」与「第二次」的区别）。现在改成
//! 实例化类型，由主循环在 [`ObservationDedup`] 里持有：谁持有谁负责，
//! 单测之间彻底隔离，可以并行。

use super::super::*;
use crate::observation::chat::ChatIdentity;
use std::collections::VecDeque;

/// 日志里的消息来源：把内部的 blue/pink 换成玩家能理解的说法。
///
/// 直接打印内部枚举会让日志出现代码词，不符合书面陈述句要求。
pub(in crate::composition::application) fn message_source_label(
    message_type: &str,
) -> &'static str {
    match message_type {
        "blue" => "大厅普通成员消息",
        "pink" => "好友消息",
        _ => "其他来源消息",
    }
}

/// 日志里的观察缺口原因：把内部枚举换成业务语言。
pub(in crate::composition::application) fn gap_kind_label(
    kind: crate::observation::shared::ObservationGapKind,
) -> &'static str {
    use crate::observation::shared::ObservationGapKind;
    match kind {
        ObservationGapKind::HistoryEvicted => "观察历史被挤掉",
        ObservationGapKind::ExclusiveSession => "独占会话期间未消费",
    }
}

/// 日志里的身份角色名：`IdentityRole` 不能直接 `{:?}` 打给维护者看。
pub(in crate::composition::application) fn actor_role_label(
    role: crate::features::identity::IdentityRole,
) -> &'static str {
    use crate::features::identity::IdentityRole;
    match role {
        IdentityRole::Friend => "好友",
        IdentityRole::Admin => "管理员",
        IdentityRole::Owner => "主人",
    }
}

/// 日志里的聊天区名称：把内部枚举换成界面语言。
pub(in crate::composition::application) fn chat_identity_label(
    chat: &ChatIdentity,
) -> &'static str {
    match chat {
        ChatIdentity::PrimaryHall => "一级大厅",
        ChatIdentity::SecondaryHall => "二级大厅",
        ChatIdentity::Friend(_) => "好友私聊",
        ChatIdentity::PublicChannel => "公共频道",
        ChatIdentity::StrangerMessages => "陌生人消息",
        ChatIdentity::Unknown => "未识别聊天区",
    }
}

/// 日志里的消息交付状态。
pub(in crate::composition::application) fn delivery_state_label(
    state: crate::observation::chat::BusinessDeliveryState,
) -> &'static str {
    use crate::observation::chat::BusinessDeliveryState;
    match state {
        BusinessDeliveryState::Pending => "仅排队未派发",
        BusinessDeliveryState::Claimed => "已认领",
        BusinessDeliveryState::IgnoredRevision => "等待有效修订",
        BusinessDeliveryState::Fenced => "被交付栅栏挡住",
        BusinessDeliveryState::NotTracked => "未入账",
    }
}

/// 日志里的界面驻留目标名称。
pub(in crate::composition::application) fn residency_target_label(
    target: UiResidency,
) -> &'static str {
    match target {
        UiResidency::Primary => "一级聊天界面",
        UiResidency::SecondaryCurrentHall => "二级当前大厅界面",
    }
}

/// 日志里的聊天正文：先脱敏，再折叠空白与截断。
///
/// 脱敏必须在截断**之前**：歌单导入命令的分享链接可能很长，先截断会把
/// `@导入网易歌单` 前缀留住、把链接截掉一半，但脱敏判定是按整串匹配的，
/// 截断后可能不再命中，反而泄漏了链接片段。
///
/// 二级/歌单窗口路径一直用 `privacy::redacted_chat_text`；一级路径原先只折叠
/// 不脱敏，导致同一句 `@导入QQ歌单<链接>` 在一级会把完整链接写进日志文件。
pub(in crate::composition::application) fn compact_for_log(text: &str) -> String {
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

/// 监控页"界面"状态的中文显示名。
///
/// 只按"当前是什么界面"给结论，不在界面状态里复述命中了什么颜色/哪个模板
/// （这些判据保留在 /debug/ui-state 与 ui.state 事件的证据字段里，供排障核对）。
/// 入参是分类标签（如 `primary:marker blue=1 yellow=0 pink=0`、`panel:friend_list`、
/// `transition:overworld 1/2`），只取冒号前的状态族做映射。
pub(in crate::composition::application) fn ui_state_display_label(label: &str) -> String {
    let family = label.split(':').next().unwrap_or(label);
    let name = match family {
        "primary" => "一级大厅",
        "secondary" => "二级界面",
        "main_menu" => "主菜单千星",
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
pub(in crate::composition::application) const MAX_EVENT_TEXT_CHARS: usize = 512;

pub(in crate::composition::application) fn truncate_for_event(text: &str) -> String {
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
pub(in crate::composition::application) fn normalize_chat_text(text: &str) -> String {
    // 唯一实现放在 crate::text：命令锁也必须复用同一份，否则同/全角抖动会互相打不中。
    crate::text::normalize_chat_text(text)
}

// ── 界面状态迁移 ──────────────────────────────────────────────────────────

/// 界面状态迁移的判定键。
///
/// 只用**稳定类型**做判据：标签里还带着聊天标记计数，而计数每帧都在变，
/// 跟着它推会把事件流变成轮询，正是事件通道要消灭的东西。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::composition::application) enum UiTransitionKey {
    /// 分类尚未稳定。过渡态本身算**一个**状态：
    /// 一直在抖但始终没稳定时，不重复推送。
    Unstable,
    Stable(UiStateKind),
}

impl UiTransitionKey {
    pub(in crate::composition::application) const fn of(kind: Option<UiStateKind>) -> Self {
        match kind {
            Some(kind) => Self::Stable(kind),
            None => Self::Unstable,
        }
    }
}

/// 界面状态迁移跟踪器。
///
/// 原先是一个进程级 `static Mutex<Option<UiTransitionKey>>`：任何单测都会改到
/// 别人的「上一次推送」记忆，于是「同类型重复观测不得再产生事件」这类断言在
/// 全量跑时取决于执行顺序。改成实例后，状态跟着持有者走。
#[derive(Debug, Default)]
pub(in crate::composition::application) struct UiTransitionTracker {
    /// 上一次已推送的界面状态键；`None` 表示「还没推送过任何东西」。
    last: Option<UiTransitionKey>,
}

impl UiTransitionTracker {
    /// 这次观测是否构成一次**界面状态迁移**；返回 true 表示应当推送。
    pub(in crate::composition::application) fn take_transition(
        &mut self,
        kind: Option<UiStateKind>,
    ) -> bool {
        let key = UiTransitionKey::of(kind);
        if self.last == Some(key) {
            return false;
        }
        self.last = Some(key);
        true
    }
}

// ── 调试事件的气泡去重 ────────────────────────────────────────────────────

/// 去重记忆的条数上限。丢掉最旧的只影响去重广度，不会让状态变得不正确。
pub(in crate::composition::application) const REMEMBERED_BUBBLE_LIMIT: usize = 256;

/// 已推送过的气泡身份（有界）。
///
/// 用**气泡身份**而不是文本相等判断「是不是同一条消息」：同一条消息被 OCR
/// 二次识别时，说话人可能迟到、标点可能全角半角、表情可能被读成乱码，
/// 这些都不是一条新消息。
#[derive(Debug, Default)]
pub(in crate::composition::application) struct RememberedBubbleTracker {
    seen: VecDeque<String>,
}

impl RememberedBubbleTracker {
    /// 决定一条气泡是否应当出现在事件里：**每条气泡只推一次**。
    ///
    /// 为什么不做事后补正：OCR 对特殊字符的抖动（半角/全角标点、表情被读成
    /// 乱码、说话人迟一拍）与真实的「读得更准」在文本层面**无法可靠区分**。
    /// 任何基于长度或相似度的补正规则都会把事件频率交回给 OCR 稳定性，
    /// 正是要消灭的东西。因此这里选择少推：一条气泡产生且仅产生一个事件。
    pub(in crate::composition::application) fn take_update(&mut self, key: &str) -> bool {
        if self.seen.iter().any(|known| known == key) {
            return false;
        }
        self.seen.push_back(key.to_string());
        while self.seen.len() > REMEMBERED_BUBBLE_LIMIT {
            self.seen.pop_front();
        }
        true
    }

    /// 当前记住的气泡条数（只用于断言上限）。
    #[cfg(test)]
    pub(in crate::composition::application) fn remembered(&self) -> usize {
        self.seen.len()
    }
}

pub(in crate::composition::application) fn bubble_key(
    id: &crate::observation::chat::ObservedChatMessageId,
) -> String {
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

/// 展示去重状态：主循环持有一份，随循环结束一起消失。
///
/// 两个跟踪器都是**本轮循环的观测记忆**，不是进程级事实：把它们放回 `static`
/// 会让「换一轮循环」与「开一个新测试」都无法从头开始。
#[derive(Debug, Default)]
pub(in crate::composition::application) struct ObservationDedup {
    pub(super) ui_transitions: UiTransitionTracker,
    pub(super) bubbles: RememberedBubbleTracker,
}

impl ObservationDedup {
    /// 只在调试模式下调用：把本会被丢弃的聊天观测发布到事件流，不执行任何命令。
    ///
    /// 这样外部 AI 在驱动游戏的同时还能收到游戏内的输入，不必轮询；
    /// 而「调试模式不跑自动化」这条安全边界原封不动。
    pub(in crate::composition::application) fn publish_debug_chat_observations(
        &mut self,
        dispatches: &[ChatObservationDispatch],
    ) {
        let bus = crate::debug_events::bus();
        for dispatch in dispatches {
            match dispatch {
                ChatObservationDispatch::Primary { messages, .. } => {
                    // 判「是不是新消息」用**气泡身份**：不用 is_new（实测会把同一屏每帧都
                    // 标成新的，44 秒 13 条重复事件），也不用文本相等（会被 OCR 抖动击穿：
                    // `:` vs `：`、说话人迟一拍）。
                    let mut fresh = Vec::new();
                    for item in messages {
                        if !self.bubbles.take_update(&bubble_key(&item.id)) {
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
}
