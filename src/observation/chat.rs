pub(crate) mod scan;
mod scan_gate;
pub(crate) use scan_gate::PassiveScanGate;
#[allow(dead_code)]
pub(crate) mod freshness;
pub(crate) use freshness::ViewAbsence;

/// 外部聊天源的消息类型（来自 kernel 的通用定义，避免重复建模）。
pub(crate) use miliastra_kernel::chat_source::ExternalChatMessage;
mod passive;
mod secondary_hall_vision;
mod secondary_unread_vision;
#[allow(dead_code)]
mod stream;
#[allow(dead_code)]
mod timeline;
pub(crate) use timeline::*;
#[allow(dead_code)]
mod visual;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

pub(crate) use passive::{
    PassiveLayout, SECONDARY_HALL_FALLBACK_SENDER, SecondarySenderIdentity,
    parse_secondary_sender_identity, passive_layout, passive_text, recognize_passive_secondary,
};
pub(crate) use scan::{
    ChatMessage, ChatScanTelemetry, ChatScanTelemetrySink, RecognizedChatMessage,
    ResolvedTemplateArgs, TemplateArgs, count_chat_markers, count_scanned_chat_markers,
    prepare_chat_scan, prepare_chat_scan_with_markers, recognize_prepared_chat, scan_chat_markers,
};
pub(crate) use secondary_unread_vision::{
    FriendUnreadLayout, UnreadFriendHit, find_unread_friend_hits, unread_hit_still_visible,
};
pub(crate) use stream::{
    ChatObservationDispatch, ChatObservationExclusiveGuard, ChatObservationShared,
    CompletionAdvanceSubscriber, PrimaryObservedMessage, SecondaryChatObservation,
    SecondaryObservedMessage,
};
pub(crate) use visual::{
    SecondaryChatIdentity, SecondaryHallBubble, hall_bubble_sequence_is_retained_prefix,
    hall_bubble_sequence_overlap, latest_incoming_bubble_rect, latest_incoming_fingerprint,
    secondary_hall_bubbles,
};

// 单元测试（composition::application::shared_observer 等）需要这些纯函数；
// release 构建不使用它们，若放在普通导入里会被 warning 清理逻辑删除。
#[cfg(test)]
pub(crate) use passive::{
    hash_region, passive_secondary_identity, passive_sender, recognize_passive_hall,
};

#[cfg(test)]
pub(crate) use visual::SECONDARY_TITLE_RECT;

#[cfg(all(test, feature = "ocr-mnn"))]
mod actual_screenshot_tests;
#[cfg(test)]
mod freshness_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VisualSessionId(u64);

impl VisualSessionId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatIdentity {
    PrimaryHall,
    SecondaryHall,
    Friend(Arc<str>),
    PublicChannel,
    StrangerMessages,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BubbleSequence(u64);

impl BubbleSequence {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

// ==== 来源抽象（阶段一） ====

/// 消息来源。屏幕 OCR 与外部聊天源产生的消息走同一条业务账本，
/// 但去重、锁锚点与身份可信度完全不同，因此必须能被区分。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceId {
    /// 屏幕 OCR：靠气泡位置与视觉会话去重。
    Ocr,
    /// 外部聊天源（如 Miliastra-Wonderland-ChatBridge）：靠服务端序号去重。
    External(Arc<str>),
}

impl SourceId {
    pub const fn ocr() -> Self {
        Self::Ocr
    }

    pub fn external(name: impl Into<Arc<str>>) -> Self {
        Self::External(name.into())
    }

    pub const fn is_ocr(&self) -> bool {
        matches!(self, Self::Ocr)
    }

    /// 日志与诊断用的短名。
    pub fn label(&self) -> &str {
        match self {
            Self::Ocr => "ocr",
            Self::External(name) => name.as_ref(),
        }
    }
}

/// 一条消息属于哪个聊天容器（与「BOT 正看着哪块屏幕」无关）。
///
/// 外部聊天源直接给出 `channel`，OCR 只能从画面布局推断；
/// 两者都归一到这个类型，业务侧不再关心来源。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatScope {
    /// 大厅。外部源带 hallId；OCR 路径为 None（画面没有稳定 id）。
    Hall { id: Option<u64> },
    /// 私聊。`peer` 是对话对方的稳定标识（有 UID 用 UID，否则用昵称）。
    Private { peer: Arc<str> },
}

impl ChatScope {
    pub fn hall() -> Self {
        Self::Hall { id: None }
    }

    pub fn hall_with_id(id: u64) -> Self {
        Self::Hall { id: Some(id) }
    }

    pub fn private(peer: impl Into<Arc<str>>) -> Self {
        Self::Private { peer: peer.into() }
    }

    pub const fn is_hall(&self) -> bool {
        matches!(self, Self::Hall { .. })
    }

    pub fn label(&self) -> String {
        match self {
            Self::Hall { id: Some(id) } => format!("hall:{id}"),
            Self::Hall { id: None } => "hall".to_string(),
            Self::Private { peer } => format!("private:{peer}"),
        }
    }
}

impl ChatScope {
    /// 从 OCR 的屏幕分桶键推导消息范围。
    ///
    /// 只用于 OCR 路径：屏幕上一级/二级聊天区都属于当前大厅，粉色气泡是好友私聊。
    /// 外部路径由消息自带的 channel/hallId 直接给出，不要走这个推断。
    pub(crate) fn from_identity(chat: &ChatIdentity) -> Self {
        match chat {
            ChatIdentity::PrimaryHall | ChatIdentity::SecondaryHall => Self::hall(),
            // OCR 拿不到 UID，好友名就是 peer 标识。
            ChatIdentity::Friend(name) => Self::private(name.clone()),
            // 公共频道/陌生人/未知按大厅语义归类：它们不参与值守门控，
            // 是否能执行仍由 Unknown 不授权的既有约束决定。
            ChatIdentity::PublicChannel
            | ChatIdentity::StrangerMessages
            | ChatIdentity::Unknown => Self::hall(),
        }
    }
}

/// 命令去重的锚点：OCR 靠屏幕位置，外部源靠单调序号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockAnchor {
    /// OCR：与已记录条目同一文本且 `y` 相差不多即视为同一条。
    Screen { y: i32 },
    /// 外部源：序号单调递增，小于等于已记录的即视为旧消息。
    Sequence { after: u64 },
}
/// 一条被观察消息的稳定身份。
///
/// 三个字段都是"去重需要的最小信息"，但它们的含义随来源不同：
///
/// | 来源 | visual_session | bubble_sequence |
/// | --- | --- | --- |
/// | OCR | 视觉会话号（换屏/重进会递增） | 本帧内气泡的稳定序号 |
/// | 外部源 | 来源 epoch（重连递增） | 服务端消息序号 |
///
/// 业务账本只把它当不透明键比较，不解释这些字段的物理含义；
/// 需要来源语义的地方读 [`Self::source`]。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObservedChatMessageId {
    /// 消息来源。OCR 是屏幕，外部源是某个具体接入端。
    pub source: SourceId,
    pub visual_session: VisualSessionId,
    pub chat: ChatIdentity,
    pub bubble_sequence: BubbleSequence,
}

impl ObservedChatMessageId {
    /// OCR 路径：来源恒为屏幕。
    pub fn new(
        visual_session: VisualSessionId,
        chat: ChatIdentity,
        bubble_sequence: BubbleSequence,
    ) -> Self {
        Self::from_source(SourceId::ocr(), visual_session, chat, bubble_sequence)
    }

    /// 显式指定来源。外部聊天源用这个，避免被误当成屏幕气泡。
    pub fn from_source(
        source: SourceId,
        visual_session: VisualSessionId,
        chat: ChatIdentity,
        bubble_sequence: BubbleSequence,
    ) -> Self {
        Self {
            source,
            visual_session,
            chat,
            bubble_sequence,
        }
    }

    /// 是否来自屏幕 OCR。命令锁的位置语义只对这类消息成立。
    pub fn is_from_screen(&self) -> bool {
        self.source.is_ocr()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationFrameId(u64);

impl ObservationFrameId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedFrame {
    id: ObservationFrameId,
    captured_at: Instant,
}

impl ObservedFrame {
    pub fn id(self) -> ObservationFrameId {
        self.id
    }

    pub fn captured_at(self) -> Instant {
        self.captured_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationCompletionEvent {
    Succeeded {
        frame: ObservedFrame,
    },
    TerminalFailure {
        frame: ObservedFrame,
        reason: Arc<str>,
    },
}

impl ObservationCompletionEvent {
    pub fn frame(&self) -> ObservedFrame {
        match self {
            Self::Succeeded { frame } | Self::TerminalFailure { frame, .. } => *frame,
        }
    }

    pub fn captured_at(&self) -> Instant {
        self.frame().captured_at()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationWatermark {
    pub completed_through: ObservationFrameId,
    pub captured_through: Instant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionAdvance {
    events: Vec<ObservationCompletionEvent>,
    watermark: Option<ObservationWatermark>,
}

impl CompletionAdvance {
    pub fn events(&self) -> &[ObservationCompletionEvent] {
        &self.events
    }

    pub fn watermark(&self) -> Option<ObservationWatermark> {
        self.watermark
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationLedgerError {
    UnknownFrame(ObservationFrameId),
    AlreadyCompleted(ObservationFrameId),
}

impl fmt::Display for ObservationLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFrame(frame) => {
                write!(formatter, "observation frame {} is unknown", frame.get())
            }
            Self::AlreadyCompleted(frame) => {
                write!(
                    formatter,
                    "observation frame {} is already complete",
                    frame.get()
                )
            }
        }
    }
}

impl Error for ObservationLedgerError {}

struct PendingFrame {
    frame: ObservedFrame,
    outcome: Option<FrameCompletionOutcome>,
}

enum FrameCompletionOutcome {
    Success,
    TerminalFailure(Arc<str>),
}

pub struct ChatObservationLedger {
    pending: BTreeMap<ObservationFrameId, PendingFrame>,
    next_frame_id: u64,
    next_to_release: u64,
    watermark: Option<ObservationWatermark>,
}

impl ChatObservationLedger {
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next_frame_id: 1,
            next_to_release: 1,
            watermark: None,
        }
    }

    pub fn begin_frame(&mut self, captured_at: Instant) -> ObservedFrame {
        let id = ObservationFrameId(self.next_frame_id);
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        let frame = ObservedFrame { id, captured_at };
        self.pending.insert(
            id,
            PendingFrame {
                frame,
                outcome: None,
            },
        );
        frame
    }

    pub fn complete_success(
        &mut self,
        frame: ObservationFrameId,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        self.complete(frame, FrameCompletionOutcome::Success)
    }

    pub fn complete_failure(
        &mut self,
        frame: ObservationFrameId,
        reason: impl Into<Arc<str>>,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        self.complete(
            frame,
            FrameCompletionOutcome::TerminalFailure(reason.into()),
        )
    }

    pub fn watermark(&self) -> Option<ObservationWatermark> {
        self.watermark
    }

    fn complete(
        &mut self,
        frame: ObservationFrameId,
        outcome: FrameCompletionOutcome,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        let pending = self
            .pending
            .get_mut(&frame)
            .ok_or(ObservationLedgerError::UnknownFrame(frame))?;
        if pending.outcome.is_some() {
            return Err(ObservationLedgerError::AlreadyCompleted(frame));
        }
        pending.outcome = Some(outcome);

        let mut events = Vec::new();
        loop {
            let id = ObservationFrameId(self.next_to_release);
            let is_complete = self
                .pending
                .get(&id)
                .is_some_and(|pending| pending.outcome.is_some());
            if !is_complete {
                break;
            }
            let pending = self
                .pending
                .remove(&id)
                .expect("complete pending frame checked above");
            let outcome = pending.outcome.expect("complete outcome checked above");
            self.next_to_release = self.next_to_release.wrapping_add(1);
            self.watermark = Some(ObservationWatermark {
                completed_through: pending.frame.id,
                captured_through: pending.frame.captured_at,
            });
            events.push(match outcome {
                FrameCompletionOutcome::Success => ObservationCompletionEvent::Succeeded {
                    frame: pending.frame,
                },
                FrameCompletionOutcome::TerminalFailure(reason) => {
                    ObservationCompletionEvent::TerminalFailure {
                        frame: pending.frame,
                        reason,
                    }
                }
            });
        }

        let watermark = if events.is_empty() {
            None
        } else {
            Some(
                self.watermark
                    .expect("released frames always advance the watermark"),
            )
        };
        Ok(CompletionAdvance { events, watermark })
    }
}

impl Default for ChatObservationLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn successful_completion_emits_the_observed_frame_and_capture_time() {
        let mut ledger = ChatObservationLedger::new();
        let captured_at = Instant::now();
        let frame = ledger.begin_frame(captured_at);

        let advance = ledger.complete_success(frame.id()).unwrap();

        assert_eq!(advance.events().len(), 1);
        assert_eq!(advance.events()[0].frame(), frame);
        assert_eq!(advance.events()[0].captured_at(), captured_at);
        assert!(matches!(
            advance.events()[0],
            ObservationCompletionEvent::Succeeded { .. }
        ));
    }

    #[test]
    fn terminal_failure_emits_the_observed_frame_capture_time_and_reason() {
        let mut ledger = ChatObservationLedger::new();
        let captured_at = Instant::now();
        let frame = ledger.begin_frame(captured_at);

        let advance = ledger
            .complete_failure(frame.id(), "OCR retry exhausted")
            .unwrap();

        assert_eq!(advance.events().len(), 1);
        assert_eq!(advance.events()[0].frame(), frame);
        assert_eq!(advance.events()[0].captured_at(), captured_at);
        assert!(matches!(
            &advance.events()[0],
            ObservationCompletionEvent::TerminalFailure { reason, .. }
                if reason.as_ref() == "OCR retry exhausted"
        ));
    }

    #[test]
    fn terminal_failure_releases_the_completion_watermark_past_later_finished_frames() {
        let mut ledger = ChatObservationLedger::new();
        let started = Instant::now();
        let first = ledger.begin_frame(started);
        let second = ledger.begin_frame(started + Duration::from_millis(20));

        let blocked = ledger.complete_success(second.id()).unwrap();
        assert!(blocked.events().is_empty());
        assert_eq!(blocked.watermark(), None);

        let released = ledger
            .complete_failure(first.id(), "OCR retry exhausted")
            .unwrap();
        assert_eq!(released.events().len(), 2);
        assert!(matches!(
            &released.events()[0],
            ObservationCompletionEvent::TerminalFailure { reason, .. }
                if reason.as_ref() == "OCR retry exhausted"
        ));
        assert!(matches!(
            released.events()[1],
            ObservationCompletionEvent::Succeeded { .. }
        ));
        assert_eq!(released.events()[0].frame(), first);
        assert_eq!(released.events()[1].frame(), second);
        assert_eq!(
            released.watermark(),
            Some(ObservationWatermark {
                completed_through: second.id(),
                captured_through: second.captured_at(),
            })
        );
    }
}
