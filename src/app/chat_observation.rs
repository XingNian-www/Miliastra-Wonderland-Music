use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};

use super::change_detection::{ChangeFingerprint, change_stats};
use super::chat_scan::ChatMessage;
use crate::observation::chat::{
    BubbleSequence, ChatIdentity, ChatObservationLedger, CompletionAdvance, FrameCompletionOutcome,
    ObservationFrameId, ObservedChatMessageId, ObservedFrame, VisualSessionId,
};
use crate::observation::exclusive::{
    ExclusiveObservationRouter, ExclusiveSessionId, RoutedObservation,
};
use crate::observation::shared::{ObservationGap, ObservationRead, ObservationSubscriber};

const SHARED_CHAT_HISTORY_CAPACITY: usize = 64;

#[derive(Clone)]
pub(super) struct PrimaryObservedMessage {
    pub(super) id: ObservedChatMessageId,
    pub(super) message: ChatMessage,
}

#[derive(Clone)]
pub(super) struct SecondaryRecognizedMessage {
    pub(super) text: String,
    pub(super) sender: Option<String>,
}

#[derive(Clone)]
pub(super) struct SecondaryObservedMessage {
    pub(super) id: ObservedChatMessageId,
    pub(super) text: String,
    pub(super) sender: Option<String>,
}

#[derive(Clone)]
pub(super) struct SecondaryChatObservation {
    pub(super) message_type: String,
    pub(super) friend_name: String,
    pub(super) accepts_turtle_questions: bool,
    pub(super) messages: Vec<SecondaryObservedMessage>,
}

#[derive(Clone)]
enum ChatObservation {
    Primary(Vec<PrimaryObservedMessage>),
    Secondary(SecondaryChatObservation),
}

pub(super) enum ChatObservationDispatch {
    Primary(Vec<PrimaryObservedMessage>),
    Secondary(SecondaryChatObservation),
    Gap(ObservationGap),
}

struct ChatObservationState {
    router: ExclusiveObservationRouter<ChatObservation>,
    business: ObservationSubscriber,
    visual_session: VisualSessionId,
    next_bubble_sequence: u64,
    primary_visible: Vec<PrimaryTrackedMessage>,
    change_mean_threshold: f32,
    change_pixel_threshold: f32,
    ledger: ChatObservationLedger,
}

struct PrimaryTrackedMessage {
    id: ObservedChatMessageId,
    message_type: String,
    visual: ChangeFingerprint,
}

#[derive(Clone)]
pub(super) struct ChatObservationShared {
    state: Arc<Mutex<ChatObservationState>>,
}

impl ChatObservationShared {
    pub(super) fn new(change_mean_threshold: f32, change_pixel_threshold: f32) -> Self {
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
                change_mean_threshold,
                change_pixel_threshold,
                ledger: ChatObservationLedger::new(),
            })),
        }
    }

    pub(super) fn publish_primary(
        &self,
        frame: ObservedFrame,
        messages: Vec<ChatMessage>,
    ) -> Result<Vec<ChatObservationDispatch>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if messages.is_empty() {
            let dispatches =
                Self::publish_locked(&mut state, ChatObservation::Primary(Vec::new()))?;
            Self::complete_success(&mut state, frame.id())?;
            return Ok(dispatches);
        }

        let previous_ids = match_primary_messages(&state, &messages);
        let mut observed = Vec::with_capacity(messages.len());
        for (index, message) in messages.into_iter().enumerate() {
            let id = previous_ids[index].clone().unwrap_or_else(|| {
                let id = ObservedChatMessageId::new(
                    state.visual_session,
                    ChatIdentity::PrimaryHall,
                    BubbleSequence::new(state.next_bubble_sequence),
                );
                state.next_bubble_sequence = state
                    .next_bubble_sequence
                    .checked_add(1)
                    .expect("primary chat bubble sequence exhausted");
                id
            });
            observed.push(PrimaryObservedMessage { id, message });
        }
        state.primary_visible = observed
            .iter()
            .map(|observed| PrimaryTrackedMessage {
                id: observed.id.clone(),
                message_type: observed.message.message_type.clone(),
                visual: observed.message.visual.clone(),
            })
            .collect();
        let dispatches = Self::publish_locked(&mut state, ChatObservation::Primary(observed))?;
        Self::complete_success(&mut state, frame.id())?;
        Ok(dispatches)
    }

    pub(super) fn publish_secondary(
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
            ChatObservation::Secondary(SecondaryChatObservation {
                message_type: message_type.to_string(),
                friend_name: friend_name.to_string(),
                accepts_turtle_questions,
                messages: observed,
            }),
        )?;
        Self::complete_success(&mut state, frame.id())?;
        Ok(dispatches)
    }

    fn publish_locked(
        state: &mut ChatObservationState,
        observation: ChatObservation,
    ) -> Result<Vec<ChatObservationDispatch>> {
        if matches!(
            state.router.route(observation),
            RoutedObservation::Exclusive { .. }
        ) {
            return Ok(Vec::new());
        }

        let mut dispatches = Vec::new();
        loop {
            let ChatObservationState {
                router, business, ..
            } = &mut *state;
            match router.read_next(business) {
                Some(ObservationRead::Item { value, .. }) => {
                    dispatches.push(match Arc::unwrap_or_clone(value) {
                        ChatObservation::Primary(messages) => {
                            ChatObservationDispatch::Primary(messages)
                        }
                        ChatObservation::Secondary(observation) => {
                            ChatObservationDispatch::Secondary(observation)
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

    pub(super) fn begin_visual_session(&self) -> Result<VisualSessionId> {
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
        Ok(state.visual_session)
    }

    pub(super) fn begin_frame(&self, captured_at: Instant) -> Result<ObservedFrame> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?
            .ledger
            .begin_frame(captured_at))
    }

    pub(super) fn complete_without_messages(&self, frame: ObservedFrame) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        Self::complete_success(&mut state, frame.id())
    }

    pub(super) fn record_terminal_failure(
        &self,
        frame: ObservedFrame,
        reason: impl Into<Arc<str>>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        let advance = state.ledger.complete_failure(frame.id(), reason)?;
        log_completion_advance(&advance);
        Ok(())
    }

    fn complete_success(state: &mut ChatObservationState, frame: ObservationFrameId) -> Result<()> {
        let advance = state.ledger.complete_success(frame)?;
        log_completion_advance(&advance);
        Ok(())
    }

    pub(super) fn begin_exclusive(&self) -> Result<ChatObservationExclusiveGuard> {
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

fn match_primary_messages(
    state: &ChatObservationState,
    current: &[ChatMessage],
) -> Vec<Option<ObservedChatMessageId>> {
    let previous = &state.primary_visible;
    let mut lengths = vec![vec![0usize; current.len() + 1]; previous.len() + 1];
    for left in (0..previous.len()).rev() {
        for right in (0..current.len()).rev() {
            lengths[left][right] = if primary_visual_matches(
                &previous[left],
                &current[right],
                state.change_mean_threshold,
                state.change_pixel_threshold,
            ) {
                1 + lengths[left + 1][right + 1]
            } else {
                lengths[left + 1][right].max(lengths[left][right + 1])
            };
        }
    }

    let mut matches = vec![None; current.len()];
    let mut left = 0usize;
    let mut right = 0usize;
    while left < previous.len() && right < current.len() {
        if primary_visual_matches(
            &previous[left],
            &current[right],
            state.change_mean_threshold,
            state.change_pixel_threshold,
        ) && lengths[left][right] == 1 + lengths[left + 1][right + 1]
        {
            matches[right] = Some(previous[left].id.clone());
            left += 1;
            right += 1;
        } else if lengths[left + 1][right] >= lengths[left][right + 1] {
            left += 1;
        } else {
            right += 1;
        }
    }
    matches
}

fn primary_visual_matches(
    previous: &PrimaryTrackedMessage,
    current: &ChatMessage,
    mean_threshold: f32,
    pixel_threshold: f32,
) -> bool {
    if previous.message_type != current.message_type {
        return false;
    }
    let stats = change_stats(&previous.visual, &current.visual);
    stats.mean_abs_diff < mean_threshold && stats.changed_ratio < pixel_threshold
}

fn log_completion_advance(advance: &CompletionAdvance) {
    for completed in advance.completed() {
        if let FrameCompletionOutcome::TerminalFailure(reason) = completed.outcome() {
            log::error!(
                "聊天观察帧终止失败: frame={} reason={}",
                completed.frame().id().get(),
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
    }
}

pub(super) struct ChatObservationExclusiveGuard {
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
    use super::*;
    use crate::app::change_detection::ChangeFingerprint;
    use crate::app::geometry::Rect;

    #[test]
    fn primary_ocr_revision_keeps_the_visual_message_identity() {
        let shared = ChatObservationShared::new(6.0, 0.03);

        let first_frame = shared.begin_frame(Instant::now()).unwrap();
        let first = shared
            .publish_primary(first_frame, vec![message("第一次 OCR")])
            .unwrap();
        let first_id = primary_id(&first);
        let revised_frame = shared.begin_frame(Instant::now()).unwrap();
        let revised = shared
            .publish_primary(revised_frame, vec![message("修订后的 OCR")])
            .unwrap();

        assert_eq!(primary_id(&revised), first_id);
    }

    fn message(text: &str) -> ChatMessage {
        ChatMessage {
            message_type: "blue".to_string(),
            block: Rect::new(0, 0, 10, 10),
            text: text.to_string(),
            visual: ChangeFingerprint {
                pixels: vec![10, 20, 30, 40],
                width: 2,
                height: 2,
            },
        }
    }

    fn primary_id(dispatches: &[ChatObservationDispatch]) -> ObservedChatMessageId {
        let ChatObservationDispatch::Primary(messages) = &dispatches[0] else {
            panic!("primary observation was not dispatched");
        };
        messages[0].id.clone()
    }
}
