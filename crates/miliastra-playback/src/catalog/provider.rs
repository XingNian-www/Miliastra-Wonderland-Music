use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::Song;

/// Stable provider identity used by the new provider contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    #[serde(rename = "qqmusic")]
    QqMusic,
    #[serde(rename = "netease")]
    Netease,
    Bilibili,
    #[serde(rename = "kugou")]
    Kugou,
}

impl ProviderId {
    pub const ALL: [Self; 4] = [Self::QqMusic, Self::Netease, Self::Bilibili, Self::Kugou];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
            Self::Kugou => "kugou",
        }
    }
}

impl Display for ProviderId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<ProviderId> for String {
    fn from(provider: ProviderId) -> Self {
        provider.to_string()
    }
}

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "qqmusic" => Ok(Self::QqMusic),
            "netease" => Ok(Self::Netease),
            "bilibili" => Ok(Self::Bilibili),
            "kugou" => Ok(Self::Kugou),
            _ => Err(UnknownProvider(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider: {0}")]
pub struct UnknownProvider(pub String);

/// Registry for the providers that are always present in the native runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderRegistry;

impl ProviderRegistry {
    pub fn known(&self, provider: &str) -> bool {
        provider.parse::<ProviderId>().is_ok()
    }

    pub fn enabled(&self) -> Vec<String> {
        ProviderId::ALL.into_iter().map(String::from).collect()
    }

    pub fn require_enabled(&self, provider: &str) -> Result<ProviderId, crate::catalog::Failure> {
        let provider_id = provider.parse::<ProviderId>().map_err(|_| {
            crate::catalog::Failure::new("unknown_provider", "provider identifier is unknown")
                .with_provider(provider)
        })?;
        Ok(provider_id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackEligibility {
    Eligible,
    Ineligible,
    #[default]
    Unknown,
}

impl PlaybackEligibility {
    pub const fn preference_rank(self) -> u8 {
        match self {
            Self::Eligible => 2,
            Self::Unknown => 1,
            Self::Ineligible => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchCandidate {
    #[serde(flatten)]
    pub song: Song,
    pub eligibility: PlaybackEligibility,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchOutcome {
    pub provider: String,
    pub candidates: Vec<ProviderSearchCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::catalog::Failure>,
}

impl ProviderSearchOutcome {
    pub fn success(provider: impl Into<String>, candidates: Vec<ProviderSearchCandidate>) -> Self {
        Self {
            provider: provider.into(),
            candidates,
            failure: None,
        }
    }

    pub fn failed(provider: impl Into<String>, failure: crate::catalog::Failure) -> Self {
        let provider = provider.into();
        // An outcome belongs to exactly one requested provider. Do not let a
        // lower-level error accidentally attribute this branch to another one.
        let failure = failure.with_provider(provider.clone());
        Self {
            provider,
            candidates: Vec::new(),
            failure: Some(failure),
        }
    }

    pub fn is_success(&self) -> bool {
        self.failure.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_ids_are_strict_and_registry_lists_all_known_providers() {
        assert_eq!(
            ProviderId::ALL.map(ProviderId::as_str),
            ["qqmusic", "netease", "bilibili", "kugou"]
        );
        assert!("tx".parse::<ProviderId>().is_err());
        let registry = ProviderRegistry;
        assert_eq!(
            registry.enabled(),
            vec!["qqmusic", "netease", "bilibili", "kugou"]
        );
    }

    #[test]
    fn availability_failures_are_stable_and_provider_attributed() {
        let registry = ProviderRegistry;
        assert_eq!(
            registry.require_enabled("bogus").unwrap_err().code,
            "unknown_provider"
        );
    }

    #[test]
    fn empty_success_is_distinct_from_failed_search() {
        let outcome = ProviderSearchOutcome::success("qqmusic", Vec::new());
        assert!(outcome.is_success());
        let failure = ProviderSearchOutcome::failed(
            "qqmusic",
            crate::catalog::Failure::new("provider_timeout", "timed out"),
        )
        .failure
        .unwrap();
        assert_eq!(failure.provider.as_deref(), Some("qqmusic"));
    }

    #[test]
    fn failed_outcome_is_always_attributed_to_its_provider_branch() {
        let failure = ProviderSearchOutcome::failed(
            "netease",
            crate::catalog::Failure::new("provider_timeout", "timed out").with_provider("qqmusic"),
        )
        .failure
        .unwrap();

        assert_eq!(failure.provider.as_deref(), Some("netease"));
    }
}
