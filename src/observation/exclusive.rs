use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

use super::shared::{
    ObservationGapKind, ObservationRead, ObservationSequence, ObservationSubscriber,
    SharedObservationStream,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExclusiveSessionId(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutedObservation<T> {
    Shared(ObservationSequence),
    Exclusive {
        session: ExclusiveSessionId,
        value: T,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusiveSessionError {
    AlreadyActive,
    NotActive,
    StaleSession,
}

impl fmt::Display for ExclusiveSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::AlreadyActive => "an exclusive observation session is already active",
            Self::NotActive => "no exclusive observation session is active",
            Self::StaleSession => "the exclusive observation session is stale",
        };
        formatter.write_str(message)
    }
}

impl Error for ExclusiveSessionError {}

pub struct ExclusiveObservationRouter<T> {
    shared: SharedObservationStream<T>,
    active: Option<ExclusiveSessionId>,
    next_session: u64,
}

impl<T> ExclusiveObservationRouter<T> {
    pub fn new(shared_capacity: NonZeroUsize) -> Self {
        Self {
            shared: SharedObservationStream::new(shared_capacity),
            active: None,
            next_session: 1,
        }
    }

    pub fn subscribe(&self) -> ObservationSubscriber {
        self.shared.subscribe()
    }

    pub fn read_next(&self, subscriber: &mut ObservationSubscriber) -> Option<ObservationRead<T>> {
        subscriber.read_next(&self.shared)
    }

    pub fn route(&mut self, value: T) -> RoutedObservation<T> {
        match self.active {
            Some(session) => RoutedObservation::Exclusive { session, value },
            None => RoutedObservation::Shared(self.shared.publish(value)),
        }
    }

    pub fn begin_exclusive(&mut self) -> Result<ExclusiveSessionId, ExclusiveSessionError> {
        if self.active.is_some() {
            return Err(ExclusiveSessionError::AlreadyActive);
        }
        let session = ExclusiveSessionId(self.next_session);
        self.next_session = self
            .next_session
            .checked_add(1)
            .expect("exclusive observation session sequence exhausted");
        self.active = Some(session);
        Ok(session)
    }

    pub fn finish_exclusive(
        &mut self,
        session: ExclusiveSessionId,
    ) -> Result<(), ExclusiveSessionError> {
        match self.active {
            None => return Err(ExclusiveSessionError::NotActive),
            Some(active) if active != session => return Err(ExclusiveSessionError::StaleSession),
            Some(_) => {}
        }
        self.active = None;
        self.shared.mark_gap(ObservationGapKind::ExclusiveSession);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_session_keeps_private_text_out_of_the_shared_stream_and_leaves_a_gap() {
        let mut router = ExclusiveObservationRouter::new(NonZeroUsize::new(4).unwrap());
        let mut subscriber = router.subscribe();

        let first = match router.route("shared-before") {
            RoutedObservation::Shared(sequence) => sequence,
            RoutedObservation::Exclusive { .. } => panic!("shared observation was isolated"),
        };
        assert_eq!(
            router.read_next(&mut subscriber),
            Some(ObservationRead::item(first, "shared-before"))
        );

        let session = router.begin_exclusive().unwrap();
        assert_eq!(
            router.route("private"),
            RoutedObservation::Exclusive {
                session,
                value: "private",
            }
        );
        assert_eq!(router.read_next(&mut subscriber), None);

        router.finish_exclusive(session).unwrap();
        let Some(ObservationRead::Gap(gap)) = router.read_next(&mut subscriber) else {
            panic!("exclusive session did not produce a shared observation gap");
        };
        assert_eq!(gap.kind, ObservationGapKind::ExclusiveSession);

        let after = match router.route("shared-after") {
            RoutedObservation::Shared(sequence) => sequence,
            RoutedObservation::Exclusive { .. } => panic!("shared observation was isolated"),
        };
        assert_eq!(
            router.read_next(&mut subscriber),
            Some(ObservationRead::item(after, "shared-after"))
        );
    }
}
