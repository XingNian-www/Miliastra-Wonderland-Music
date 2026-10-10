//! 调试事件推送：把"等状态变化"从轮询改成服务端推送。
//!
//! 设计边界：
//! - 事件总线只负责**排序、缓存和分发**，不认识任何业务类型；payload 是 JSON。
//! - 每个事件有单调递增的 sequence，同时作为 SSE 的 id；
//!   客户端断线重连时带上来，就能拿回断线期间的事件，不会"漏一拍"。
//! - 没有订阅者时事件照样进重放缓冲：@历史与"当下有没有人在听"无关@。
//!
//! 当前发布点只覆盖调试与沙盒层（调试模式、运行生命周期、直接命令、决策提交）。
//! 应用内部的状态变化（播放进度、大厅计时等）**尚未**推送，仍需轮询查询接口。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

/// 重放缓冲容量：客户端带 Last-Event-ID 重连时用它补齐断线期间的事件。
const REPLAY_CAPACITY: usize = 256;
/// 广播通道容量。慢客户端落后超过这个数会收到 Lagged，届时只能依赖重放或重新同步。
const BROADCAST_CAPACITY: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DebugEvent {
    /// 单调递增序号，同时作为 SSE 的 id。
    pub sequence: u64,
    /// 事件类型，例如 run.finished、debug.mode。
    pub kind: String,
    pub payload: serde_json::Value,
    pub at_ms: u64,
}

pub(crate) struct DebugEventBus {
    sender: broadcast::Sender<DebugEvent>,
    sequence: AtomicU64,
    replay: Mutex<VecDeque<DebugEvent>>,
}

impl Default for DebugEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugEventBus {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            sender,
            sequence: AtomicU64::new(0),
            replay: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<DebugEvent> {
        self.sender.subscribe()
    }

    /// 发布一个事件并返回它。没有订阅者时也不会失败：事件仍然进重放缓冲。
    pub(crate) fn publish(
        &self,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> DebugEvent {
        // Sequence allocation, replay and broadcast share the same publication order.
        let mut replay = self
            .replay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = DebugEvent {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst) + 1,
            kind: kind.into(),
            payload,
            at_ms: now_ms(),
        };
        replay.push_back(event.clone());
        while replay.len() > REPLAY_CAPACITY {
            replay.pop_front();
        }
        // 无订阅者时 send 会返回 Err，这是正常情况，不是错误。
        let _ = self.sender.send(event.clone());
        event
    }

    /// Atomically join replay and live delivery: no gap or duplicated boundary event.
    pub(crate) fn subscribe_replay_after(
        &self,
        since: u64,
    ) -> (Vec<DebugEvent>, broadcast::Receiver<DebugEvent>) {
        let replay = self
            .replay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let receiver = self.sender.subscribe();
        let events = replay
            .iter()
            .filter(|event| event.sequence > since)
            .cloned()
            .collect();
        (events, receiver)
    }

    /// 序号严格大于 since 的缓存事件，供断线重连补齐。
    pub(crate) fn replay_after(&self, since: u64) -> Vec<DebugEvent> {
        self.replay
            .lock()
            .map(|replay| {
                replay
                    .iter()
                    .filter(|event| event.sequence > since)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn latest_sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 进程内全局总线。与 debug_mode 的全局状态保持一致的用法。
pub(crate) fn bus() -> &'static DebugEventBus {
    static BUS: OnceLock<DebugEventBus> = OnceLock::new();
    BUS.get_or_init(DebugEventBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_assigns_gap_free_monotonic_sequences() {
        let bus = DebugEventBus::new();
        assert_eq!(bus.latest_sequence(), 0, "新总线必须从 0 开始");
        let first = bus.publish("a", serde_json::json!({}));
        let second = bus.publish("b", serde_json::json!({}));
        let third = bus.publish("c", serde_json::json!({}));
        assert_eq!((first.sequence, second.sequence, third.sequence), (1, 2, 3));
        assert_eq!(bus.latest_sequence(), 3);
        // 类型和载荷必须原样保留，不能在这里被改写。
        assert_eq!(second.kind, "b");
    }

    #[test]
    fn concurrent_publishers_keep_replay_and_live_in_sequence_order() {
        let bus = std::sync::Arc::new(DebugEventBus::new());
        let (replay, mut receiver) = bus.subscribe_replay_after(0);
        assert!(replay.is_empty());
        let workers = (0..8)
            .map(|_| {
                let bus = bus.clone();
                std::thread::spawn(move || {
                    for _ in 0..32 {
                        bus.publish("parallel", serde_json::json!({}));
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let live = (1..=256)
            .map(|expected| {
                let event = receiver.try_recv().unwrap();
                assert_eq!(event.sequence, expected);
                event
            })
            .collect::<Vec<_>>();
        assert_eq!(live, bus.replay_after(0));
        let (replay, mut receiver) = bus.subscribe_replay_after(250);
        assert_eq!(replay.len(), 6);
        assert!(receiver.try_recv().is_err());
        let last = bus.publish("after-boundary", serde_json::json!({}));
        assert_eq!(receiver.try_recv().unwrap(), last);
    }

    #[test]
    fn replay_after_returns_only_strictly_newer_events() {
        let bus = DebugEventBus::new();
        for index in 0..5 {
            bus.publish(format!("e{index}"), serde_json::json!(index));
        }
        let replay = bus.replay_after(2);
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4, 5],
            "必须严格大于 since，否则重连会重复投递边界事件"
        );
        assert!(bus.replay_after(5).is_empty());
        assert_eq!(bus.replay_after(0).len(), 5);
    }

    #[test]
    fn replay_buffer_is_bounded_and_drops_oldest_first() {
        let bus = DebugEventBus::new();
        for index in 0..(REPLAY_CAPACITY + 10) {
            bus.publish("e", serde_json::json!(index));
        }
        let replay = bus.replay_after(0);
        assert_eq!(replay.len(), REPLAY_CAPACITY, "重放缓冲必须有上界");
        assert_eq!(
            replay.first().map(|event| event.sequence),
            Some(11),
            "应当丢弃最旧的事件，保留最近的一批"
        );
        assert_eq!(bus.latest_sequence(), (REPLAY_CAPACITY + 10) as u64);
    }

    #[tokio::test]
    async fn subscribers_receive_only_events_published_after_subscribing() {
        let bus = DebugEventBus::new();
        bus.publish("before", serde_json::json!({}));
        let mut receiver = bus.subscribe();
        bus.publish("after", serde_json::json!({"n": 1}));
        let received = receiver.recv().await.expect("应当收到订阅后的事件");
        assert_eq!(received.kind, "after");
        assert_eq!(received.sequence, 2);
        assert_eq!(received.payload["n"], 1);
    }
}
