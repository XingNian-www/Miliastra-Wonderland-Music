use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

#[derive(Clone, Debug)]
pub(crate) struct PrimaryObservedMessage {
    pub(crate) id: ObservedChatMessageId,
    pub(crate) message: ChatMessage,
    pub(crate) is_new: bool,
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
    /// 消息来源的账号（仅外部聊天 API 提供）；权限解析优先用它。
    pub(crate) account: Option<miliastra_kernel::chat_source::CommandAccount>,
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
    business_dispatches: usize,
    exclusive_requested: bool,
    exclusive_active: bool,
    router: ExclusiveObservationRouter<ChatObservation>,
    business: ObservationSubscriber,
    visual_session: VisualSessionId,
    next_bubble_sequence: u64,
    ledger: ChatObservationLedger,
    completion_advances: SharedObservationStream<CompletionAdvance>,
}

#[derive(Clone)]
pub(crate) struct ChatObservationShared {
    state: Arc<Mutex<ChatObservationState>>,
    dispatch_changed: Arc<Condvar>,
}

impl ChatObservationShared {
    pub(crate) fn new() -> Self {
        let router = ExclusiveObservationRouter::new(
            NonZeroUsize::new(SHARED_CHAT_HISTORY_CAPACITY)
                .expect("shared chat history capacity is non-zero"),
        );
        let business = router.subscribe();
        Self {
            dispatch_changed: Arc::new(Condvar::new()),
            state: Arc::new(Mutex::new(ChatObservationState {
                business_dispatches: 0,
                exclusive_requested: false,
                exclusive_active: false,
                router,
                business,
                visual_session: VisualSessionId::new(1),
                next_bubble_sequence: 1,
                ledger: ChatObservationLedger::new(),
                completion_advances: SharedObservationStream::new(
                    NonZeroUsize::new(SHARED_CHAT_HISTORY_CAPACITY)
                        .expect("shared chat history capacity is non-zero"),
                ),
            })),
        }
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
            state.next_bubble_sequence = state.next_bubble_sequence.wrapping_add(1);
            observed.push(SecondaryObservedMessage {
                id,
                text: message.text,
                sender: message.sender,
                // OCR 路径没有账号。
                account: None,
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
        let next = state.visual_session.get().wrapping_add(1);
        state.visual_session = VisualSessionId::new(next);
        state.next_bubble_sequence = 1;

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
        let reason = reason.into();
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

    /// Shared OCR stays live. Only normal business dispatch defers during a decision.
    pub(crate) fn try_begin_business_dispatch(&self) -> Result<Option<ChatBusinessDispatchGuard>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if state.exclusive_requested || state.exclusive_active {
            return Ok(None);
        }
        state.business_dispatches += 1;
        Ok(Some(ChatBusinessDispatchGuard {
            shared: self.clone(),
        }))
    }

    pub(crate) fn begin_exclusive(&self) -> Result<ChatObservationExclusiveGuard> {
        self.begin_exclusive_until(Instant::now() + Duration::from_secs(30), || false)
    }

    pub(crate) fn begin_exclusive_until<F>(
        &self,
        deadline: Instant,
        aborted: F,
    ) -> Result<ChatObservationExclusiveGuard>
    where
        F: Fn() -> bool,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        if state.exclusive_requested || state.exclusive_active {
            return Err(anyhow!("已有独占聊天观察会话"));
        }
        state.exclusive_requested = true;
        self.dispatch_changed.notify_all();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if aborted() || remaining.is_zero() {
                state.exclusive_requested = false;
                self.dispatch_changed.notify_all();
                return Err(anyhow!("建立独占聊天观察已取消或超时"));
            }
            if state.business_dispatches == 0 {
                break;
            }
            state = self
                .dispatch_changed
                .wait_timeout(state, remaining.min(Duration::from_millis(50)))
                .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?
                .0;
        }
        let session = match state.router.begin_exclusive() {
            Ok(session) => session,
            Err(error) => {
                state.exclusive_requested = false;
                self.dispatch_changed.notify_all();
                return Err(error.into());
            }
        };
        state.exclusive_requested = false;
        state.exclusive_active = true;
        Ok(ChatObservationExclusiveGuard {
            shared: self.clone(),
            session: Some(session),
        })
    }

    fn finish_exclusive(&self, session: ExclusiveSessionId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("聊天观察流状态锁已损坏"))?;
        state.router.finish_exclusive(session)?;
        state.exclusive_active = false;
        self.dispatch_changed.notify_all();
        Ok(())
    }
}

fn publish_completion_advance(state: &mut ChatObservationState, advance: CompletionAdvance) {
    for event in advance.events() {
        if let ObservationCompletionEvent::TerminalFailure { frame, reason } = event {
            log::error!(
                "聊天观察帧终止失败（步骤：处理该帧的观察事件），该帧之后的观察结果不会产出，需等待下一帧重建；帧号 {}，原因 {}",
                frame.id().get(),
                reason
            );
        }
    }
    if let Some(watermark) = advance.watermark() {
        log::debug!(
            "聊天观察完成水位推进：已推进到帧号 {}，该帧距采集已过 {} 毫秒",
            watermark.completed_through.get(),
            watermark.captured_through.elapsed().as_millis()
        );
        state.completion_advances.publish(advance);
    }
}

pub(crate) struct CompletionAdvanceSubscriber {
    inner: ObservationSubscriber,
}

pub(crate) struct ChatBusinessDispatchGuard {
    shared: ChatObservationShared,
}
impl Drop for ChatBusinessDispatchGuard {
    fn drop(&mut self) {
        match self.shared.state.lock() {
            Ok(mut state) => {
                state.business_dispatches = state.business_dispatches.saturating_sub(1);
                self.shared.dispatch_changed.notify_all();
            }
            Err(_) => log::error!("释放普通聊天派发租约失败"),
        }
    }
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
mod dispatch_lease_tests {
    use super::*;
    use crate::observation::chat::{ChatTimeline, RecognizedChatMessage};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[test]
    fn decision_waits_for_prior_dispatch_and_reservation_blocks_new_dispatch() {
        let shared = ChatObservationShared::new();
        let dispatch = shared.try_begin_business_dispatch().unwrap().unwrap();
        let worker = shared.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (end_tx, end_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let exclusive = worker
                .begin_exclusive_until(Instant::now() + Duration::from_secs(2), || false)
                .unwrap();
            ready_tx.send(()).unwrap();
            end_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(exclusive);
        });
        {
            let mut state = shared.state.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !state.exclusive_requested {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "exclusive reservation did not arrive");
                state = shared
                    .dispatch_changed
                    .wait_timeout(state, remaining)
                    .unwrap()
                    .0;
            }
        }
        assert!(shared.try_begin_business_dispatch().unwrap().is_none());
        assert!(matches!(
            ready_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(dispatch);
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(shared.try_begin_business_dispatch().unwrap().is_none());
        end_tx.send(()).unwrap();
        thread.join().unwrap();
        assert!(shared.try_begin_business_dispatch().unwrap().is_some());
    }

    #[test]
    fn timeout_and_cancellation_release_reservation_without_interrupting_business() {
        let shared = ChatObservationShared::new();
        let prior = shared.try_begin_business_dispatch().unwrap().unwrap();
        assert!(
            shared
                .begin_exclusive_until(Instant::now() + Duration::from_millis(10), || false)
                .is_err()
        );
        assert!(shared.try_begin_business_dispatch().unwrap().is_some());
        let cancelled = AtomicBool::new(true);
        assert!(
            shared
                .begin_exclusive_until(Instant::now() + Duration::from_secs(1), || cancelled
                    .load(Ordering::SeqCst))
                .is_err()
        );
        assert!(shared.try_begin_business_dispatch().unwrap().is_some());
        drop(prior);
        let exclusive = shared.begin_exclusive().unwrap();
        assert!(
            shared.begin_exclusive().is_err(),
            "nested failure cannot release active ownership"
        );
        assert!(shared.try_begin_business_dispatch().unwrap().is_none());
        drop(exclusive);
        assert!(shared.try_begin_business_dispatch().unwrap().is_some());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
