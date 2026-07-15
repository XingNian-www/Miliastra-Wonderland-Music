use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Supplies monotonic business time without coupling domain code to the system clock.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> Instant {
        C::now(self)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A cloneable monotonic clock for deterministic tests.
///
/// Clones observe and advance the same instant. The mutex makes concurrent advances additive
/// rather than allowing one test driver to overwrite another driver's progress.
#[derive(Clone, Debug)]
pub struct ManualClock {
    now: Arc<Mutex<Instant>>,
}

impl ManualClock {
    pub fn new(now: Instant) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }

    pub fn advance(&self, duration: Duration) -> Result<Instant, ManualClockError> {
        let mut now = self.lock();
        let advanced = now
            .checked_add(duration)
            .ok_or(ManualClockError::InstantOverflow)?;
        *now = advanced;
        Ok(advanced)
    }

    pub fn advance_to(&self, target: Instant) -> Result<Instant, ManualClockError> {
        let mut now = self.lock();
        if target < *now {
            return Err(ManualClockError::WouldMoveBackwards);
        }
        *now = target;
        Ok(target)
    }

    fn lock(&self) -> MutexGuard<'_, Instant> {
        self.now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.lock()
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

    use super::{Clock, ManualClock, ManualClockError};

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
}
