use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

use super::shared::{
    ObservationRead, ObservationSequence, ObservationSubscriber, SharedObservationStream,
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
        // An exclusive reader owns its decision baseline, but ordinary chat
        // commands must continue through the shared stream while it waits.
        RoutedObservation::Shared(self.shared.publish(value))
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_session_keeps_shared_stream_live_without_a_gap() {
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
        let private_sequence = match router.route("private") {
            RoutedObservation::Shared(sequence) => sequence,
            RoutedObservation::Exclusive { .. } => panic!("shared observation was isolated"),
        };
        assert_eq!(
            router.read_next(&mut subscriber),
            Some(ObservationRead::item(private_sequence, "private"))
        );

        router.finish_exclusive(session).unwrap();
        assert_eq!(router.read_next(&mut subscriber), None);

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
