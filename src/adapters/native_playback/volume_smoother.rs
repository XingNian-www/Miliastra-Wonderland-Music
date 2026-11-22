//! 音量渐变的**代际 worker**。
//!
//! 旧实现每条音量命令都起一条 `volume-smooth` 线程，靠代际号让旧线程提前退出；
//! 命令密集时线程数随命令数增长。这里改为一条常驻 worker：新目标只更新代际与
//! 待办，线程始终只有一条；正在进行的渐变发现代际变化就放弃，立即改做最新目标。
//!
//! worker 只持 [`Weak`]，最后一个 adapter 释放后由空闲轮询自行退出，不留常驻线程。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::Duration;

use super::volume_smooth_sequence;

/// 空闲轮询：既用于发现新目标，也用于在 adapter 全部释放后让 worker 退出。
const IDLE_POLL: Duration = Duration::from_millis(200);

pub(super) struct VolumeSmoother {
    generation: AtomicU64,
    step: Duration,
    state: Mutex<SmootherState>,
    ready: Condvar,
    apply: Arc<dyn Fn(u8) -> bool + Send + Sync>,
    read_current: Arc<dyn Fn() -> Option<u8> + Send + Sync>,
}

#[derive(Default)]
struct SmootherState {
    pending: Option<Target>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct Target {
    volume: u8,
    generation: u64,
}

impl VolumeSmoother {
    pub(super) fn new(
        step_ms: u64,
        apply: impl Fn(u8) -> bool + Send + Sync + 'static,
        read_current: impl Fn() -> Option<u8> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(0),
            step: Duration::from_millis(step_ms),
            state: Mutex::new(SmootherState::default()),
            ready: Condvar::new(),
            apply: Arc::new(apply),
            read_current: Arc::new(read_current),
        })
    }

    /// 设定新目标：只更新代际与待办；线程只在首次调用时创建一次。
    pub(super) fn request(self: &Arc<Self>, volume: u8) -> std::io::Result<()> {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut state = self.lock();
        state.pending = Some(Target { volume, generation });
        self.ready.notify_all();
        if state.worker.is_some() {
            return Ok(());
        }
        let worker = Arc::downgrade(self);
        let handle = thread::Builder::new()
            .name("volume-smooth".to_owned())
            .spawn(move || run(worker))?;
        state.worker = Some(handle);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, SmootherState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn run(weak: Weak<VolumeSmoother>) {
    loop {
        let target = {
            let Some(smoother) = weak.upgrade() else {
                return;
            };
            let mut state = smoother.lock();
            loop {
                if let Some(target) = state.pending.take() {
                    break target;
                }
                let (next, waited) = match smoother.ready.wait_timeout(state, IDLE_POLL) {
                    Ok(waited) => waited,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state = next;
                // 只有 worker 自己还持有 Arc：所有 adapter 已释放，退出。
                if waited.timed_out() && weak.strong_count() == 1 {
                    return;
                }
            }
        };
        smooth(&weak, target);
    }
}

fn smooth(weak: &Weak<VolumeSmoother>, target: Target) {
    let Some(smoother) = weak.upgrade() else {
        return;
    };
    let current = (smoother.read_current)().unwrap_or(target.volume);
    for next in volume_smooth_sequence(current, target.volume) {
        if smoother.generation.load(Ordering::Relaxed) != target.generation {
            // 已有更新的音量命令接管渐变。
            return;
        }
        if !(smoother.apply)(next) {
            // 引擎拒绝本次设置：与旧实现一致，停止这一轮渐变。
            return;
        }
        // 步进等待可被新目标立刻打断（代际变化后下一轮即生效）。
        let state = smoother.lock();
        let (state, _) = match smoother.ready.wait_timeout(state, smoother.step) {
            Ok(waited) => waited,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(state);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::VolumeSmoother;

    /// Linux `/proc/self/task/<tid>/comm` 里可读的线程名。
    fn count_named(name: &str) -> usize {
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return 0;
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
            .filter(|thread| thread.trim() == name)
            .count()
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    /// 连续多次音量目标只使用**一条** `volume-smooth` worker；
    /// 中途改变目标时旧渐变被代际打断，不会出现两条线程抢着写音量。
    #[test]
    fn volume_smooth_uses_one_worker() {
        const NAME: &str = "volume-smooth";
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicBool::new(false));
        let current = Arc::new(Mutex::new(0u8));
        let applied = Arc::new(Mutex::new(Vec::<u8>::new()));

        let apply_gate = Arc::clone(&gate);
        let apply_started = Arc::clone(&started);
        let apply_current = Arc::clone(&current);
        let apply_targets = Arc::clone(&applied);
        let read_current = Arc::clone(&current);
        let smoother = VolumeSmoother::new(
            1,
            move |volume| {
                apply_started.store(true, Ordering::Release);
                let (released, changed) = &*apply_gate;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                drop(released);
                *apply_current.lock().unwrap() = volume;
                apply_targets.lock().unwrap().push(volume);
                true
            },
            move || Some(*read_current.lock().unwrap()),
        );

        smoother.request(100).expect("first request");
        assert!(
            wait_until(|| started.load(Ordering::Acquire)),
            "第一档音量应当很快开始"
        );
        for _ in 0..19 {
            smoother.request(20).expect("burst request");
        }
        assert_eq!(
            count_named(NAME),
            1,
            "20 次音量目标只允许一条 volume-smooth worker"
        );

        // 放行被卡住的第一档：代际已变化，旧渐变必须放弃并改做 20。
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert!(
            wait_until(|| applied.lock().unwrap().last() == Some(&20)),
            "最新目标必须生效：{:?}",
            applied.lock().unwrap()
        );
        let applied = applied.lock().unwrap().clone();
        assert!(
            !applied.contains(&100),
            "目标 100 的代际已被新命令取代，不应该走完：{applied:?}"
        );
        assert_eq!(count_named(NAME), 1, "突发目标不得新增 worker 线程");

        drop(smoother);
        assert!(
            wait_until(|| count_named(NAME) == 0),
            "adapter 释放后 volume-smooth worker 应自行退出"
        );
    }
}
