use anyhow::Result;

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

#[derive(Debug, Default)]
pub(crate) struct EntertainmentState {
    active: Option<EntertainmentKind>,
}

impl EntertainmentState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn try_acquire(&mut self, kind: EntertainmentKind) -> Result<AcquireOutcome> {
        match self.active {
            Some(current) if current == kind => Ok(AcquireOutcome::AlreadyOwned),
            Some(current) => Ok(AcquireOutcome::Occupied(current)),
            None => {
                self.active = Some(kind);
                Ok(AcquireOutcome::Acquired)
            }
        }
    }

    pub(crate) fn release(&mut self, kind: EntertainmentKind) {
        if self.active == Some(kind) {
            self.active = None;
        }
    }

    pub(crate) fn active(&self) -> Option<EntertainmentKind> {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_entertainment_module_can_be_active() {
        let mut state = EntertainmentState::new();

        assert_eq!(
            state.try_acquire(EntertainmentKind::IdiomChain).unwrap(),
            AcquireOutcome::Acquired
        );
        assert_eq!(
            state.try_acquire(EntertainmentKind::IdiomChain).unwrap(),
            AcquireOutcome::AlreadyOwned
        );
        assert_eq!(
            state.try_acquire(EntertainmentKind::TurtleSoup).unwrap(),
            AcquireOutcome::Occupied(EntertainmentKind::IdiomChain)
        );

        state.release(EntertainmentKind::IdiomChain);
        assert_eq!(state.active(), None);
    }
}
