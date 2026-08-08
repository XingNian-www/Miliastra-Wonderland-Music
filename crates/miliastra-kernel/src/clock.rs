use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Supplies monotonic business time without coupling domain code to the system clock.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// Waits without forcing a business gateway to depend directly on the system scheduler.
pub trait Delay: Send + Sync + 'static {
    fn wait(&self, duration: Duration);
}

/// Supplies wall-clock metadata without using it to judge business deadlines.
pub trait WallClock: Send + Sync + 'static {
    fn unix_seconds(&self) -> u64;

    fn unix_millis(&self) -> u64 {
        self.unix_seconds().saturating_mul(1_000)
    }
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> Instant {
        C::now(self)
    }
}

impl<D: Delay + ?Sized> Delay for Arc<D> {
    fn wait(&self, duration: Duration) {
        D::wait(self, duration);
    }
}

impl<W: WallClock + ?Sized> WallClock for Arc<W> {
    fn unix_seconds(&self) -> u64 {
        W::unix_seconds(self)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl Delay for SystemClock {
    fn wait(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

impl WallClock for SystemClock {
    fn unix_seconds(&self) -> u64 {
        self.unix_millis() / 1_000
    }

    fn unix_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0)
    }
}

/// A cloneable monotonic clock for deterministic tests.
///
/// Clones observe and advance the same instant. The mutex makes concurrent advances additive
/// rather than allowing one test driver to overwrite another driver's progress.
#[derive(Clone, Debug)]
pub struct ManualClock {
    time: Arc<Mutex<ManualTime>>,
}

#[derive(Debug)]
struct ManualTime {
    monotonic: Instant,
    unix_millis: u64,
}

impl ManualClock {
    pub fn new(now: Instant) -> Self {
        Self::with_unix_seconds(now, 0)
    }

    pub fn with_unix_seconds(now: Instant, unix_seconds: u64) -> Self {
        Self {
            time: Arc::new(Mutex::new(ManualTime {
                monotonic: now,
                unix_millis: unix_seconds.saturating_mul(1_000),
            })),
        }
    }

    pub fn advance(&self, duration: Duration) -> Result<Instant, ManualClockError> {
        let mut time = self.lock();
        let advanced = time
            .monotonic
            .checked_add(duration)
            .ok_or(ManualClockError::InstantOverflow)?;
        time.monotonic = advanced;
        let elapsed_ms = duration.as_millis().min(u64::MAX as u128) as u64;
        time.unix_millis = time.unix_millis.saturating_add(elapsed_ms);
        Ok(advanced)
    }

    pub fn advance_to(&self, target: Instant) -> Result<Instant, ManualClockError> {
        let mut time = self.lock();
        if target < time.monotonic {
            return Err(ManualClockError::WouldMoveBackwards);
        }
        let elapsed = target.duration_since(time.monotonic);
        time.monotonic = target;
        let elapsed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        time.unix_millis = time.unix_millis.saturating_add(elapsed_ms);
        Ok(target)
    }

    fn lock(&self) -> MutexGuard<'_, ManualTime> {
        self.time
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.lock().monotonic
    }
}

impl WallClock for ManualClock {
    fn unix_seconds(&self) -> u64 {
        self.lock().unix_millis / 1_000
    }

    fn unix_millis(&self) -> u64 {
        self.lock().unix_millis
    }
}

impl Delay for ManualClock {
    fn wait(&self, duration: Duration) {
        let _ = self.advance(duration);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualClockError {
    WouldMoveBackwards,
    InstantOverflow,
}

impl Display for ManualClockError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldMoveBackwards => formatter.write_str("manual clock cannot move backwards"),
            Self::InstantOverflow => formatter.write_str("manual clock instant overflowed"),
        }
    }
}

impl Error for ManualClockError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Clock, ManualClock, ManualClockError, WallClock};

    #[test]
    fn clones_share_monotonic_time() {
        let initial = Instant::now();
        let clock = ManualClock::new(initial);
        let clone = clock.clone();

        clone.advance(Duration::from_secs(3)).unwrap();

        assert_eq!(clock.now(), initial + Duration::from_secs(3));
    }

    #[test]
    fn advance_to_rejects_backwards_time() {
        let initial = Instant::now();
        let clock = ManualClock::new(initial);
        clock.advance(Duration::from_secs(1)).unwrap();

        assert_eq!(
            clock.advance_to(initial),
            Err(ManualClockError::WouldMoveBackwards)
        );
        assert_eq!(clock.now(), initial + Duration::from_secs(1));
    }

    #[test]
    fn concurrent_advances_are_not_lost() {
        let initial = Instant::now();
        let clock = Arc::new(ManualClock::new(initial));
        let workers = (0..8)
            .map(|_| {
                let clock = Arc::clone(&clock);
                thread::spawn(move || {
                    for _ in 0..100 {
                        clock.advance(Duration::from_millis(1)).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(clock.now(), initial + Duration::from_millis(800));
    }

    #[test]
    fn manual_wall_clock_preserves_subsecond_metadata() {
        let clock = ManualClock::with_unix_seconds(Instant::now(), 100);

        clock.advance(Duration::from_millis(1_500)).unwrap();

        assert_eq!(clock.unix_millis(), 101_500);
        assert_eq!(clock.unix_seconds(), 101);
    }
}
