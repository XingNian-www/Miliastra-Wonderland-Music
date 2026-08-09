mod bilibili;
mod kugou;
mod netease;
mod provider;
mod qqmusic;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::credentials::ProviderCredential;
use crate::domain::{ResolverLocator, SearchSpec, SongKey, StreamSource};
use crate::lyrics::TimedLyrics;

#[async_trait]
pub trait KugouAccountAdapter: Send + Sync + 'static {
    async fn refresh_token(&self) -> Result<ProviderCredential, CatalogError>;
    async fn account_status(&self) -> Result<kugou::KugouAccountStatus, CatalogError>;
    async fn claim_vip(&self) -> Result<kugou::KugouListenReport, CatalogError>;
    async fn upgrade_vip(&self) -> Result<kugou::KugouListenReport, CatalogError>;
}

/// 支持手动凭据刷新的平台适配器（QQ 音乐、哔哩哔哩）。
/// 刷新成功后返回新凭据；返回 `None` 表示当前凭据无需刷新。
#[async_trait]
pub trait CredentialRefreshAdapter: Send + Sync + 'static {
    async fn refresh_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError>;
}

pub use crate::domain::Failure;
pub use bilibili::BilibiliAdapter;
pub use kugou::{KugouAccountStatus, KugouAdapter, KugouListenReport};
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
    #[error("track requires VIP membership: {0}")]
    VipRequired(String),
    #[error("track has no copyright: {0}")]
    NoCopyright(String),
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
    /// 将适配器错误转换为稳定的平台故障分类。
    pub fn as_failure(&self, provider: Option<&str>) -> Failure {
        let (code, retryable) = match self {
            Self::AuthRequired(_) => ("provider_auth_required", false),
            Self::CredentialRejected(_) => ("relogin_required", false),
            Self::VipRequired(_) => ("track_vip_required", false),
            Self::NoCopyright(_) => ("track_no_copyright", false),
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
    async fn validate_credential(
        &self,
        _candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        Ok(())
    }
    async fn search(&self, spec: &SearchSpec)
    -> Result<Vec<ProviderSearchCandidate>, CatalogError>;
    async fn resolve(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, CatalogError>;
    /// 搜索候选的真实可用性探测：默认实际解析一次播放流确认。
    /// 平台有权威权限接口时可覆盖（如酷狗 /privilege/lite 的 pay_type）。
    /// 返回 `Ok(eligibility)` 表示已确认；`Err` 透传探测错误（认证/限流/超时等无法确认）。
    async fn probe_eligibility(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<PlaybackEligibility, CatalogError> {
        let _ = self.resolve(key, locator).await?;
        Ok(PlaybackEligibility::Eligible)
    }
    async fn lyrics(
        &self,
        _key: &SongKey,
        _locator: Option<&ResolverLocator>,
    ) -> Result<Option<TimedLyrics>, CatalogError> {
        Ok(None)
    }
    /// 平台账号状态（登录态/昵称/VIP）。默认不支持，返回 None。
    async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        Ok(None)
    }
}

/// 平台账号状态（web 面板登录卡片展示）。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountStatus {
    pub logged_in: bool,
    pub user_id: Option<String>,
    pub nickname: Option<String>,
    pub vip: bool,
    pub vip_type: Option<String>,
    pub vip_expire_at_ms: Option<u64>,
    /// 登录方式标识（如 QQ 音乐 "qq"/"wechat"）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_method: Option<String>,
}

#[derive(Clone, Default)]
pub struct SourceCatalog {
    adapters: Arc<HashMap<String, Arc<dyn SourceAdapter>>>,
    kugou_account: Option<Arc<dyn KugouAccountAdapter>>,
    refresh_adapters: Arc<HashMap<String, Arc<dyn CredentialRefreshAdapter>>>,
}

impl SourceCatalog {
    pub fn new(adapters: impl IntoIterator<Item = (String, Arc<dyn SourceAdapter>)>) -> Self {
        let adapters = adapters.into_iter().collect();
        Self {
            adapters: Arc::new(adapters),
            kugou_account: None,
            refresh_adapters: Arc::new(HashMap::new()),
        }
    }

    pub fn with_kugou_account(mut self, account: Arc<dyn KugouAccountAdapter>) -> Self {
        self.kugou_account = Some(account);
        self
    }

    pub fn kugou_account(&self) -> Option<Arc<dyn KugouAccountAdapter>> {
        self.kugou_account.clone()
    }

    pub fn with_refresh_adapter(
        mut self,
        provider: &str,
        adapter: Arc<dyn CredentialRefreshAdapter>,
    ) -> Self {
        Arc::make_mut(&mut self.refresh_adapters).insert(provider.to_owned(), adapter);
        self
    }

    pub fn refresh_adapter(&self, provider: &str) -> Option<Arc<dyn CredentialRefreshAdapter>> {
        self.refresh_adapters.get(provider).cloned()
    }

    pub fn get(&self, source: &str) -> Option<Arc<dyn SourceAdapter>> {
        self.adapters.get(source).cloned()
    }

    /// 查询平台账号状态（QQ 音乐/网易云）；平台不支持或未登录时返回 None。
    pub async fn account_status(
        &self,
        source: &str,
    ) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let adapter = self
            .get(source)
            .ok_or_else(|| CatalogError::UnknownSource(format!("unknown source: {source}")))?;
        adapter.account_status().await
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
