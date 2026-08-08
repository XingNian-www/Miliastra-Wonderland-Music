mod bilibili;
mod netease;
mod provider;
mod qqmusic;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::credentials::ProviderCredential;
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};

pub use crate::domain::Failure;
pub use bilibili::BilibiliAdapter;
pub use netease::NeteaseAdapter;
pub use provider::{
    PlaybackEligibility, ProviderId, ProviderRegistry, ProviderSearchCandidate,
    ProviderSearchOutcome,
};
pub use qqmusic::QqMusicAdapter;

#[derive(Clone, Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("source authentication is required: {0}")]
    AuthRequired(String),
    #[error("source credential was rejected: {0}")]
    CredentialRejected(String),
    #[error("track is unavailable: {0}")]
    Unavailable(String),
    #[error("source rate limit reached: {0}")]
    RateLimited(String),
    #[error("source request timed out: {0}")]
    TimedOut(String),
    #[error("source request failed: {0}")]
    Transient(String),
    #[error("source response is invalid: {0}")]
    InvalidResponse(String),
    #[error("resolver locator is invalid: {0}")]
    InvalidResolverLocator(String),
    #[error(
        "resolver locator source {locator_source} does not match requested source {requested_source}"
    )]
    ResolverLocatorSourceMismatch {
        requested_source: String,
        locator_source: String,
    },
    #[error("resolver locator track {locator_id} does not match requested track {requested_id}")]
    ResolverLocatorTrackMismatch {
        requested_id: String,
        locator_id: String,
    },
    #[error("unknown source: {0}")]
    UnknownSource(String),
}

impl CatalogError {
    /// Convert legacy adapter errors into the stable provider failure taxonomy.
    pub fn as_failure(&self, provider: Option<&str>) -> Failure {
        let (code, retryable) = match self {
            Self::AuthRequired(_) => ("provider_auth_required", false),
            Self::CredentialRejected(_) => ("relogin_required", false),
            Self::Unavailable(_) => ("track_unavailable", false),
            Self::RateLimited(_) => ("provider_rate_limited", true),
            Self::TimedOut(_) => ("provider_timeout", true),
            Self::Transient(_) => ("provider_transient", true),
            Self::InvalidResponse(_) => ("provider_invalid_response", false),
            Self::UnknownSource(_) => ("unknown_provider", false),
            Self::InvalidResolverLocator(_)
            | Self::ResolverLocatorSourceMismatch { .. }
            | Self::ResolverLocatorTrackMismatch { .. } => ("provider_invalid_response", false),
        };
        let mut failure = Failure::new(code, self.to_string());
        failure.retryable = retryable;
        failure.provider = provider.map(str::to_owned);
        failure
    }
}

impl From<&CatalogError> for Failure {
    fn from(error: &CatalogError) -> Self {
        error.as_failure(None)
    }
}

#[async_trait]
pub trait SourceAdapter: Send + Sync + 'static {
    fn source_code(&self) -> &'static str;
    async fn validate_credential(
        &self,
        _candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        Ok(())
    }
    async fn search(&self, spec: &SearchSpec) -> Result<Vec<Song>, CatalogError>;
    /// Provider-specific search metadata is normalized here. Legacy adapters
    /// that do not expose rights fields remain `unknown` rather than being
    /// guessed as playable.
    async fn search_candidates(
        &self,
        spec: &SearchSpec,
    ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
        Ok(self
            .search(spec)
            .await?
            .into_iter()
            .map(|song| ProviderSearchCandidate {
                song,
                eligibility: PlaybackEligibility::Unknown,
            })
            .collect())
    }
    async fn resolve(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, CatalogError>;
}

#[derive(Clone, Default)]
pub struct SourceCatalog {
    adapters: Arc<HashMap<String, Arc<dyn SourceAdapter>>>,
}

impl SourceCatalog {
    pub fn new(adapters: impl IntoIterator<Item = Arc<dyn SourceAdapter>>) -> Self {
        let adapters = adapters
            .into_iter()
            .map(|adapter| (adapter.source_code().to_owned(), adapter))
            .collect();
        Self {
            adapters: Arc::new(adapters),
        }
    }

    pub fn get(&self, source: &str) -> Option<Arc<dyn SourceAdapter>> {
        self.adapters.get(source).cloned()
    }

    pub fn sources(&self) -> Vec<String> {
        let mut sources = self.adapters.keys().cloned().collect::<Vec<_>>();
        sources.sort();
        sources
    }
}

#[cfg(test)]
mod provider_contract_tests {
    use super::{CatalogError, Failure};

    #[test]
    fn catalog_errors_map_to_stable_failure_codes() {
        let cases = [
            (
                CatalogError::RateLimited("busy".to_owned()),
                "provider_rate_limited",
                true,
            ),
            (
                CatalogError::TimedOut("slow".to_owned()),
                "provider_timeout",
                true,
            ),
            (
                CatalogError::Transient("offline".to_owned()),
                "provider_transient",
                true,
            ),
            (
                CatalogError::InvalidResponse("bad json".to_owned()),
                "provider_invalid_response",
                false,
            ),
            (
                CatalogError::Unavailable("no rights".to_owned()),
                "track_unavailable",
                false,
            ),
            (
                CatalogError::CredentialRejected("expired".to_owned()),
                "relogin_required",
                false,
            ),
        ];
        for (error, code, retryable) in cases {
            let failure = error.as_failure(Some("qqmusic"));
            assert_eq!(failure.code, code);
            assert_eq!(failure.retryable, retryable);
            assert_eq!(failure.provider.as_deref(), Some("qqmusic"));
        }
    }

    #[test]
    fn failure_serialization_uses_the_wire_field_names() {
        let failure = Failure::new("provider_timeout", "timed out")
            .with_provider("qqmusic")
            .with_retry_after_ms(250);
        let value = serde_json::to_value(failure).unwrap();
        assert_eq!(value["retryAfterMs"], 250);
        assert!(value.get("retry_after_ms").is_none());
    }
}
