use std::fmt::{Display, Formatter};

/// Identifies one asynchronous operation submitted by the business runtime.
///
/// The identifier is deliberately separate from runtime-specific identifiers such as
/// `UiOperationId`, so a delayed result cannot accidentally be correlated by queue position or
/// text content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusinessOperationId(u64);

impl BusinessOperationId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl Display for BusinessOperationId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Identifies the current incarnation of a business session.
///
/// Ending and restarting a game advances its generation. Results from an older generation can
/// then be rejected even if their operation identifier is otherwise valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionGeneration(u64);

impl SessionGeneration {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl Display for SessionGeneration {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::{BusinessOperationId, SessionGeneration};

    #[test]
    fn identities_do_not_wrap() {
        assert_eq!(
            BusinessOperationId::new(41).checked_next(),
            Some(BusinessOperationId::new(42))
        );
        assert_eq!(BusinessOperationId::new(u64::MAX).checked_next(), None);
        assert_eq!(
            SessionGeneration::INITIAL.checked_next(),
            Some(SessionGeneration::new(1))
        );
        assert_eq!(SessionGeneration::new(u64::MAX).checked_next(), None);
    }
}
