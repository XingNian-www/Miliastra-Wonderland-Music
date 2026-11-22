//! 进程级只读服务健康快照（W0-C）。
//!
//! 设计边界（与架构方案 W0 第 6 项一致）：
//! - **拉取式**：只被 `GET /health/services` 按需读取一次快照，不写入
//!   `MonitorEvent`。那条通道是无界队列，把心跳塞进去等于让它无限增长。
//! - **零行为改动**：埋点只写时间戳与计数器，不参与任何分支、不持有任何锁，
//!   因此不会与业务锁形成顺序依赖（全部是 `Relaxed` 原子操作）。
//! - **隐私**：快照只含固定的服务名、计数与毫秒时间戳；不含昵称、UID、
//!   令牌、文件路径。
//!
//! 已知边界（W0 刻意不做）：
//! - 没有生命周期钩子，所以识别不出「线程已退出」；`state` 只按
//!   「最近一次推进距今多久」推断，`stale` 是观测结论而非业务判定。
//! - 5 个核心 actor 今天都没有「入队/丢弃」计数器，`queue_depth` 一律为
//!   `None`（未知），`dropped` 保持 0。宁可显示未知，也不猜一个数。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 5 个核心服务名。顺序即快照顺序，也是启动序列里的先后顺序。
pub(crate) const CORE_SERVICE_NAMES: [&str; 5] =
    ["ui", "ocr", "business", "formal-task", "shared-observer"];

/// 超过该时长没有推进即视为 `stalled`（只影响快照里的 `state` 文案）。
const STALE_AFTER_MS: u64 = 5_000;

/// `queue_depth` 的内部哨兵：该服务没有可观测的队列长度。
const UNKNOWN_DEPTH: usize = usize::MAX;

/// 服务在快照里的三种可观测状态。
///
/// 只有「从未推进」「最近在推进」「很久没推进」三种——进程退出与否由
/// 上游的 supervisor（W1）判定，本模块不越权。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ServiceState {
    Idle,
    Running,
    Stalled,
}

/// 单个服务的只读快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ServiceHealth {
    pub name: &'static str,
    pub state: ServiceState,
    pub start_step: u8,
    /// 队列长度；`None` 表示该服务的队列长度不可观测（今天全部如此）。
    pub queue_depth: Option<usize>,
    pub in_flight: usize,
    pub accepted: u64,
    pub completed: u64,
    pub rejected: u64,
    pub dropped: u64,
    /// 最近一次推进的墙上时间（毫秒）；`None` 表示本次进程内从未推进。
    ///
    /// 刻意用 `Option` 而不是 0：0 会被读成 1970-01-01，和「从未推进」
    /// 是两件事。
    pub last_progress_at: Option<u64>,
}

/// 一次拉取得到的全量快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct HealthSnapshot {
    pub services: Vec<ServiceHealth>,
    pub generated_at_ms: u64,
}

/// 单个服务的计数器集合。
///
/// 全部字段都是原子量，所以整张表可以是 `static`，不需要 `OnceLock`，
/// 也不存在初始化顺序问题。所有权重都是 `Relaxed`：这些数字只用于展示，
/// 不参与任何 happens-before 判定。
pub(crate) struct ServiceSlot {
    name: &'static str,
    start_step: u8,
    queue_depth: AtomicUsize,
    in_flight: AtomicUsize,
    accepted: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
    dropped: AtomicU64,
    last_progress_at_ms: AtomicU64,
}

impl ServiceSlot {
    const fn new(name: &'static str, start_step: u8) -> Self {
        Self {
            name,
            start_step,
            queue_depth: AtomicUsize::new(UNKNOWN_DEPTH),
            in_flight: AtomicUsize::new(0),
            accepted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_progress_at_ms: AtomicU64::new(0),
        }
    }

    /// 记录一次「活着并做了一轮事」。空闲轮询（有界等待超时后 `continue`）
    /// 也算推进——这正是 `last_progress_at` 要区分的「窗口消失但进程还在」。
    pub(crate) fn touch(&self) {
        self.last_progress_at_ms
            .store(wall_clock_millis(), Ordering::Relaxed);
    }

    /// 开始处理一件工作：`accepted + 1`、`in_flight + 1`，并刷新推进时间。
    ///
    /// 返回的探针在 `Drop` 时把 `in_flight` 减回去，异常路径也不会漏减。
    pub(crate) fn enter(&'static self) -> InFlightProbe {
        self.touch();
        self.accepted.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightProbe { slot: self }
    }

    /// 记录队列长度；`None` 表示暂时不可观测。
    ///
    /// 今天没有 actor 接线，保留给「有 channel 的 actor 以后一行接上」。
    #[allow(dead_code)]
    pub(crate) fn set_queue_depth(&self, depth: Option<usize>) {
        self.queue_depth
            .store(depth.unwrap_or(UNKNOWN_DEPTH), Ordering::Relaxed);
    }

    /// 记录一件被丢弃的工作（取消、去重命中、关停收尾）。
    #[allow(dead_code)]
    pub(crate) fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn queue_depth(&self) -> Option<usize> {
        match self.queue_depth.load(Ordering::Relaxed) {
            UNKNOWN_DEPTH => None,
            depth => Some(depth),
        }
    }

    /// 从**同一次加载**的时间戳推导状态。
    ///
    /// 刻意不在这里再读一次原子量：两次读之间若有并发 `touch()`，就会出现
    /// 「`last_progress_at` 为 null 但 `state` 是 running」的自相矛盾快照。
    fn state_of(last_progress_at_ms: u64, now_ms: u64) -> ServiceState {
        match last_progress_at_ms {
            0 => ServiceState::Idle,
            last if now_ms.saturating_sub(last) > STALE_AFTER_MS => ServiceState::Stalled,
            _ => ServiceState::Running,
        }
    }

    fn snapshot_at(&self, now_ms: u64) -> ServiceHealth {
        let last_progress_at_ms = self.last_progress_at_ms.load(Ordering::Relaxed);
        ServiceHealth {
            name: self.name,
            state: Self::state_of(last_progress_at_ms, now_ms),
            start_step: self.start_step,
            queue_depth: self.queue_depth(),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            accepted: self.accepted.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            last_progress_at: (last_progress_at_ms != 0).then_some(last_progress_at_ms),
        }
    }
}

/// 一件进行中工作的探针：离开作用域即把 `in_flight` 减一。
#[must_use = "掉在这里才会计入 in_flight；结果必须显式 finish/completed/rejected"]
pub(crate) struct InFlightProbe {
    slot: &'static ServiceSlot,
}

impl InFlightProbe {
    /// 按「成功 / 失败」二选一记账后结束。
    pub(crate) fn finish(self, ok: bool) {
        if ok {
            self.slot.completed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.slot.rejected.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn completed(self) {
        self.slot.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn rejected(self) {
        self.slot.rejected.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for InFlightProbe {
    fn drop(&mut self) {
        self.slot.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 进程级健康注册表：全局一份，读多写少。
pub(crate) struct HealthRegistry {
    slots: [ServiceSlot; CORE_SERVICE_NAMES.len()],
}

static HEALTH: HealthRegistry = HealthRegistry {
    slots: [
        ServiceSlot::new(CORE_SERVICE_NAMES[0], 2),
        ServiceSlot::new(CORE_SERVICE_NAMES[1], 3),
        ServiceSlot::new(CORE_SERVICE_NAMES[2], 4),
        ServiceSlot::new(CORE_SERVICE_NAMES[3], 5),
        ServiceSlot::new(CORE_SERVICE_NAMES[4], 6),
    ],
};

pub(crate) fn ui() -> &'static ServiceSlot {
    &HEALTH.slots[0]
}

pub(crate) fn ocr() -> &'static ServiceSlot {
    &HEALTH.slots[1]
}

pub(crate) fn business() -> &'static ServiceSlot {
    &HEALTH.slots[2]
}

pub(crate) fn formal_task() -> &'static ServiceSlot {
    &HEALTH.slots[3]
}

pub(crate) fn shared_observer() -> &'static ServiceSlot {
    &HEALTH.slots[4]
}

/// 只读取一次全量快照。不写任何计数器，可被反复调用。
pub(crate) fn snapshot() -> HealthSnapshot {
    let now_ms = wall_clock_millis();
    HealthSnapshot {
        services: HEALTH
            .slots
            .iter()
            .map(|slot| slot.snapshot_at(now_ms))
            .collect(),
        generated_at_ms: now_ms,
    }
}

fn wall_clock_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 夹具专用槽：不进全局表，避免与并行测试共享计数。
    static FIXTURE_SLOT: ServiceSlot = ServiceSlot::new("fixture", 1);
    static IDLE_SLOT: ServiceSlot = ServiceSlot::new("idle-fixture", 1);
    static PROBE_SLOT: ServiceSlot = ServiceSlot::new("probe-fixture", 1);

    #[test]
    fn health_reports_five_core_services() {
        let snapshot = snapshot();
        let names: Vec<_> = snapshot
            .services
            .iter()
            .map(|service| service.name)
            .collect();
        assert_eq!(names, CORE_SERVICE_NAMES);
        assert_eq!(snapshot.services.len(), 5);
        // 启动阶段序号必须严格递增：快照要能回答「启动走到第几步」。
        let steps: Vec<_> = snapshot
            .services
            .iter()
            .map(|service| service.start_step)
            .collect();
        assert!(steps.windows(2).all(|pair| pair[0] < pair[1]), "{steps:?}");
        assert!(steps.iter().all(|step| (1..=6).contains(step)));
        assert!(snapshot.generated_at_ms > 0);
        // 从未推进的服务只能是 idle，不能是 running/stalled。
        for service in &snapshot.services {
            if service.last_progress_at.is_none() {
                assert_eq!(service.state, ServiceState::Idle, "{}", service.name);
            }
        }
    }

    #[test]
    fn last_progress_at_advances_on_idle_actor() {
        // 与 `shared_observer` 的空转循环同构：每轮只记一次推进，不做任何业务。
        assert_eq!(
            IDLE_SLOT.snapshot_at(wall_clock_millis()).state,
            ServiceState::Idle
        );
        IDLE_SLOT.touch();
        let first = IDLE_SLOT
            .snapshot_at(wall_clock_millis())
            .last_progress_at
            .expect("首次推进后必须有时间戳");
        assert_eq!(
            IDLE_SLOT.snapshot_at(wall_clock_millis()).state,
            ServiceState::Running
        );
        std::thread::sleep(Duration::from_millis(3));
        IDLE_SLOT.touch();
        let second = IDLE_SLOT
            .snapshot_at(wall_clock_millis())
            .last_progress_at
            .expect("再次推进后必须有时间戳");
        assert!(second > first, "空闲轮次也必须推进：{first} -> {second}");
        // 空转不产生任何工作计数。
        let health = IDLE_SLOT.snapshot_at(wall_clock_millis());
        assert_eq!(health.in_flight, 0);
        assert_eq!(health.accepted, 0);
        assert_eq!(health.completed, 0);
        assert_eq!(health.rejected, 0);
    }

    #[test]
    fn queue_depth_is_none_when_unknown() {
        // 默认未知：不做「猜一个 0」。
        assert_eq!(
            FIXTURE_SLOT.snapshot_at(wall_clock_millis()).queue_depth,
            None
        );
        // 接线后的形态（今天没有 actor 走这条路径）。
        FIXTURE_SLOT.set_queue_depth(Some(3));
        assert_eq!(
            FIXTURE_SLOT.snapshot_at(wall_clock_millis()).queue_depth,
            Some(3)
        );
        FIXTURE_SLOT.set_queue_depth(None);
        assert_eq!(
            FIXTURE_SLOT.snapshot_at(wall_clock_millis()).queue_depth,
            None
        );
        // 5 个核心服务今天都没有队列长度可观测。
        assert!(
            snapshot()
                .services
                .iter()
                .all(|service| service.queue_depth.is_none())
        );
    }

    #[test]
    fn in_flight_probe_balances_and_counts_outcomes() {
        let baseline = PROBE_SLOT.snapshot_at(wall_clock_millis());
        let probe = PROBE_SLOT.enter();
        let during = PROBE_SLOT.snapshot_at(wall_clock_millis());
        assert_eq!(during.in_flight, baseline.in_flight + 1);
        assert_eq!(during.accepted, baseline.accepted + 1);
        probe.finish(true);
        let after = PROBE_SLOT.snapshot_at(wall_clock_millis());
        assert_eq!(after.in_flight, baseline.in_flight);
        assert_eq!(after.completed, baseline.completed + 1);
        assert_eq!(after.rejected, baseline.rejected);
        // 失败路径同样要减回去，不能漏掉 in_flight。
        PROBE_SLOT.enter().finish(false);
        let failed = PROBE_SLOT.snapshot_at(wall_clock_millis());
        assert_eq!(failed.in_flight, baseline.in_flight);
        assert_eq!(failed.rejected, baseline.rejected + 1);
    }

    #[test]
    fn health_snapshot_json_carries_no_private_text() {
        // 隐私红线：快照里不得出现路径、令牌字段名等可回溯信息。
        let json = serde_json::to_string(&snapshot()).expect("serialize snapshot");
        for forbidden in ["/", "\\", "token", "uid", "nickname", "path"] {
            assert!(
                !json.to_ascii_lowercase().contains(forbidden),
                "快照出现敏感片段 {forbidden:?}：{json}"
            );
        }
    }
}
