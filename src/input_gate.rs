use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{RwLock, RwLockReadGuard};

#[derive(Clone, Copy, Default)]
struct Mode {
    enabled: bool,
    generation: u64,
}

struct Isolation(RwLock<Mode>);
impl Isolation {
    const fn new() -> Self {
        Self(RwLock::new(Mode {
            enabled: false,
            generation: 0,
        }))
    }
    fn snapshot(&self) -> Mode {
        *self.0.read().unwrap_or_else(|e| e.into_inner())
    }
    fn set_enabled(&self, enabled: bool) -> bool {
        // The write lock is an execution barrier: every admitted routine has finished.
        let mut mode = self.0.write().unwrap_or_else(|e| e.into_inner());
        let previous = mode.enabled;
        if previous != enabled {
            mode.enabled = enabled;
            mode.generation = mode.generation.wrapping_add(1);
        }
        previous
    }
    fn enter(&self, generation: u64) -> Option<RwLockReadGuard<'_, Mode>> {
        let mode = self.0.read().unwrap_or_else(|e| e.into_inner());
        (mode.generation == generation).then_some(mode)
    }
}
static ISOLATION: Isolation = Isolation::new();

/// 串行化会改全局 `ISOLATION` 的测试。
///
/// `ISOLATION` 是进程级 RwLock，`generation` 是全局计数：任何测试调用
/// `set_enabled` 都会递增它，让**其他测试**排队中的 operation 被判 stale 后
/// 直接丢响应（表现为 `UiWaitTimeoutError { disconnected: true }`，
/// 或断言里拿到空结果）。这不是产品缺陷，是测试之间共享进程状态互相干扰。
///
/// 凡是用 `UiRuntime` 或调试模式开关的测试都必须先拿这把锁。
#[cfg(test)]
pub(crate) fn isolation_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // 前一个测试 panic 会毒化锁；这里只做互斥，不读被保护数据，可安全恢复。
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
thread_local! {
    static EXTERNAL: Cell<bool> = const { Cell::new(false) };
    static EXECUTION_INPUT: Cell<Option<bool>> = const { Cell::new(None) };
    static EXECUTION_MODE: Cell<Option<Mode>> = const { Cell::new(None) };
}
pub(crate) fn enabled() -> bool {
    EXECUTION_MODE
        .with(Cell::get)
        .unwrap_or_else(|| ISOLATION.snapshot())
        .enabled
}
pub(crate) fn set_enabled(value: bool) -> bool {
    ISOLATION.set_enabled(value)
}
pub(crate) fn generation() -> u64 {
    EXECUTION_MODE
        .with(Cell::get)
        .unwrap_or_else(|| ISOLATION.snapshot())
        .generation
}
pub(crate) fn external_allowed() -> bool {
    EXTERNAL.with(Cell::get)
}
pub(crate) fn input_allowed() -> bool {
    EXECUTION_INPUT
        .with(Cell::get)
        .unwrap_or_else(|| !enabled() || external_allowed())
}
pub(crate) fn require_input() -> anyhow::Result<()> {
    anyhow::ensure!(
        input_allowed(),
        "调试模式已禁用内部游戏输入，请通过明确授权的外部 API 操作"
    );
    Ok(())
}

/// Acquire on the UI worker, never on a caller waiting for that worker.
/// A stale queued operation is discarded, including external operations.
pub(crate) fn execution_guard(generation: u64, external: bool) -> Option<ExecutionGuard<'static>> {
    let mode = ISOLATION.enter(generation)?;
    Some(ExecutionGuard::new(mode, external))
}
pub(crate) struct ExecutionGuard<'a> {
    _mode: RwLockReadGuard<'a, Mode>,
    previous: Option<bool>,
    previous_mode: Option<Mode>,
    _thread: PhantomData<Rc<()>>,
}
impl<'a> ExecutionGuard<'a> {
    fn new(mode: RwLockReadGuard<'a, Mode>, external: bool) -> Self {
        let previous = EXECUTION_INPUT.with(|flag| flag.replace(Some(!mode.enabled || external)));
        let previous_mode = EXECUTION_MODE.with(|flag| flag.replace(Some(*mode)));
        Self {
            _mode: mode,
            previous,
            previous_mode,
            _thread: PhantomData,
        }
    }
}
impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        EXECUTION_INPUT.with(|flag| flag.set(self.previous));
        EXECUTION_MODE.with(|flag| flag.set(self.previous_mode));
    }
}

#[cfg(test)]
pub(crate) fn with_test_execution<T>(enabled: bool, external: bool, run: impl FnOnce() -> T) -> T {
    let isolation = Isolation::new();
    isolation.set_enabled(enabled);
    let _guard = ExecutionGuard::new(
        isolation.enter(isolation.snapshot().generation).unwrap(),
        external,
    );
    run()
}

/// Synchronous caller-side compatibility only; this does not authorize queued routines.
pub(crate) fn external_guard() -> ExternalGuard {
    ExternalGuard::enter()
}
pub(crate) struct ExternalGuard(bool, PhantomData<Rc<()>>);
impl ExternalGuard {
    pub(crate) fn enter() -> Self {
        Self(EXTERNAL.with(|flag| flag.replace(true)), PhantomData)
    }
}
impl Drop for ExternalGuard {
    fn drop(&mut self) {
        EXTERNAL.with(|flag| flag.set(self.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn mode_switch_invalidates_old_queue_even_after_round_trip() {
        let isolation = Isolation::new();
        let generation = isolation.snapshot().generation;
        assert!(!isolation.set_enabled(true));
        assert!(isolation.enter(generation).is_none());
        assert!(isolation.set_enabled(false));
        assert!(isolation.enter(generation).is_none());
        assert!(isolation.enter(isolation.snapshot().generation).is_some());
    }

    #[test]
    fn mode_switch_waits_for_active_execution() {
        let isolation = Arc::new(Isolation::new());
        let guard = isolation.enter(0).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let other = isolation.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            other.set_enabled(true);
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(isolation.snapshot().enabled);
    }

    #[test]
    fn execution_authorization_overrides_caller_guard_and_restores() {
        // Thread-local state only: never change the process mode in parallel tests.
        let previous = EXECUTION_INPUT.with(|flag| flag.replace(Some(false)));
        {
            let _external = external_guard();
            assert!(!input_allowed());
        }
        EXECUTION_INPUT.with(|flag| flag.set(previous));
        assert!(!external_allowed());
    }
}
