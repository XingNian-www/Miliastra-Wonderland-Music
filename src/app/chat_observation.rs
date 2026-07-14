use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use super::chat_scan::ChatMessage;
use crate::observation::exclusive::{
    ExclusiveObservationRouter, ExclusiveSessionId, RoutedObservation,
};
use crate::observation::shared::{ObservationGap, ObservationRead, ObservationSubscriber};

const SHARED_CHAT_HISTORY_CAPACITY: usize = 64;

#[derive(Clone)]
pub(super) struct SecondaryObservedMessage {
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
            state: Arc::new(Mutex::new(ChatObservationState { router, business })),
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
        observation: SecondaryChatObservation,
    ) -> Result<Vec<ChatObservationDispatch>> {
        self.publish(ChatObservation::Secondary(observation))
    }

    fn publish(&self, observation: ChatObservation) -> Result<Vec<ChatObservationDispatch>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if matches!(
            state.router.route(observation),
            RoutedObservation::Exclusive { .. }
        ) {
            return Ok(Vec::new());
        }

        let mut dispatches = Vec::new();
        loop {
            let ChatObservationState { router, business } = &mut *state;
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
