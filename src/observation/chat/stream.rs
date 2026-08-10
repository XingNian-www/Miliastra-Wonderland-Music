use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};

use super::scan::ChatMessage;
use crate::observation::chat::{
    BubbleSequence, ChatIdentity, ChatObservationLedger, CompletionAdvance,
    ObservationCompletionEvent, ObservationFrameId, ObservedChatMessageId, ObservedFrame,
    VisualSessionId,
};
use crate::observation::exclusive::{ExclusiveObservationRouter, ExclusiveSessionId};
use crate::observation::shared::{
    ObservationGap, ObservationRead, ObservationSubscriber, SharedObservationStream,
};
const SHARED_CHAT_HISTORY_CAPACITY: usize = 64;
const PRIMARY_VISIBLE_MIN_MESSAGES: usize = 2;
const PRIMARY_VISIBLE_MAX_MESSAGES: usize = 5;
const PRIMARY_REBASE_STABLE_SAMPLES: u32 = 2;

#[derive(Clone)]
pub(crate) struct PrimaryObservedMessage {
    pub(crate) id: ObservedChatMessageId,
    pub(crate) message: ChatMessage,
    pub(crate) is_new: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrimaryObservationCursor {
    visual_session: VisualSessionId,
    bubble_sequence: BubbleSequence,
}

impl PrimaryObservationCursor {
    pub(crate) fn is_before(self, message_id: &ObservedChatMessageId) -> bool {
        self.visual_session == message_id.visual_session
            && self.bubble_sequence.get() < message_id.bubble_sequence.get()
    }
}

#[derive(Clone)]
pub(crate) struct SecondaryRecognizedMessage {
    pub(crate) text: String,
    pub(crate) sender: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SecondaryObservedMessage {
    pub(crate) id: ObservedChatMessageId,
    pub(crate) text: String,
    pub(crate) sender: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SecondaryChatObservation {
    pub(crate) message_type: String,
    pub(crate) friend_name: String,
    pub(crate) accepts_turtle_questions: bool,
    pub(crate) messages: Vec<SecondaryObservedMessage>,
}

#[derive(Clone)]
enum ChatObservation {
    Primary {
        frame: ObservedFrame,
        messages: Vec<PrimaryObservedMessage>,
    },
    Secondary {
        frame: ObservedFrame,
        observation: SecondaryChatObservation,
    },
}

pub(crate) enum ChatObservationDispatch {
    Primary {
        frame: ObservedFrame,
        messages: Vec<PrimaryObservedMessage>,
    },
    Secondary {
        frame: ObservedFrame,
        observation: SecondaryChatObservation,
    },
    Gap(ObservationGap),
}

struct ChatObservationState {
    router: ExclusiveObservationRouter<ChatObservation>,
    business: ObservationSubscriber,
    visual_session: VisualSessionId,
    next_bubble_sequence: u64,
    primary_visible: Vec<PrimaryTrackedMessage>,
    primary_initialized: bool,
    primary_rebase_candidate: Option<PrimaryRebaseCandidate>,
    ledger: ChatObservationLedger,
    completion_advances: SharedObservationStream<CompletionAdvance>,
}

#[derive(Clone)]
struct PrimaryTrackedMessage {
    id: ObservedChatMessageId,
    message_type: String,
    text_key: String,
    handled: bool,
}

struct PrimaryRebaseCandidate {
    lost_samples: u32,
}

#[derive(Clone)]
pub(crate) struct ChatObservationShared {
    state: Arc<Mutex<ChatObservationState>>,
}

impl ChatObservationShared {
    pub(crate) fn new() -> Self {
        let router = ExclusiveObservationRouter::new(
            NonZeroUsize::new(SHARED_CHAT_HISTORY_CAPACITY)
                .expect("shared chat history capacity is non-zero"),
        );
        let business = router.subscribe();
        Self {
            state: Arc::new(Mutex::new(ChatObservationState {
                router,
                business,
                visual_session: VisualSessionId::new(1),
                next_bubble_sequence: 1,
                primary_visible: Vec::new(),
                primary_initialized: false,
                primary_rebase_candidate: None,
                ledger: ChatObservationLedger::new(),
                completion_advances: SharedObservationStream::new(
                    NonZeroUsize::new(SHARED_CHAT_HISTORY_CAPACITY)
                        .expect("shared chat history capacity is non-zero"),
                ),
            })),
        }
    }

    pub(crate) fn publish_primary(
        &self,
        frame: ObservedFrame,
        messages: Vec<ChatMessage>,
    ) -> Result<Vec<ChatObservationDispatch>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if !(PRIMARY_VISIBLE_MIN_MESSAGES..=PRIMARY_VISIBLE_MAX_MESSAGES).contains(&messages.len())
        {
            if !messages.is_empty() {
                log::debug!(
                    "一级聊天本轮只识别到 {} 条消息，保留上一帧记录并等待重扫",
                    messages.len()
                );
            }
            let dispatches = Self::publish_locked(
                &mut state,
                ChatObservation::Primary {
                    frame,
                    messages: Vec::new(),
                },
            )?;
            Self::complete_success(&mut state, frame.id())?;
            return Ok(dispatches);
        }

        let observed = track_primary_messages(&mut state, messages);
        let dispatches = Self::publish_locked(
            &mut state,
            ChatObservation::Primary {
                frame,
                messages: observed,
            },
        )?;
        Self::complete_success(&mut state, frame.id())?;
        Ok(dispatches)
    }

    pub(crate) fn observe_primary(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<Vec<PrimaryObservedMessage>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if !(PRIMARY_VISIBLE_MIN_MESSAGES..=PRIMARY_VISIBLE_MAX_MESSAGES).contains(&messages.len())
        {
            return Ok(Vec::new());
        }
        Ok(track_primary_messages(&mut state, messages))
    }

    pub(crate) fn primary_cursor(&self) -> Result<Option<PrimaryObservationCursor>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if !state.primary_initialized {
            return Ok(None);
        }
        Ok(state
            .primary_visible
            .last()
            .map(|message| PrimaryObservationCursor {
                visual_session: message.id.visual_session,
                bubble_sequence: message.id.bubble_sequence,
            }))
    }

    pub(crate) fn acknowledge_primary(&self, id: &ObservedChatMessageId) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        let Some(message) = state
            .primary_visible
            .iter_mut()
            .find(|message| &message.id == id)
        else {
            return Ok(false);
        };
        message.handled = true;
        Ok(true)
    }

    pub(crate) fn publish_secondary(
        &self,
        frame: ObservedFrame,
        message_type: &str,
        friend_name: &str,
        accepts_turtle_questions: bool,
        messages: Vec<SecondaryRecognizedMessage>,
    ) -> Result<Vec<ChatObservationDispatch>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        let chat = if message_type == "pink" {
            ChatIdentity::Friend(Arc::from(friend_name.trim()))
        } else {
            ChatIdentity::SecondaryHall
        };
        let mut observed = Vec::with_capacity(messages.len());
        for message in messages {
            let id = ObservedChatMessageId::new(
                state.visual_session,
                chat.clone(),
                BubbleSequence::new(state.next_bubble_sequence),
            );
            state.next_bubble_sequence = state
                .next_bubble_sequence
                .checked_add(1)
                .expect("secondary chat bubble sequence exhausted");
            observed.push(SecondaryObservedMessage {
                id,
                text: message.text,
                sender: message.sender,
            });
        }
        let dispatches = Self::publish_locked(
            &mut state,
            ChatObservation::Secondary {
                frame,
                observation: SecondaryChatObservation {
                    message_type: message_type.to_string(),
                    friend_name: friend_name.to_string(),
                    accepts_turtle_questions,
                    messages: observed,
                },
            },
        )?;
        Self::complete_success(&mut state, frame.id())?;
        Ok(dispatches)
    }

    fn publish_locked(
        state: &mut ChatObservationState,
        observation: ChatObservation,
    ) -> Result<Vec<ChatObservationDispatch>> {
        state.router.route(observation);

        let mut dispatches = Vec::new();
        loop {
            let ChatObservationState {
                router, business, ..
            } = &mut *state;
            match router.read_next(business) {
                Some(ObservationRead::Item { value, .. }) => {
                    dispatches.push(match Arc::unwrap_or_clone(value) {
                        ChatObservation::Primary { frame, messages } => {
                            ChatObservationDispatch::Primary { frame, messages }
                        }
                        ChatObservation::Secondary { frame, observation } => {
                            ChatObservationDispatch::Secondary { frame, observation }
                        }
                    });
                }
                Some(ObservationRead::Gap(gap)) => {
                    dispatches.push(ChatObservationDispatch::Gap(gap));
                }
                None => break,
            }
        }
        Ok(dispatches)
    }

    pub(crate) fn begin_visual_session(&self) -> Result<VisualSessionId> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        let next = state
            .visual_session
            .get()
            .checked_add(1)
            .expect("chat visual session sequence exhausted");
        state.visual_session = VisualSessionId::new(next);
        state.next_bubble_sequence = 1;
        state.primary_visible.clear();
        state.primary_initialized = false;
        state.primary_rebase_candidate = None;
        Ok(state.visual_session)
    }

    pub(crate) fn begin_frame(&self, captured_at: Instant) -> Result<ObservedFrame> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?
            .ledger
            .begin_frame(captured_at))
    }

    pub(crate) fn complete_without_messages(&self, frame: ObservedFrame) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        Self::complete_success(&mut state, frame.id())
    }

    pub(crate) fn record_terminal_failure(
        &self,
        frame: ObservedFrame,
        reason: impl Into<Arc<str>>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        let advance = state.ledger.complete_failure(frame.id(), reason)?;
        publish_completion_advance(&mut state, advance);
        Ok(())
    }

    fn complete_success(state: &mut ChatObservationState, frame: ObservationFrameId) -> Result<()> {
        let advance = state.ledger.complete_success(frame)?;
        publish_completion_advance(state, advance);
        Ok(())
    }

    pub(crate) fn subscribe_completion_advances(&self) -> Result<CompletionAdvanceSubscriber> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        Ok(CompletionAdvanceSubscriber {
            inner: state.completion_advances.subscribe(),
        })
    }

    pub(crate) fn read_completion_advance(
        &self,
        subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Result<Option<ObservationRead<CompletionAdvance>>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        Ok(subscriber.inner.read_next(&state.completion_advances))
    }

    pub(crate) fn begin_exclusive(&self) -> Result<ChatObservationExclusiveGuard> {
        let session = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?
            .router
            .begin_exclusive()?;
        Ok(ChatObservationExclusiveGuard {
            shared: self.clone(),
            session: Some(session),
        })
    }

    fn finish_exclusive(&self, session: ExclusiveSessionId) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?
            .router
            .finish_exclusive(session)?;
        Ok(())
    }
}

fn track_primary_messages(
    state: &mut ChatObservationState,
    messages: Vec<ChatMessage>,
) -> Vec<PrimaryObservedMessage> {
    if !state.primary_initialized {
        return establish_primary_baseline(state, messages);
    }

    let Some(overlap) = primary_suffix_prefix_overlap(state, &messages) else {
        return handle_primary_lost_overlap(state, messages);
    };
    state.primary_rebase_candidate = None;

    let previous_start = state.primary_visible.len() - overlap;
    let mut tracked = Vec::with_capacity(messages.len());
    let mut observed = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        let (next, is_new) = if index < overlap {
            update_primary_tracked_message(&state.primary_visible[previous_start + index], &message)
        } else {
            let next = new_primary_tracked_message(state, &message, false);
            (next, true)
        };
        observed.push(PrimaryObservedMessage {
            id: next.id.clone(),
            message,
            is_new,
        });
        tracked.push(next);
    }
    state.primary_visible = tracked;
    observed
}

fn establish_primary_baseline(
    state: &mut ChatObservationState,
    messages: Vec<ChatMessage>,
) -> Vec<PrimaryObservedMessage> {
    state.primary_initialized = true;
    state.primary_rebase_candidate = None;
    let mut tracked = Vec::with_capacity(messages.len());
    let mut observed = Vec::with_capacity(messages.len());
    for message in messages {
        let next = new_primary_tracked_message(state, &message, true);
        observed.push(PrimaryObservedMessage {
            id: next.id.clone(),
            message,
            is_new: false,
        });
        tracked.push(next);
    }
    state.primary_visible = tracked;
    observed
}

/// 无法与旧画面对应时的新基线建立：按内容对回旧基线。
/// 旧基线里见过的消息保持原状态（已处理的不重复识别），
/// 没见过的消息保留为未处理（命令不丢，宁可重复也不丢失）。
fn rebase_primary_baseline(
    state: &mut ChatObservationState,
    messages: Vec<ChatMessage>,
) -> Vec<PrimaryObservedMessage> {
    state.primary_initialized = true;
    state.primary_rebase_candidate = None;
    let mut consumed = vec![false; state.primary_visible.len()];
    let mut tracked = Vec::with_capacity(messages.len());
    let mut observed = Vec::with_capacity(messages.len());
    for message in messages {
        let text_key = normalize_primary_text(&message.text);
        let matched = state
            .primary_visible
            .iter()
            .enumerate()
            .find(|(index, previous)| {
                !consumed[*index]
                    && previous.message_type == message.message_type
                    && previous.text_key == text_key
            })
            .map(|(index, previous)| {
                consumed[index] = true;
                previous
            });
        match matched {
            Some(previous) => {
                let next = PrimaryTrackedMessage {
                    id: previous.id.clone(),
                    message_type: message.message_type.clone(),
                    text_key,
                    handled: previous.handled,
                };
                observed.push(PrimaryObservedMessage {
                    id: next.id.clone(),
                    message,
                    is_new: false,
                });
                tracked.push(next);
            }
            None => {
                let next = new_primary_tracked_message(state, &message, false);
                observed.push(PrimaryObservedMessage {
                    id: next.id.clone(),
                    message,
                    is_new: true,
                });
                tracked.push(next);
            }
        }
    }
    state.primary_visible = tracked;
    observed
}

fn handle_primary_lost_overlap(
    state: &mut ChatObservationState,
    messages: Vec<ChatMessage>,
) -> Vec<PrimaryObservedMessage> {
    let lost_samples = state
        .primary_rebase_candidate
        .as_ref()
        .map_or(1, |candidate| candidate.lost_samples.saturating_add(1));
    if lost_samples >= PRIMARY_REBASE_STABLE_SAMPLES {
        log::warn!(
            "一级聊天连续 {} 次无法与旧画面对应，已把当前画面作为新基线；旧消息按内容保留状态，新消息立即识别",
            lost_samples
        );
        return rebase_primary_baseline(state, messages);
    }
    state.primary_rebase_candidate = Some(PrimaryRebaseCandidate { lost_samples });
    log::debug!("一级聊天当前画面无法与旧画面可靠对应，暂不更新基线，等待重扫");
    Vec::new()
}

fn new_primary_tracked_message(
    state: &mut ChatObservationState,
    message: &ChatMessage,
    handled: bool,
) -> PrimaryTrackedMessage {
    let id = ObservedChatMessageId::new(
        state.visual_session,
        ChatIdentity::PrimaryHall,
        BubbleSequence::new(state.next_bubble_sequence),
    );
    state.next_bubble_sequence = state
        .next_bubble_sequence
        .checked_add(1)
        .expect("primary chat bubble sequence exhausted");
    PrimaryTrackedMessage {
        id,
        message_type: message.message_type.clone(),
        text_key: normalize_primary_text(&message.text),
        handled,
    }
}

fn update_primary_tracked_message(
    previous: &PrimaryTrackedMessage,
    message: &ChatMessage,
) -> (PrimaryTrackedMessage, bool) {
    let text_key = normalize_primary_text(&message.text);
    let is_new = !previous.handled;
    (
        PrimaryTrackedMessage {
            id: previous.id.clone(),
            message_type: message.message_type.clone(),
            text_key,
            handled: previous.handled,
        },
        is_new,
    )
}

fn normalize_primary_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_ascii_alphanumeric() || is_cjk(*character))
        .collect()
}

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

fn primary_suffix_prefix_overlap(
    state: &ChatObservationState,
    current: &[ChatMessage],
) -> Option<usize> {
    let previous = &state.primary_visible;
    let maximum = previous.len().min(current.len());
    (1..=maximum)
        .filter_map(|overlap| {
            let previous_start = previous.len() - overlap;
            let score = primary_ocr_sequence_score(
                previous[previous_start..]
                    .iter()
                    .zip(&current[..overlap])
                    .map(|(left, right)| {
                        (
                            left.message_type.as_str(),
                            left.text_key.as_str(),
                            left.handled,
                            right.message_type.as_str(),
                            normalize_primary_text(&right.text),
                        )
                    }),
            );
            score.is_reliable().then_some((overlap, score))
        })
        .max_by_key(|(overlap, score)| (score.weight(), score.exact, *overlap))
        .map(|(overlap, _)| overlap)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OcrSequenceScore {
    exact: usize,
    close: usize,
    different: usize,
    unhandled_different: usize,
}

impl OcrSequenceScore {
    fn is_reliable(self) -> bool {
        (self.different == 0 && (self.exact > 0 || self.close == 1))
            || (self.different == 1 && self.unhandled_different == 1 && self.exact > 0)
    }

    fn weight(self) -> usize {
        self.exact * 2 + self.close
    }
}

fn primary_ocr_sequence_score<'a>(
    pairs: impl Iterator<Item = (&'a str, &'a str, bool, &'a str, String)>,
) -> OcrSequenceScore {
    let mut score = OcrSequenceScore::default();
    for (left_type, left_text, left_handled, right_type, right_text) in pairs {
        if left_type != right_type {
            score.different += 1;
            continue;
        }
        match primary_ocr_text_match(left_text, &right_text) {
            OcrTextMatch::Exact => score.exact += 1,
            OcrTextMatch::Close => score.close += 1,
            OcrTextMatch::Different => {
                score.different += 1;
                if !left_handled {
                    score.unhandled_different += 1;
                }
            }
        }
    }
    score
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcrTextMatch {
    Exact,
    Close,
    Different,
}

fn primary_ocr_text_match(previous: &str, current: &str) -> OcrTextMatch {
    if previous == current && !previous.is_empty() {
        return OcrTextMatch::Exact;
    }
    if previous.is_empty() || current.is_empty() {
        return OcrTextMatch::Different;
    }
    let previous = previous.chars().collect::<Vec<_>>();
    let current = current.chars().collect::<Vec<_>>();
    let maximum_len = previous.len().max(current.len());
    let tolerance = if maximum_len <= 8 {
        1
    } else {
        (maximum_len / 6).max(1)
    };
    let distance = levenshtein_distance(&previous, &current);
    if distance <= tolerance && distance < maximum_len {
        OcrTextMatch::Close
    } else {
        OcrTextMatch::Different
    }
}

fn levenshtein_distance(left: &[char], right: &[char]) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0usize; right.len() + 1];
    for (left_index, left_character) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != right_character);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn publish_completion_advance(state: &mut ChatObservationState, advance: CompletionAdvance) {
    for event in advance.events() {
        if let ObservationCompletionEvent::TerminalFailure { frame, reason } = event {
            log::error!(
                "聊天观察帧终止失败: frame={} reason={}",
                frame.id().get(),
                reason
            );
        }
    }
    if let Some(watermark) = advance.watermark() {
        log::debug!(
            "聊天观察完成水位推进: frame={} age={}ms",
            watermark.completed_through.get(),
            watermark.captured_through.elapsed().as_millis()
        );
        state.completion_advances.publish(advance);
    }
}

pub(crate) struct CompletionAdvanceSubscriber {
    inner: ObservationSubscriber,
}

pub(crate) struct ChatObservationExclusiveGuard {
    shared: ChatObservationShared,
    session: Option<ExclusiveSessionId>,
}

impl Drop for ChatObservationExclusiveGuard {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        if let Err(error) = self.shared.finish_exclusive(session) {
            log::error!("结束独占聊天观察会话失败: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ui::geometry::Rect;

    #[test]
    fn primary_ocr_revision_keeps_the_ocr_message_identity() {
        let shared = ChatObservationShared::new();

        let first_frame = shared.begin_frame(Instant::now()).unwrap();
        let first = shared
            .publish_primary(
                first_frame,
                vec![
                    message_at("较早消息", 0, 10),
                    message_at("用户：@确汄", 20, 20),
                ],
            )
            .unwrap();
        let first_id = primary_messages(&first)[1].id.clone();
        let revised_frame = shared.begin_frame(Instant::now()).unwrap();
        let revised = shared
            .publish_primary(
                revised_frame,
                vec![
                    message_at("较早消息", 0, 80),
                    message_at("用户：@确认", 20, 90),
                ],
            )
            .unwrap();

        assert_eq!(primary_messages(&revised)[1].id, first_id);
    }

    #[test]
    fn primary_position_and_wrapping_changes_do_not_change_ocr_message_identity() {
        let shared = ChatObservationShared::new();
        let baseline = publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );
        let baseline_ids = primary_messages(&baseline)
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();

        let mut moved_and_wrapped =
            vec![message_at("消息1", 35, 180), message_at("消息2", 90, 220)];
        moved_and_wrapped[0].block.height = 45;
        moved_and_wrapped[1].block.height = 35;
        let moved_and_wrapped = publish_primary(&shared, moved_and_wrapped);

        assert_eq!(
            primary_messages(&moved_and_wrapped)
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>(),
            baseline_ids
        );
    }

    #[test]
    fn primary_baseline_is_not_reported_as_new() {
        let shared = ChatObservationShared::new();

        let baseline = publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("@状态", 20, 20)],
        );

        assert!(
            primary_messages(&baseline)
                .iter()
                .all(|message| !message.is_new)
        );
    }

    #[test]
    fn primary_scroll_retains_the_overlap_and_reports_appended_messages_immediately() {
        let shared = ChatObservationShared::new();
        let baseline = publish_primary(
            &shared,
            vec![
                message_at("消息1", 0, 10),
                message_at("消息2", 20, 20),
                message_at("消息3", 40, 30),
                message_at("@状态", 60, 40),
            ],
        );
        let baseline_messages = primary_messages(&baseline);
        let old_message_3 = baseline_messages[2].id.clone();
        let old_status = baseline_messages[3].id.clone();
        let updated = vec![
            message_at("消息3", 0, 30),
            message_at("@状态", 20, 40),
            message_at("状态消息", 40, 50),
            message_at("@状态", 60, 60),
        ];

        let first_sample = publish_primary(&shared, updated);
        let messages = primary_messages(&first_sample);

        assert_eq!(messages[0].id, old_message_3);
        assert_eq!(messages[1].id, old_status);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.is_new)
                .collect::<Vec<_>>(),
            vec![false, false, true, true]
        );
    }

    #[test]
    fn primary_ocr_correction_keeps_an_unhandled_message_new_for_retry() {
        let shared = ChatObservationShared::new();
        publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );

        let incorrect = publish_primary(
            &shared,
            vec![message_at("消息2", 0, 70), message_at("丨", 20, 80)],
        );
        let command_id = primary_messages(&incorrect)[1].id.clone();
        assert!(primary_messages(&incorrect)[1].is_new);

        let corrected = publish_primary(
            &shared,
            vec![message_at("消息2", 0, 90), message_at("@确认", 20, 100)],
        );
        assert_eq!(primary_messages(&corrected)[1].id, command_id);
        assert!(primary_messages(&corrected)[1].is_new);

        shared.acknowledge_primary(&command_id).unwrap();
        let handled = publish_primary(
            &shared,
            vec![message_at("消息2", 0, 110), message_at("@确认", 20, 120)],
        );
        assert_eq!(primary_messages(&handled)[1].id, command_id);
        assert!(!primary_messages(&handled)[1].is_new);
    }

    #[test]
    fn primary_identical_text_in_a_distinct_appended_bubble_is_new() {
        let shared = ChatObservationShared::new();
        let baseline = publish_primary(
            &shared,
            vec![message_at("消息", 0, 10), message_at("@状态", 20, 20)],
        );
        let old_status_id = primary_messages(&baseline)[1].id.clone();
        let updated = vec![message_at("@状态", 0, 20), message_at("@状态", 20, 20)];

        let current = publish_primary(&shared, updated);
        let messages = primary_messages(&current);

        assert_eq!(messages[0].id, old_status_id);
        assert_ne!(messages[1].id, old_status_id);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.is_new)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
    }

    #[test]
    fn primary_overlap_prefers_exact_ocr_text_over_a_longer_fuzzy_alignment() {
        let shared = ChatObservationShared::new();
        let baseline = publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );
        let old_second_id = primary_messages(&baseline)[1].id.clone();

        let current = publish_primary(
            &shared,
            vec![message_at("消息2", 0, 80), message_at("消息3", 20, 90)],
        );
        let messages = primary_messages(&current);

        assert_eq!(messages[0].id, old_second_id);
        assert!(messages[1].is_new);
    }

    #[test]
    fn primary_rebase_keeps_unhandled_commands_and_preserves_handled_messages() {
        let shared = ChatObservationShared::new();
        publish_primary(
            &shared,
            vec![
                message_at("消息1", 0, 10),
                message_at("消息2", 20, 20),
                message_at("消息3", 40, 30),
            ],
        );

        // 画面头部多出无法对应的新消息（overlap 失败），画面仍含旧消息「消息3」。
        let rolling = vec![message_at("不同", 0, 5), message_at("消息3", 40, 30)];
        let first_lost = publish_primary(&shared, rolling.clone());
        assert!(primary_messages(&first_lost).is_empty());
        let settled = publish_primary(&shared, rolling);

        let messages = primary_messages(&settled);
        assert_eq!(messages.len(), 2);
        // 未见过的新消息保留为未处理（命令不丢）；旧消息保持已处理（不重复识别）。
        assert_eq!(
            messages
                .iter()
                .map(|message| message.is_new)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
    }

    #[test]
    fn primary_rebase_does_not_wait_for_two_identical_lost_frames() {
        let shared = ChatObservationShared::new();
        publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );

        let first_lost = publish_primary(
            &shared,
            vec![
                message_at("旧画面已滚出", 0, 30),
                message_at("临时识别", 20, 40),
            ],
        );
        assert!(primary_messages(&first_lost).is_empty());

        let second_lost = publish_primary(
            &shared,
            vec![
                message_at("另一个新消息", 0, 50),
                message_at("用户：@状态", 20, 60),
            ],
        );
        let messages = primary_messages(&second_lost);

        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| message.is_new));
        assert_eq!(messages[1].message.text, "用户：@状态");
    }

    #[test]
    fn primary_single_message_frame_does_not_replace_the_previous_sequence() {
        let shared = ChatObservationShared::new();
        let baseline = publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );
        let old_second_id = primary_messages(&baseline)[1].id.clone();

        let incomplete = publish_primary(&shared, vec![message_at("误识别", 0, 99)]);
        assert!(primary_messages(&incomplete).is_empty());

        let updated = vec![message_at("消息2", 0, 20), message_at("@确认", 20, 30)];
        let current = publish_primary(&shared, updated);
        let messages = primary_messages(&current);

        assert_eq!(messages[0].id, old_second_id);
        assert!(messages[1].is_new);
    }

    #[test]
    fn primary_cursor_recovers_a_message_already_visible_on_the_first_decision_scan() {
        let shared = ChatObservationShared::new();
        publish_primary(
            &shared,
            vec![message_at("消息1", 0, 10), message_at("消息2", 20, 20)],
        );
        let cursor = shared
            .primary_cursor()
            .unwrap()
            .expect("primary baseline cursor");
        let updated = vec![message_at("消息2", 0, 20), message_at("@确认", 20, 30)];

        let first = shared.observe_primary(updated.clone()).unwrap();
        let decisions = first
            .iter()
            .filter(|message| cursor.is_before(&message.id))
            .map(|message| message.message.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(decisions, ["@确认"]);
    }

    #[test]
    fn primary_matching_discards_punctuation_but_keeps_ai_letters() {
        assert_eq!(normalize_primary_text("【玩家-01】：＠AI！"), "玩家01ai");
        assert_eq!(primary_ocr_text_match("甲", "乙"), OcrTextMatch::Different);
    }

    #[test]
    fn completion_subscriber_receives_success_with_the_original_capture_time() {
        let shared = ChatObservationShared::new();
        let mut subscriber = shared.subscribe_completion_advances().unwrap();
        let captured_at = Instant::now();
        let frame = shared.begin_frame(captured_at).unwrap();

        shared.complete_without_messages(frame).unwrap();

        let advance = next_completion_advance(&shared, &mut subscriber);
        assert_eq!(advance.events().len(), 1);
        assert_eq!(advance.events()[0].frame(), frame);
        assert_eq!(advance.events()[0].captured_at(), captured_at);
        assert!(matches!(
            advance.events()[0],
            ObservationCompletionEvent::Succeeded { .. }
        ));
    }

    #[test]
    fn completion_subscriber_receives_terminal_failure_without_a_message() {
        let shared = ChatObservationShared::new();
        let mut subscriber = shared.subscribe_completion_advances().unwrap();
        let frame = shared.begin_frame(Instant::now()).unwrap();

        shared
            .record_terminal_failure(frame, "OCR retry exhausted")
            .unwrap();

        let advance = next_completion_advance(&shared, &mut subscriber);
        assert_eq!(advance.events().len(), 1);
        assert!(matches!(
            &advance.events()[0],
            ObservationCompletionEvent::TerminalFailure {
                frame: failed_frame,
                reason,
            } if *failed_frame == frame && reason.as_ref() == "OCR retry exhausted"
        ));
    }

    #[test]
    fn completion_subscriber_observes_watermark_advances_in_frame_order() {
        let shared = ChatObservationShared::new();
        let mut subscriber = shared.subscribe_completion_advances().unwrap();
        let started = Instant::now();
        let first = shared.begin_frame(started).unwrap();
        let second = shared
            .begin_frame(started + Duration::from_millis(20))
            .unwrap();

        shared.complete_without_messages(second).unwrap();
        assert!(
            shared
                .read_completion_advance(&mut subscriber)
                .unwrap()
                .is_none()
        );

        shared.record_terminal_failure(first, "failed").unwrap();
        let advance = next_completion_advance(&shared, &mut subscriber);
        assert_eq!(
            advance
                .events()
                .iter()
                .map(ObservationCompletionEvent::frame)
                .collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(
            advance.watermark(),
            Some(crate::observation::chat::ObservationWatermark {
                completed_through: second.id(),
                captured_through: second.captured_at(),
            })
        );
    }

    #[test]
    fn exclusive_chat_still_publishes_shared_dispatches() {
        let shared = ChatObservationShared::new();
        let mut subscriber = shared.subscribe_completion_advances().unwrap();
        let _exclusive = shared.begin_exclusive().unwrap();
        let frame = shared.begin_frame(Instant::now()).unwrap();

        let dispatches = shared
            .publish_secondary(
                frame,
                "pink",
                "private friend",
                false,
                vec![SecondaryRecognizedMessage {
                    text: "private text".to_string(),
                    sender: None,
                }],
            )
            .unwrap();

        let [ChatObservationDispatch::Secondary { observation, .. }] = dispatches.as_slice() else {
            panic!("exclusive observation was not published to the shared stream");
        };
        assert_eq!(observation.messages.len(), 1);
        assert_eq!(observation.messages[0].text, "private text");
        let advance = next_completion_advance(&shared, &mut subscriber);
        assert_eq!(advance.events().len(), 1);
        assert_eq!(advance.events()[0].frame(), frame);
    }

    fn message_at(text: &str, y: i32, _ignored_image_revision: u8) -> ChatMessage {
        ChatMessage {
            message_type: "blue".to_string(),
            block: Rect::new(0, y, 10, 10),
            text: text.to_string(),
        }
    }

    fn publish_primary(
        shared: &ChatObservationShared,
        messages: Vec<ChatMessage>,
    ) -> Vec<ChatObservationDispatch> {
        let frame = shared.begin_frame(Instant::now()).unwrap();
        shared.publish_primary(frame, messages).unwrap()
    }

    fn primary_messages(dispatches: &[ChatObservationDispatch]) -> &[PrimaryObservedMessage] {
        let ChatObservationDispatch::Primary { messages, .. } = &dispatches[0] else {
            panic!("primary observation was not dispatched");
        };
        messages
    }

    fn next_completion_advance(
        shared: &ChatObservationShared,
        subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Arc<CompletionAdvance> {
        let Some(ObservationRead::Item { value, .. }) = shared
            .read_completion_advance(subscriber)
            .expect("completion stream remains available")
        else {
            panic!("completion advance was not published");
        };
        value
    }
}
