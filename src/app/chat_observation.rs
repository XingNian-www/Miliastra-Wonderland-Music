use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use super::chat_scan::ChatMessage;
use crate::observation::chat::{
    BubbleSequence, ChatIdentity, ObservedChatMessageId, VisualSessionId,
};
use crate::observation::exclusive::{
    ExclusiveObservationRouter, ExclusiveSessionId, RoutedObservation,
};
use crate::observation::shared::{ObservationGap, ObservationRead, ObservationSubscriber};

const SHARED_CHAT_HISTORY_CAPACITY: usize = 64;

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
    Primary(Vec<ChatMessage>),
    Secondary(SecondaryChatObservation),
}

pub(super) enum ChatObservationDispatch {
    Primary(Vec<ChatMessage>),
    Secondary(SecondaryChatObservation),
    Gap(ObservationGap),
}

struct ChatObservationState {
    router: ExclusiveObservationRouter<ChatObservation>,
    business: ObservationSubscriber,
    visual_session: VisualSessionId,
    next_bubble_sequence: u64,
}

#[derive(Clone)]
pub(super) struct ChatObservationShared {
    state: Arc<Mutex<ChatObservationState>>,
}

impl ChatObservationShared {
    pub(super) fn new() -> Self {
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
            })),
        }
    }

    pub(super) fn publish_primary(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<Vec<ChatObservationDispatch>> {
        self.publish(ChatObservation::Primary(messages))
    }

    pub(super) fn publish_secondary(
        &self,
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
        Self::publish_locked(
            &mut state,
            ChatObservation::Secondary(SecondaryChatObservation {
                message_type: message_type.to_string(),
                friend_name: friend_name.to_string(),
                accepts_turtle_questions,
                messages: observed,
            }),
        )
    }

    fn publish(&self, observation: ChatObservation) -> Result<Vec<ChatObservationDispatch>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        Self::publish_locked(&mut state, observation)
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
        Ok(state.visual_session)
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
