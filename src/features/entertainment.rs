use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntertainmentKind {
    IdiomChain,
    Landlord,
    RunFast,
    TurtleSoup,
    Undercover,
}

impl EntertainmentKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::IdiomChain => "成语接龙",
            Self::Landlord => "斗地主",
            Self::RunFast => "跑得快",
            Self::TurtleSoup => "海龟汤",
            Self::Undercover => "谁是卧底",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcquireOutcome {
    Acquired,
    AlreadyOwned,
    Occupied(EntertainmentKind),
}

#[derive(Clone, Default)]
pub(crate) struct EntertainmentCoordinator {
    active: Arc<Mutex<Option<EntertainmentKind>>>,
}

impl EntertainmentCoordinator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn try_acquire(&self, kind: EntertainmentKind) -> Result<AcquireOutcome> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| anyhow!("娱乐模块互斥锁已损坏"))?;
        match *active {
            None => {
                *active = Some(kind);
                Ok(AcquireOutcome::Acquired)
            }
            Some(current) if current == kind => Ok(AcquireOutcome::AlreadyOwned),
            Some(current) => Ok(AcquireOutcome::Occupied(current)),
        }
    }

    pub(crate) fn release(&self, kind: EntertainmentKind) {
        match self.active.lock() {
            Ok(mut active) if *active == Some(kind) => *active = None,
            Ok(_) => {}
            Err(_) => log::error!("娱乐模块互斥锁已损坏"),
        }
    }

    pub(crate) fn active(&self) -> Option<EntertainmentKind> {
        self.active.lock().ok().and_then(|active| *active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_entertainment_module_can_be_active() {
        let coordinator = EntertainmentCoordinator::new();

        assert_eq!(
            coordinator
                .try_acquire(EntertainmentKind::IdiomChain)
                .unwrap(),
            AcquireOutcome::Acquired
        );
        assert_eq!(
            coordinator
                .try_acquire(EntertainmentKind::IdiomChain)
                .unwrap(),
            AcquireOutcome::AlreadyOwned
        );
        assert_eq!(
            coordinator
                .try_acquire(EntertainmentKind::TurtleSoup)
                .unwrap(),
            AcquireOutcome::Occupied(EntertainmentKind::IdiomChain)
        );

        coordinator.release(EntertainmentKind::IdiomChain);
        assert_eq!(coordinator.active(), None);
    }
}
