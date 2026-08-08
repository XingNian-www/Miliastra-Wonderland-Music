use std::collections::BTreeMap;
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
    Kuwo,
    Kugou,
}

impl ProviderId {
    pub const ALL: [Self; 5] = [
        Self::QqMusic,
        Self::Netease,
        Self::Bilibili,
        Self::Kuwo,
        Self::Kugou,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
            Self::Kuwo => "kuwo",
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
            "kuwo" => Ok(Self::Kuwo),
            "kugou" => Ok(Self::Kugou),
            _ => Err(UnknownProvider(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider: {0}")]
pub struct UnknownProvider(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAvailability {
    pub provider: String,
    pub compiled: bool,
    pub enabled: bool,
}

impl ProviderAvailability {
    pub fn new(provider: ProviderId, compiled: bool, enabled: bool) -> Self {
        Self {
            provider: provider.to_string(),
            compiled,
            enabled: compiled && enabled,
        }
    }
}

/// Registry of known providers and their build/runtime availability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRegistry {
    entries: BTreeMap<ProviderId, ProviderAvailability>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::first_stage()
    }
}

impl ProviderRegistry {
    pub fn new(
        compiled: impl IntoIterator<Item = ProviderId>,
        enabled: impl IntoIterator<Item = ProviderId>,
    ) -> Self {
        let compiled = compiled
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let enabled = enabled
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let entries = ProviderId::ALL
            .into_iter()
            .map(|provider| {
                (
                    provider,
                    ProviderAvailability::new(
                        provider,
                        compiled.contains(&provider),
                        enabled.contains(&provider),
                    ),
                )
            })
            .collect();
        Self { entries }
    }

    /// First-stage build: QQ Music, NetEase, and Bilibili are compiled and enabled.
    pub fn first_stage() -> Self {
        #[allow(unused_mut)]
        let mut compiled = Vec::new();
        #[cfg(feature = "qqmusic")]
        compiled.push(ProviderId::QqMusic);
        #[cfg(feature = "netease")]
        compiled.push(ProviderId::Netease);
        #[allow(unused_mut)]
        let mut enabled = compiled.clone();
        #[cfg(feature = "bilibili")]
        {
            compiled.push(ProviderId::Bilibili);
            enabled.push(ProviderId::Bilibili);
        }
        Self::new(compiled, enabled)
    }

    pub fn availability(&self, provider: ProviderId) -> ProviderAvailability {
        self.entries[&provider].clone()
    }

    pub fn known(&self, provider: &str) -> bool {
        provider.parse::<ProviderId>().is_ok()
    }

    pub fn known_providers(&self) -> Vec<String> {
        ProviderId::ALL.into_iter().map(String::from).collect()
    }

    pub fn is_compiled(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderId>()
            .ok()
            .is_some_and(|provider| self.availability(provider).compiled)
    }

    pub fn is_enabled(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderId>()
            .ok()
            .is_some_and(|provider| self.availability(provider).enabled)
    }

    pub fn status(&self) -> Vec<ProviderAvailability> {
        ProviderId::ALL
            .into_iter()
            .map(|provider| self.availability(provider))
            .collect()
    }

    pub fn compiled(&self) -> Vec<String> {
        self.status()
            .into_iter()
            .filter(|entry| entry.compiled)
            .map(|entry| entry.provider)
            .collect()
    }

    pub fn enabled(&self) -> Vec<String> {
        self.status()
            .into_iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.provider)
            .collect()
    }

    pub fn require_enabled(&self, provider: &str) -> Result<ProviderId, crate::catalog::Failure> {
        let provider_id = provider.parse::<ProviderId>().map_err(|_| {
            crate::catalog::Failure::new("unknown_provider", "provider identifier is unknown")
                .with_provider(provider)
        })?;
        let status = self.availability(provider_id);
        if !status.compiled {
            return Err(crate::catalog::Failure::new(
                "provider_not_compiled",
                "provider adapter is not compiled into this binary",
            )
            .with_provider(provider));
        }
        if !status.enabled {
            return Err(crate::catalog::Failure::new(
                "provider_disabled",
                "provider is disabled by runtime policy",
            )
            .with_provider(provider));
        }
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

pub type Eligibility = PlaybackEligibility;

pub fn canonical_provider_id(value: &str) -> Option<ProviderId> {
    value.parse().ok()
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
            ["qqmusic", "netease", "bilibili", "kuwo", "kugou"]
        );
        assert!("tx".parse::<ProviderId>().is_err());
        let registry = ProviderRegistry::default();
        assert_eq!(registry.compiled(), vec!["qqmusic", "netease", "bilibili"]);
        assert_eq!(registry.enabled(), vec!["qqmusic", "netease", "bilibili"]);
        assert_eq!(registry.status().len(), 5);
    }

    #[test]
    fn availability_failures_are_stable_and_provider_attributed() {
        let registry = ProviderRegistry::default();
        assert_eq!(
            registry.require_enabled("kuwo").unwrap_err().code,
            "provider_not_compiled"
        );
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
