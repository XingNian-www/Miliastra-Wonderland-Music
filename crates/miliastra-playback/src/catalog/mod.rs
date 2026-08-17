mod bilibili;
mod kugou;
mod netease;
mod provider;
mod qqmusic;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::credentials::ProviderCredential;
use crate::domain::{ResolverLocator, SearchSpec, SongKey, StreamSource};
use crate::lyrics::TimedLyrics;

#[async_trait]
pub trait KugouAccountAdapter: Send + Sync + 'static {
    /// Refresh the explicit credential snapshot supplied by the caller.
    ///
    /// The runtime uses the same snapshot version for the conditional save after
    /// this request completes, so implementations must not re-read the store.
    async fn refresh_token(
        &self,
        credential: &ProviderCredential,
    ) -> Result<ProviderCredential, CatalogError>;
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
    ///
    /// 适配器原始错误可能包含请求 URL、查询参数或服务端回显，不能越过此边界
    /// 进入播放状态或 HTTP 响应。
    pub fn as_failure(&self, provider: Option<&str>) -> Failure {
        let (code, message, retryable) = match self {
            Self::AuthRequired(_) => (
                "provider_auth_required",
                "source authentication is required",
                false,
            ),
            Self::CredentialRejected(_) => {
                ("relogin_required", "source credential was rejected", false)
            }
            Self::VipRequired(_) => ("track_vip_required", "track requires VIP membership", false),
            Self::NoCopyright(_) => ("track_no_copyright", "track has no copyright", false),
            Self::Unavailable(_) => ("track_unavailable", "track is unavailable", false),
            Self::RateLimited(_) => ("provider_rate_limited", "source rate limit reached", true),
            Self::TimedOut(_) => ("provider_timeout", "source request timed out", true),
            Self::Transient(_) => ("provider_transient", "source request failed", true),
            Self::InvalidResponse(_) => (
                "provider_invalid_response",
                "source response is invalid",
                false,
            ),
            Self::UnknownSource(_) => ("unknown_provider", "source is unknown", false),
            Self::InvalidResolverLocator(_)
            | Self::ResolverLocatorSourceMismatch { .. }
            | Self::ResolverLocatorTrackMismatch { .. } => (
                "track_unavailable",
                "track resolver metadata is invalid",
                false,
            ),
        };
        let mut failure = Failure::new(code, message);
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
    /// 跳过缓存并重新查询平台账号状态。
    async fn refresh_account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        self.account_status().await
    }
    /// 使账号状态缓存失效。登录、退出和凭据更新后调用。
    fn invalidate_account_status(&self) {}
    /// 播放请求已经给出了可以确定归因到账号的失败时，丢弃对应的乐观状态。
    ///
    /// 曲目元数据、版权、限流与网络错误不能说明登录态变化，因此不应触发刷新。
    /// 默认实现只处理认证与会员结论；适配器可按平台协议补充更细的处理。
    fn observe_playback_failure(&self, error: &CatalogError) {
        if matches!(
            error,
            CatalogError::AuthRequired(_)
                | CatalogError::CredentialRejected(_)
                | CatalogError::VipRequired(_)
        ) {
            self.invalidate_account_status();
        }
    }
}

/// 平台账号状态（web 面板登录卡片展示）。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountStatus {
    pub provider: String,
    pub logged_in: bool,
    pub user_id: Option<String>,
    pub nickname: Option<String>,
    /// 平台是否已给出可靠的 VIP 结论；B站等无VIP概念的平台为 false。
    pub vip_known: bool,
    pub vip: bool,
    pub vip_type: Option<String>,
    pub vip_expire_at_ms: Option<u64>,
    /// 登录方式标识（如 QQ 音乐 "qq"/"wechat"）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_method: Option<String>,
    pub checked_at_ms: u64,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 账号状态缓存及其失效代际。
///
/// 账号查询需要在释放互斥锁后等待网络返回。登录、登出或凭据更新期间，旧查询
/// 的响应不应重新写入缓存；调用方在请求前记录 [`Self::generation`]，写回时使用
/// [`Self::store_if_current`] 即可形成栅栏。
pub(crate) struct AccountStatusCache {
    generation: u64,
    entry: Option<(ProviderAccountStatus, Instant, bool)>,
    /// 缓存未命中时只能有一个任务访问账号接口；其余调用者等待后重新读取缓存。
    refresh_gate: Arc<tokio::sync::Mutex<()>>,
}

impl Default for AccountStatusCache {
    fn default() -> Self {
        Self {
            generation: 0,
            entry: None,
            refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl AccountStatusCache {
    pub(crate) fn refresh_gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_gate.clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn cached(
        &self,
        success_ttl: Duration,
        failed_ttl: Duration,
    ) -> Option<ProviderAccountStatus> {
        self.entry.as_ref().and_then(|(status, cached_at, failed)| {
            if cached_at.elapsed() >= if *failed { failed_ttl } else { success_ttl } {
                return None;
            }
            let mut status = status.clone();
            // 会员到期是权威时间边界，不能因为账号缓存仍在一天窗口内而继续将
            // 已到期会员当作可用。未知状态会交给曲目权限探测或下一次每日刷新确认。
            if status.vip
                && status
                    .vip_expire_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms <= current_epoch_ms())
            {
                status.vip = false;
                status.vip_known = false;
                status.stale = true;
                status.last_error = Some("vip_status_expired".to_owned());
            }
            Some(status)
        })
    }

    /// 仅当发起请求时的代际仍有效时，写入账号状态缓存。
    pub(crate) fn store_if_current(
        &mut self,
        expected_generation: u64,
        status: ProviderAccountStatus,
        cached_at: Instant,
        failed: bool,
    ) -> bool {
        if self.generation != expected_generation {
            return false;
        }
        self.entry = Some((status, cached_at, failed));
        true
    }

    /// 从同一代际的缓存构造降级状态。代际发生变化后不能使用新会话的缓存。
    pub(crate) fn stale_for_current(
        &self,
        expected_generation: u64,
        error: &CatalogError,
    ) -> Option<ProviderAccountStatus> {
        if self.generation != expected_generation {
            return None;
        }
        self.entry.as_ref().map(|(cached, ..)| {
            let mut stale = cached.clone();
            stale.stale = true;
            stale.last_error = Some(account_status_error_code(error).to_owned());
            stale
        })
    }

    /// 为首次查询失败构造可短暂缓存的脱敏状态。
    ///
    /// 首个调用者仍应收到原始错误，以保留重试/提示语义；缓存只用于随后一小段
    /// 时间内的轮询，避免在平台已限流或网络异常时持续重复请求。
    pub(crate) fn failed_for_current(
        &self,
        expected_generation: u64,
        provider: &str,
        checked_at_ms: u64,
        error: &CatalogError,
    ) -> Option<ProviderAccountStatus> {
        (self.generation == expected_generation).then(|| ProviderAccountStatus {
            provider: provider.to_owned(),
            checked_at_ms,
            stale: true,
            last_error: Some(account_status_error_code(error).to_owned()),
            ..ProviderAccountStatus::default()
        })
    }

    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entry = None;
    }
}

fn current_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 将账号状态查询错误映射为可公开展示的稳定错误码。
///
/// 不返回原始错误文本，因为请求错误可能包含 URL 查询参数、Cookie 或凭据。
pub(crate) fn account_status_error_code(error: &CatalogError) -> &'static str {
    match error {
        CatalogError::AuthRequired(_) | CatalogError::CredentialRejected(_) => "relogin_required",
        CatalogError::RateLimited(_) => "provider_rate_limited",
        CatalogError::TimedOut(_) => "provider_timeout",
        CatalogError::Transient(_) => "provider_transient",
        CatalogError::InvalidResponse(_)
        | CatalogError::InvalidResolverLocator(_)
        | CatalogError::ResolverLocatorSourceMismatch { .. }
        | CatalogError::ResolverLocatorTrackMismatch { .. } => "provider_invalid_response",
        CatalogError::VipRequired(_) => "account_vip_required",
        CatalogError::NoCopyright(_) | CatalogError::Unavailable(_) => "provider_unavailable",
        CatalogError::UnknownSource(_) => "unknown_provider",
    }
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

    pub async fn refresh_account_status(
        &self,
        source: &str,
    ) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let adapter = self
            .get(source)
            .ok_or_else(|| CatalogError::UnknownSource(format!("unknown source: {source}")))?;
        adapter.refresh_account_status().await
    }

    pub fn invalidate_account_status(&self, source: &str) -> Result<(), CatalogError> {
        let adapter = self
            .get(source)
            .ok_or_else(|| CatalogError::UnknownSource(format!("unknown source: {source}")))?;
        adapter.invalidate_account_status();
        Ok(())
    }

    /// 将播放过程中的确定性认证/VIP失败反馈给账号缓存。
    pub fn observe_playback_failure(
        &self,
        source: &str,
        error: &CatalogError,
    ) -> Result<(), CatalogError> {
        let adapter = self
            .get(source)
            .ok_or_else(|| CatalogError::UnknownSource(format!("unknown source: {source}")))?;
        adapter.observe_playback_failure(error);
        Ok(())
    }

    pub fn sources(&self) -> Vec<String> {
        let mut sources = self.adapters.keys().cloned().collect::<Vec<_>>();
        sources.sort();
        sources
    }
}

#[cfg(test)]
mod provider_contract_tests {
    use std::time::{Duration, Instant};

    use super::{
        AccountStatusCache, CatalogError, Failure, ProviderAccountStatus,
        account_status_error_code, current_epoch_ms,
    };

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
                CatalogError::InvalidResolverLocator("malformed metadata".to_owned()),
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
    fn catalog_failures_never_expose_adapter_request_details() {
        let secret = "token=private-token-value&cookie=session-secret";
        let failure =
            CatalogError::Transient(format!("request failed: {secret}")).as_failure(Some("kugou"));

        assert_eq!(failure.code, "provider_transient");
        assert_eq!(failure.message, "source request failed");
        assert_eq!(failure.provider.as_deref(), Some("kugou"));
        assert!(!failure.message.contains(secret));
        assert!(!failure.message.contains("private-token-value"));
        assert!(!failure.message.contains("session-secret"));
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

    #[test]
    fn account_status_cache_rejects_a_write_from_an_invalidated_generation() {
        let mut cache = AccountStatusCache::default();
        let old_generation = cache.generation();
        cache.invalidate();

        let written = cache.store_if_current(
            old_generation,
            ProviderAccountStatus {
                provider: "qqmusic".to_owned(),
                logged_in: true,
                ..ProviderAccountStatus::default()
            },
            Instant::now(),
            false,
        );

        assert!(!written);
        assert!(
            cache
                .cached(Duration::from_secs(1), Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn account_status_cache_does_not_treat_an_expired_vip_as_current() {
        let mut cache = AccountStatusCache::default();
        let generation = cache.generation();
        assert!(cache.store_if_current(
            generation,
            ProviderAccountStatus {
                provider: "qqmusic".to_owned(),
                logged_in: true,
                vip_known: true,
                vip: true,
                vip_expire_at_ms: Some(current_epoch_ms().saturating_sub(1)),
                ..ProviderAccountStatus::default()
            },
            Instant::now(),
            false,
        ));

        let cached = cache
            .cached(Duration::from_secs(60), Duration::from_secs(1))
            .expect("the account state itself is still cached");
        assert!(!cached.vip);
        assert!(!cached.vip_known);
        assert!(cached.stale);
        assert_eq!(cached.last_error.as_deref(), Some("vip_status_expired"));
    }

    #[test]
    fn account_status_error_serialization_never_exposes_request_details() {
        let secret = "token=private-token-value&cookie=session-secret";
        let error = CatalogError::Transient(format!("request failed: {secret}"));
        let mut cache = AccountStatusCache::default();
        let generation = cache.generation();
        assert!(cache.store_if_current(
            generation,
            ProviderAccountStatus {
                provider: "kugou".to_owned(),
                logged_in: true,
                ..ProviderAccountStatus::default()
            },
            Instant::now(),
            false,
        ));

        let stale = cache.stale_for_current(generation, &error).unwrap();
        let serialized = serde_json::to_string(&stale).unwrap();
        assert_eq!(stale.last_error.as_deref(), Some("provider_transient"));
        assert_eq!(account_status_error_code(&error), "provider_transient");
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("private-token-value"));
        assert!(!serialized.contains("session-secret"));
    }

    #[test]
    fn first_account_status_failure_can_be_short_term_cached_without_leaking_details() {
        let mut cache = AccountStatusCache::default();
        let generation = cache.generation();
        let secret = "cookie=session-secret";
        let error = CatalogError::Transient(format!("request failed: {secret}"));
        let failed = cache
            .failed_for_current(generation, "qqmusic", 42, &error)
            .expect("the current generation may cache its failure state");

        assert!(failed.stale);
        assert_eq!(failed.provider, "qqmusic");
        assert_eq!(failed.checked_at_ms, 42);
        assert_eq!(failed.last_error.as_deref(), Some("provider_transient"));
        assert!(cache.store_if_current(generation, failed, Instant::now(), true));

        let cached = cache
            .cached(Duration::from_secs(1), Duration::from_secs(1))
            .expect("a failed entry uses the short failure TTL");
        let serialized = serde_json::to_string(&cached).unwrap();
        assert!(cached.stale);
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("session-secret"));
    }
}
