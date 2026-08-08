use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::catalog::{Failure, ProviderId};

/// Process-wide lease used by the optional Windows login helper. Capturing
/// cookies is serialized across providers because WebView2 profile creation
/// and credential activation are both setup-time critical sections.
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(180);

#[derive(Clone, Debug)]
pub struct LoginCoordinator {
    active: Arc<Mutex<Option<ActiveLogin>>>,
    lease_ttl: Duration,
}

#[derive(Clone, Debug)]
struct ActiveLogin {
    id: Uuid,
    provider: ProviderId,
    expires_at: Instant,
}

#[cfg(test)]
#[derive(Debug)]
pub struct LoginLease {
    coordinator: LoginCoordinator,
    active: ActiveLogin,
}

impl LoginCoordinator {
    pub fn new() -> Self {
        Self::with_lease_ttl(DEFAULT_LEASE_TTL)
    }

    fn with_lease_ttl(lease_ttl: Duration) -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            lease_ttl,
        }
    }

    #[cfg(test)]
    pub fn begin(&self, provider: ProviderId) -> Result<LoginLease, Failure> {
        let active = self.reserve(provider)?;
        Ok(LoginLease {
            coordinator: self.clone(),
            active,
        })
    }

    /// Reserve a lease for an external helper. The returned identifier remains
    /// active until `release_id` is called, which lets the HTTP helper keep the
    /// capture lifetime across multiple requests without holding a Rust guard.
    pub fn acquire(&self, provider: ProviderId) -> Result<(Uuid, ProviderId), Failure> {
        let active = self.reserve(provider)?;
        Ok((active.id, active.provider))
    }

    pub fn release_id(&self, id: Uuid) {
        self.release(id);
    }

    pub fn owns(&self, id: Uuid, provider: ProviderId) -> bool {
        let Ok(mut active) = self.active.lock() else {
            return false;
        };
        if active
            .as_ref()
            .is_some_and(|current| current.expires_at <= Instant::now())
        {
            *active = None;
        }
        active
            .as_ref()
            .is_some_and(|current| current.id == id && current.provider == provider)
    }

    fn reserve(&self, provider: ProviderId) -> Result<ActiveLogin, Failure> {
        let mut active = self.active.lock().map_err(|_| {
            let mut failure = Failure::new(
                "login_coordinator_unavailable",
                "login lease state is unavailable",
            );
            failure.retryable = true;
            failure
        })?;
        if active
            .as_ref()
            .is_some_and(|current| current.expires_at <= Instant::now())
        {
            *active = None;
        }
        if let Some(current) = active.as_ref() {
            return Err(Failure::new(
                "login_in_progress",
                "another provider login is already active",
            )
            .with_provider(current.provider.to_string()));
        }
        let current = ActiveLogin {
            id: Uuid::new_v4(),
            provider,
            expires_at: Instant::now() + self.lease_ttl,
        };
        *active = Some(current.clone());
        Ok(current)
    }

    pub fn active(&self) -> Option<(Uuid, ProviderId)> {
        let mut active = self.active.lock().ok()?;
        if active
            .as_ref()
            .is_some_and(|current| current.expires_at <= Instant::now())
        {
            *active = None;
        }
        active
            .as_ref()
            .map(|current| (current.id, current.provider))
    }

    fn release(&self, id: Uuid) {
        if let Ok(mut active) = self.active.lock()
            && active.as_ref().is_some_and(|current| current.id == id)
        {
            *active = None;
        }
    }
}

impl Default for LoginCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Drop for LoginLease {
    fn drop(&mut self) {
        self.coordinator.release(self.active.id);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::LoginCoordinator;
    use crate::catalog::ProviderId;

    #[test]
    fn only_one_provider_login_capture_is_active() {
        let coordinator = LoginCoordinator::default();
        let lease = coordinator.begin(ProviderId::QqMusic).unwrap();
        let failure = coordinator.begin(ProviderId::Netease).unwrap_err();
        assert_eq!(failure.code, "login_in_progress");
        assert_eq!(failure.provider.as_deref(), Some("qqmusic"));
        drop(lease);
        assert!(coordinator.begin(ProviderId::Netease).is_ok());
    }

    #[test]
    fn expired_external_lease_is_evicted_before_status_or_next_login() {
        let coordinator = LoginCoordinator::with_lease_ttl(Duration::ZERO);
        let (session_id, _) = coordinator.acquire(ProviderId::QqMusic).unwrap();
        assert_eq!(coordinator.active(), None);
        assert!(!coordinator.owns(session_id, ProviderId::QqMusic));
        assert!(coordinator.acquire(ProviderId::Netease).is_ok());
    }
}
