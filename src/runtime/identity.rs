use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

#[derive(Clone, Debug)]
pub struct BusinessOperationIdAllocator {
    next: Arc<AtomicU64>,
}

impl BusinessOperationIdAllocator {
    pub fn new() -> Self {
        Self {
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn allocate(&self) -> Result<BusinessOperationId, BusinessOperationIdExhausted> {
        self.next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| match next {
                0 => None,
                u64::MAX => Some(0),
                _ => Some(next + 1),
            })
            .map(BusinessOperationId::new)
            .map_err(|_| BusinessOperationIdExhausted)
    }

    #[cfg(test)]
    fn with_next_for_test(next: u64) -> Self {
        Self {
            next: Arc::new(AtomicU64::new(next)),
        }
    }
}

impl Default for BusinessOperationIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusinessOperationIdExhausted;

impl Display for BusinessOperationIdExhausted {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("business operation identifiers are exhausted")
    }
}

impl std::error::Error for BusinessOperationIdExhausted {}

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
    use super::{BusinessOperationId, BusinessOperationIdAllocator, SessionGeneration};

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

    #[test]
    fn operation_allocator_is_shared_by_all_clones() {
        let allocator = BusinessOperationIdAllocator::new();
        let clone = allocator.clone();

        assert_eq!(allocator.allocate().unwrap(), BusinessOperationId::new(1));
        assert_eq!(clone.allocate().unwrap(), BusinessOperationId::new(2));
        assert_eq!(allocator.allocate().unwrap(), BusinessOperationId::new(3));
    }

    #[test]
    fn operation_allocator_never_wraps_after_the_last_identifier() {
        let allocator = BusinessOperationIdAllocator::with_next_for_test(u64::MAX);

        assert_eq!(
            allocator.allocate().unwrap(),
            BusinessOperationId::new(u64::MAX)
        );
        assert!(allocator.allocate().is_err());
        assert!(allocator.allocate().is_err());
    }
}
