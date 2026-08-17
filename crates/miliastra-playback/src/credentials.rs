use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SUPPORTED_PROVIDERS: &[&str] = &["qqmusic", "netease", "bilibili", "kugou"];
/// Successful credential and account-status checks are intentionally sparse.
/// Playback failures and explicit user actions remain separate recovery paths.
pub(crate) const DAILY_REFRESH_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_SECRET_BYTES: usize = 64 * 1024;
const CREDENTIAL_SCHEMA_VERSION: u32 = 2;
const REFRESH_STATE_FILE: &str = "refresh-state.json";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialEnvelope {
    schema_version: u32,
    credential: ProviderCredential,
}

/// Plaintext account state captured by the provider login helper. The store
/// never exposes secret values through its status APIs; native adapters read a
/// clone only when they need to make a provider request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind",
    deny_unknown_fields
)]
pub enum ProviderCredential {
    #[serde(rename = "qqMusic")]
    QqMusic { cookies: BTreeMap<String, String> },
    #[serde(rename = "netease")]
    Netease { cookies: BTreeMap<String, String> },
    #[serde(rename = "bilibili")]
    Bilibili {
        cookies: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
    },
    #[serde(rename = "kugou")]
    Kugou {
        token: String,
        userid: String,
        dfid: String,
        cookies: BTreeMap<String, String>,
    },
}

impl std::fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderCredential")
            .field("provider", &self.provider())
            .field("fields", &self.presence().fields)
            .finish()
    }
}

impl ProviderCredential {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::QqMusic { .. } => "qqmusic",
            Self::Netease { .. } => "netease",
            Self::Bilibili { .. } => "bilibili",
            Self::Kugou { .. } => "kugou",
        }
    }

    pub fn cookies(&self) -> &BTreeMap<String, String> {
        match self {
            Self::QqMusic { cookies }
            | Self::Netease { cookies }
            | Self::Bilibili { cookies, .. }
            | Self::Kugou { cookies, .. } => cookies,
        }
    }

    fn validate(&self) -> Result<(), CredentialError> {
        match self {
            Self::QqMusic { cookies } => {
                require_allowed_cookie_names(
                    cookies,
                    &[
                        "uin",
                        "wxuin",
                        "p_uin",
                        "p_luin",
                        "luin",
                        "p_skey",
                        "p_lskey",
                        "skey",
                        "lskey",
                        "qqmusic_key",
                        "qm_keyst",
                        "lqm_keyst",
                        "login_type",
                        "wxopenid",
                        "wxaccess_token",
                        "wxrefresh_token",
                        "wxunionid",
                        "psrf_qqopenid",
                        "openid",
                        "psrf_qqaccess_token",
                        "access_token",
                        "psrf_qqrefresh_token",
                        "refresh_token",
                        "psrf_qqrefresh_key",
                        "refresh_key",
                        "psrf_qqunionid",
                    ],
                )?;
                require_any_cookie(cookies, "uin", &["uin", "wxuin"])?;
                require_any_cookie(cookies, "musicKey", &["qqmusic_key", "qm_keyst"])?;
            }
            Self::Netease { cookies } => {
                require_allowed_cookie_names(cookies, &["MUSIC_U", "__csrf"])?;
                require_any_cookie(cookies, "musicU", &["MUSIC_U"])?;
            }
            Self::Bilibili { cookies, .. } => {
                require_allowed_cookie_names(
                    cookies,
                    &[
                        "SESSDATA",
                        "bili_jct",
                        "DedeUserID",
                        "DedeUserID__ckMd5",
                        "sid",
                        "buvid3",
                        "buvid4",
                        "b_nut",
                        "_uuid",
                        "b_lsid",
                    ],
                )?;
                require_any_cookie(cookies, "sessdata", &["SESSDATA"])?;
            }
            Self::Kugou {
                token,
                userid,
                dfid,
                cookies,
            } => {
                if !has_value(token) || !has_value(userid) || !has_value(dfid) {
                    return Err(CredentialError::Invalid(
                        "Kugou token, userid and dfid must not be empty".to_owned(),
                    ));
                }
                // t1 是概念版续期字段，登录/刷新时由官方 Set-Cookie 下发。
                require_allowed_cookie_names(
                    cookies,
                    &["KugouGUID", "kg_mid", "mid", "t1", "vip_type", "vip_token"],
                )?;
            }
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| CredentialError::Invalid(error.to_string()))?;
        if encoded.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::Invalid(format!(
                "credential exceeds {MAX_SECRET_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn presence(&self) -> CredentialStatus {
        let fields = match self {
            Self::QqMusic { cookies } => BTreeMap::from([
                ("uin", has_any_cookie(cookies, &["uin", "wxuin"])),
                (
                    "musicKey",
                    has_any_cookie(cookies, &["qqmusic_key", "qm_keyst"]),
                ),
                (
                    "openId",
                    has_any_cookie(cookies, &["psrf_qqopenid", "openid", "wxopenid"]),
                ),
                (
                    "accessToken",
                    has_any_cookie(
                        cookies,
                        &["psrf_qqaccess_token", "access_token", "wxaccess_token"],
                    ),
                ),
                (
                    "refreshToken",
                    has_any_cookie(
                        cookies,
                        &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"],
                    ),
                ),
                (
                    "refreshKey",
                    has_any_cookie(cookies, &["psrf_qqrefresh_key", "refresh_key"]),
                ),
            ]),
            Self::Netease { cookies } => BTreeMap::from([
                ("musicU", has_any_cookie(cookies, &["MUSIC_U"])),
                ("csrf", has_any_cookie(cookies, &["__csrf"])),
            ]),
            Self::Bilibili {
                cookies,
                refresh_token,
            } => BTreeMap::from([
                ("sessdata", has_any_cookie(cookies, &["SESSDATA"])),
                ("csrf", has_any_cookie(cookies, &["bili_jct"])),
                ("userId", has_any_cookie(cookies, &["DedeUserID"])),
                (
                    "refreshToken",
                    refresh_token.as_deref().is_some_and(has_value),
                ),
                ("buvid3", has_any_cookie(cookies, &["buvid3"])),
            ]),
            Self::Kugou {
                token,
                userid,
                dfid,
                ..
            } => BTreeMap::from([
                ("token", has_value(token)),
                ("userId", has_value(userid)),
                ("dfid", has_value(dfid)),
            ]),
        };
        CredentialStatus {
            provider: self.provider(),
            configured: true,
            fields,
            refresh_supported: matches!(
                self,
                Self::Kugou { .. } | Self::QqMusic { .. } | Self::Bilibili { .. }
            ),
            manual_refresh_supported: matches!(
                self,
                Self::Kugou { .. } | Self::QqMusic { .. } | Self::Bilibili { .. }
            ),
            refresh_ready: match self {
                Self::QqMusic { cookies } => qq_refresh_ready(cookies),
                Self::Bilibili { refresh_token, .. } => {
                    refresh_token.as_deref().is_some_and(has_value)
                }
                Self::Kugou { token, .. } => has_value(token),
                Self::Netease { .. } => false,
            },
            refresh_state: "idle",
            last_refresh_at_ms: None,
            next_refresh_check_at_ms: None,
            last_refresh_error: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RefreshMetadata {
    state: String,
    last_refresh_at_ms: Option<u64>,
    next_refresh_check_at_ms: Option<u64>,
    next_account_status_check_at_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatus {
    pub provider: &'static str,
    pub configured: bool,
    pub fields: BTreeMap<&'static str, bool>,
    pub refresh_supported: bool,
    pub manual_refresh_supported: bool,
    pub refresh_ready: bool,
    pub refresh_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_refresh_check_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_error: Option<String>,
}

impl CredentialStatus {
    pub fn empty(provider: &'static str) -> Self {
        let fields = match provider {
            "qqmusic" => BTreeMap::from([
                ("uin", false),
                ("musicKey", false),
                ("openId", false),
                ("accessToken", false),
                ("refreshToken", false),
                ("refreshKey", false),
            ]),
            "netease" => BTreeMap::from([("musicU", false), ("csrf", false)]),
            "bilibili" => BTreeMap::from([
                ("sessdata", false),
                ("csrf", false),
                ("userId", false),
                ("refreshToken", false),
                ("buvid3", false),
            ]),
            "kugou" => BTreeMap::from([("token", false), ("userId", false), ("dfid", false)]),
            _ => BTreeMap::new(),
        };
        Self {
            provider,
            configured: false,
            fields,
            refresh_supported: matches!(provider, "kugou" | "qqmusic" | "bilibili"),
            manual_refresh_supported: matches!(provider, "kugou" | "qqmusic" | "bilibili"),
            refresh_ready: false,
            refresh_state: "unavailable",
            last_refresh_at_ms: None,
            next_refresh_check_at_ms: None,
            last_refresh_error: None,
        }
    }
}

#[derive(Clone)]
pub struct CredentialStore {
    directory: Option<PathBuf>,
    credentials: Arc<RwLock<BTreeMap<&'static str, ProviderCredential>>>,
    /// 单调递增的凭据版本。异步刷新只能写回它开始时观察到的版本。
    credential_revisions: Arc<RwLock<BTreeMap<&'static str, u64>>>,
    refresh_metadata: Arc<RwLock<BTreeMap<String, RefreshMetadata>>>,
    /// 同一平台的手动和自动刷新共用此集合，避免并发发起重复续期请求。
    refresh_inflight: Arc<Mutex<BTreeSet<&'static str>>>,
    /// 串行化凭据的“检查版本 -> 落盘 -> 更新内存”事务。
    mutation_lock: Arc<Mutex<()>>,
    /// 串行化磁盘写入,避免 fsync 在 RwLock 写锁内执行阻塞所有读取。
    save_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
pub(crate) struct CredentialSnapshot {
    pub credential: ProviderCredential,
    pub revision: u64,
}

/// A per-provider refresh lease shared by explicit and implicit refresh paths.
///
/// Dropping the lease releases the provider even when an async task exits early.
pub(crate) struct CredentialRefreshLease {
    inflight: Arc<Mutex<BTreeSet<&'static str>>>,
    provider: &'static str,
}

impl Drop for CredentialRefreshLease {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.remove(self.provider);
        }
    }
}

impl CredentialStore {
    pub fn open(directory: PathBuf) -> Result<Self, CredentialError> {
        ensure_credential_directory(&directory)?;
        let mut credentials = BTreeMap::new();
        let refresh_metadata = load_refresh_metadata(&directory)?;
        for provider in SUPPORTED_PROVIDERS {
            let path = credential_path(&directory, provider);
            restore_interrupted_write(&path)?;
            if !path.exists() {
                continue;
            }
            secure_credential_file(&path)?;
            let text = fs::read_to_string(&path).map_err(|error| CredentialError::Read {
                path: path.clone(),
                error: error.to_string(),
            })?;
            let value = serde_json::from_str::<serde_json::Value>(&text).map_err(|error| {
                CredentialError::Invalid(format!("{}: {error}", path.display()))
            })?;
            let schema_version = value
                .get("schemaVersion")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u32::try_from(version).ok());
            if schema_version != Some(CREDENTIAL_SCHEMA_VERSION) {
                return Err(CredentialError::UnsupportedSchema {
                    path,
                    expected: CREDENTIAL_SCHEMA_VERSION,
                    actual: schema_version,
                });
            }
            let envelope =
                serde_json::from_value::<CredentialEnvelope>(value).map_err(|error| {
                    CredentialError::Invalid(format!("{}: {error}", path.display()))
                })?;
            let credential = envelope.credential;
            validate_source(provider, &credential)?;
            credential.validate()?;
            credentials.insert(*provider, credential);
        }
        Ok(Self {
            directory: Some(directory),
            credentials: Arc::new(RwLock::new(credentials)),
            credential_revisions: Arc::new(RwLock::new(BTreeMap::new())),
            refresh_metadata: Arc::new(RwLock::new(refresh_metadata)),
            refresh_inflight: Arc::new(Mutex::new(BTreeSet::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            save_lock: Arc::new(Mutex::new(())),
        })
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Self {
            directory: None,
            credentials: Arc::new(RwLock::new(BTreeMap::new())),
            credential_revisions: Arc::new(RwLock::new(BTreeMap::new())),
            refresh_metadata: Arc::new(RwLock::new(BTreeMap::new())),
            refresh_inflight: Arc::new(Mutex::new(BTreeSet::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            save_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn status(&self, provider: &str) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        let mut status = self
            .credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)
            .map(|credentials| {
                credentials
                    .get(provider)
                    .map(ProviderCredential::presence)
                    .unwrap_or_else(|| CredentialStatus::empty(provider))
            })?;
        if let Some(metadata) = self
            .refresh_metadata
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .cloned()
        {
            status.refresh_state = match metadata.state.as_str() {
                "unavailable" => "unavailable",
                "refreshing" => "refreshing",
                "success" => "success",
                "failed" => "failed",
                _ => "idle",
            };
            status.last_refresh_at_ms = metadata.last_refresh_at_ms;
            status.next_refresh_check_at_ms = metadata.next_refresh_check_at_ms;
            status.last_refresh_error = metadata.last_error;
        }
        Ok(status)
    }

    /// 尝试取得不更新刷新状态元数据的续期租约。
    ///
    /// QQ 的请求内隐式续期也必须使用这把租约，避免与手动/自动续期并发旋转
    /// 同一份 refresh token。
    pub(crate) fn try_acquire_refresh_lease(
        &self,
        provider: &str,
    ) -> Result<Option<CredentialRefreshLease>, CredentialError> {
        let provider = canonical_provider(provider)?;
        let mut inflight = self
            .refresh_inflight
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        if !inflight.insert(provider) {
            return Ok(None);
        }
        Ok(Some(CredentialRefreshLease {
            inflight: self.refresh_inflight.clone(),
            provider,
        }))
    }

    /// 尝试取得平台续期租约并记录刷新中状态。返回 `None` 说明同平台已有续期任务。
    pub(crate) fn try_mark_refresh_started(
        &self,
        provider: &str,
    ) -> Result<Option<CredentialRefreshLease>, CredentialError> {
        let provider = canonical_provider(provider)?;
        let Some(lease) = self.try_acquire_refresh_lease(provider)? else {
            return Ok(None);
        };
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let (previous, updated) = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            let previous = metadata.get(provider).cloned();
            metadata.insert(
                provider.to_owned(),
                RefreshMetadata {
                    state: "refreshing".to_owned(),
                    next_account_status_check_at_ms: previous
                        .as_ref()
                        .and_then(|metadata| metadata.next_account_status_check_at_ms),
                    ..RefreshMetadata::default()
                },
            );
            (previous, metadata.clone())
        };
        if let Err(error) =
            self.persist_refresh_metadata(&updated)
                .map_err(|error| CredentialError::Write {
                    path: self.refresh_state_path().unwrap_or_default(),
                    error,
                })
        {
            // The durable write did not complete, so do not leave a task that never started
            // represented as permanently "refreshing" in this process.
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            match previous {
                Some(previous) => {
                    metadata.insert(provider.to_owned(), previous);
                }
                None => {
                    metadata.remove(provider);
                }
            }
            return Err(error);
        }
        Ok(Some(lease))
    }

    #[cfg(test)]
    pub fn mark_refresh_finished(
        &self,
        provider: &str,
        result: Result<(), String>,
        next_check_at_ms: Option<u64>,
    ) -> Result<(), CredentialError> {
        let provider = canonical_provider(provider)?;
        let persisted = self.write_refresh_metadata(provider, result, next_check_at_ms);
        self.release_refresh_lease(provider);
        persisted
    }

    /// 仅当同一凭据版本仍存在时写入刷新结果。
    ///
    /// 这把“检查版本 -> 更新刷新元数据”放在同一条凭据变更锁下，避免旧请求在
    /// 新登录完成后把成功状态和日常检查间隔写回新会话。
    pub(crate) fn mark_refresh_finished_if_current_revision(
        &self,
        provider: &str,
        expected_revision: Option<u64>,
        result: Result<(), String>,
        next_check_at_ms: Option<u64>,
    ) -> Result<bool, CredentialError> {
        let provider = canonical_provider(provider)?;
        let mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let current = match expected_revision {
            Some(expected_revision) => {
                let revision = self
                    .credential_revisions
                    .read()
                    .map_err(|_| CredentialError::Poisoned)?
                    .get(provider)
                    .copied()
                    .unwrap_or_default();
                let exists = self
                    .credentials
                    .read()
                    .map_err(|_| CredentialError::Poisoned)?
                    .contains_key(provider);
                exists && revision == expected_revision
            }
            None => true,
        };
        if !current {
            drop(mutation);
            // A newer login/save already owns the metadata. The old request
            // must release only its lease rather than deleting that schedule.
            self.release_refresh_lease(provider);
            return Ok(false);
        }
        let persisted = self.write_refresh_metadata(provider, result, next_check_at_ms);
        drop(mutation);
        self.release_refresh_lease(provider);
        persisted.map(|_| true)
    }

    /// Whether a configured provider is due for the background account/VIP
    /// status refresh. This schedule is intentionally independent from the
    /// credential-renewal deadline because NetEase has no credential refresh.
    pub(crate) fn account_status_check_due(
        &self,
        provider: &str,
        now_ms: u64,
    ) -> Result<bool, CredentialError> {
        let provider = canonical_provider(provider)?;
        let configured = self
            .credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .contains_key(provider);
        if !configured {
            return Ok(false);
        }
        Ok(self
            .refresh_metadata
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .and_then(|metadata| metadata.next_account_status_check_at_ms)
            .is_none_or(|next_check| next_check <= now_ms))
    }

    /// Record the next forced account/VIP status check only when the
    /// credential observed before the network request is still current.
    pub(crate) fn mark_account_status_check_finished_if_current_revision(
        &self,
        provider: &str,
        expected_revision: u64,
        next_check_at_ms: u64,
    ) -> Result<bool, CredentialError> {
        let provider = canonical_provider(provider)?;
        let mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let revision = self
            .credential_revisions
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .copied()
            .unwrap_or_default();
        let configured = self
            .credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .contains_key(provider);
        if !configured || revision != expected_revision {
            return Ok(false);
        }
        let updated = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            metadata
                .entry(provider.to_owned())
                .or_default()
                .next_account_status_check_at_ms = Some(next_check_at_ms);
            metadata.clone()
        };
        let persisted =
            self.persist_refresh_metadata(&updated)
                .map_err(|error| CredentialError::Write {
                    path: self.refresh_state_path().unwrap_or_default(),
                    error,
                });
        drop(mutation);
        persisted.map(|_| true)
    }

    fn write_refresh_metadata(
        &self,
        provider: &'static str,
        result: Result<(), String>,
        next_check_at_ms: Option<u64>,
    ) -> Result<(), CredentialError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .ok();
        let (state, error) = match result {
            Ok(()) => ("success", None),
            // refresh-state.json 会被 Web 状态接口读取，绝不能把请求 URL、cookie
            // 或 provider 原始错误持久化进去。
            Err(_) => ("failed", Some("refresh_failed".to_owned())),
        };
        let updated = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            let next_account_status_check_at_ms = metadata
                .get(provider)
                .and_then(|metadata| metadata.next_account_status_check_at_ms);
            metadata.insert(
                provider.to_owned(),
                RefreshMetadata {
                    state: state.to_owned(),
                    last_refresh_at_ms: now,
                    next_refresh_check_at_ms: next_check_at_ms,
                    next_account_status_check_at_ms,
                    last_error: error,
                },
            );
            metadata.clone()
        };
        let persisted =
            self.persist_refresh_metadata(&updated)
                .map_err(|error| CredentialError::Write {
                    path: self.refresh_state_path().unwrap_or_default(),
                    error,
                });
        persisted.map(|_| ())
    }

    fn release_refresh_lease(&self, provider: &'static str) {
        if let Ok(mut inflight) = self.refresh_inflight.lock() {
            inflight.remove(provider);
        }
    }

    /// 丢弃已被退出/重新登录取代的刷新任务，不让旧任务污染新会话的刷新状态。
    pub fn discard_refresh(&self, provider: &str) -> Result<(), CredentialError> {
        let provider = canonical_provider(provider)?;
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let updated = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            metadata.remove(provider);
            metadata.clone()
        };
        let persisted =
            self.persist_refresh_metadata(&updated)
                .map_err(|error| CredentialError::Write {
                    path: self.refresh_state_path().unwrap_or_default(),
                    error,
                });
        self.release_refresh_lease(provider);
        persisted.map(|_| ())
    }

    pub(crate) fn non_sensitive_path(&self, file_name: &str) -> Option<PathBuf> {
        self.directory
            .as_ref()
            .map(|directory| directory.join(file_name))
    }

    fn refresh_state_path(&self) -> Option<PathBuf> {
        self.non_sensitive_path(REFRESH_STATE_FILE)
    }

    fn persist_refresh_metadata(
        &self,
        metadata: &BTreeMap<String, RefreshMetadata>,
    ) -> Result<(), String> {
        let Some(path) = self.refresh_state_path() else {
            return Ok(());
        };
        let content = serde_json::to_vec_pretty(metadata).map_err(|error| error.to_string())?;
        let _guard = self
            .save_lock
            .lock()
            .map_err(|_| "save lock poisoned".to_string())?;
        write_atomic(&path, &content).map_err(|error| error.to_string())
    }

    pub fn statuses(&self) -> Result<Vec<CredentialStatus>, CredentialError> {
        SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| self.status(provider))
            .collect()
    }

    pub fn get(&self, provider: &str) -> Result<Option<ProviderCredential>, CredentialError> {
        let provider = canonical_provider(provider)?;
        self.credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)
            .map(|credentials| credentials.get(provider).cloned())
    }

    /// 读取凭据及其当前版本，供可能跨越网络 await 的续期流程使用。
    pub(crate) fn snapshot(
        &self,
        provider: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialError> {
        let provider = canonical_provider(provider)?;
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let credential = self
            .credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .cloned();
        let Some(credential) = credential else {
            return Ok(None);
        };
        let revision = self
            .credential_revisions
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .copied()
            .unwrap_or_default();
        Ok(Some(CredentialSnapshot {
            credential,
            revision,
        }))
    }

    pub fn save(
        &self,
        provider: &str,
        credential: ProviderCredential,
    ) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        validate_source(provider, &credential)?;
        credential.validate()?;
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        // 磁盘写入在 save_lock 下串行执行,且不持有 RwLock 写锁,
        // 避免 fsync/ACL 调用阻塞其他读取。
        let mut status = credential.presence();
        // 新登录/手工保存不能继承上一个会话的失败退避或“刷新中”标记；
        // 同时从当前时刻开始计算下一次日常检查，避免启动后的首个 tick 立即续期。
        let next_check_at_ms = epoch_ms().saturating_add(DAILY_REFRESH_INTERVAL_MS);
        status.next_refresh_check_at_ms = status.refresh_supported.then_some(next_check_at_ms);
        // 日程是凭据接受的一部分。先持久化它，避免磁盘失败时 UI 报告登录失败、
        // 但新凭据其实已经写入且重启后丢失每日刷新安排。
        self.schedule_initial_refresh_checks(provider, status.refresh_supported, next_check_at_ms)?;
        if let Some(directory) = self.directory.as_ref() {
            let _guard = self
                .save_lock
                .lock()
                .map_err(|_| CredentialError::Poisoned)?;
            persist_credential(&credential_path(directory, provider), &credential)?;
        }
        let mut credentials = self
            .credentials
            .write()
            .map_err(|_| CredentialError::Poisoned)?;
        credentials.insert(provider, credential);
        drop(credentials);
        self.bump_revision(provider)?;
        Ok(status)
    }

    /// 仅在凭据仍是续期请求开始时的版本时写入新凭据。
    ///
    /// 这会阻止已退出账号或已重新登录账号被旧网络任务的结果覆盖。
    pub(crate) fn save_if_revision(
        &self,
        provider: &str,
        expected_revision: u64,
        credential: ProviderCredential,
    ) -> Result<Option<CredentialStatus>, CredentialError> {
        let provider = canonical_provider(provider)?;
        validate_source(provider, &credential)?;
        credential.validate()?;
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let revision = self
            .credential_revisions
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .get(provider)
            .copied()
            .unwrap_or_default();
        let exists = self
            .credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)?
            .contains_key(provider);
        if revision != expected_revision || !exists {
            return Ok(None);
        }
        if let Some(directory) = self.directory.as_ref() {
            let _guard = self
                .save_lock
                .lock()
                .map_err(|_| CredentialError::Poisoned)?;
            persist_credential(&credential_path(directory, provider), &credential)?;
        }
        let status = credential.presence();
        self.credentials
            .write()
            .map_err(|_| CredentialError::Poisoned)?
            .insert(provider, credential);
        self.bump_revision(provider)?;
        Ok(Some(status))
    }

    pub fn remove(&self, provider: &str) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        let _mutation = self
            .mutation_lock
            .lock()
            .map_err(|_| CredentialError::Poisoned)?;
        let mut credentials = self
            .credentials
            .write()
            .map_err(|_| CredentialError::Poisoned)?;
        if let Some(directory) = self.directory.as_ref() {
            remove_credential_file(&credential_path(directory, provider))?;
        }
        credentials.remove(provider);
        drop(credentials);
        self.bump_revision(provider)?;
        let _ = self.clear_refresh_metadata(provider);
        Ok(CredentialStatus::empty(provider))
    }

    fn bump_revision(&self, provider: &'static str) -> Result<(), CredentialError> {
        let mut revisions = self
            .credential_revisions
            .write()
            .map_err(|_| CredentialError::Poisoned)?;
        let revision = revisions.entry(provider).or_default();
        *revision = revision.wrapping_add(1);
        Ok(())
    }

    fn clear_refresh_metadata(&self, provider: &'static str) -> Result<(), CredentialError> {
        let updated = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            if metadata.remove(provider).is_none() {
                return Ok(());
            }
            metadata.clone()
        };
        self.persist_refresh_metadata(&updated)
            .map_err(|error| CredentialError::Write {
                path: self.refresh_state_path().unwrap_or_default(),
                error,
            })
    }

    fn schedule_initial_refresh_checks(
        &self,
        provider: &'static str,
        refresh_supported: bool,
        next_check_at_ms: u64,
    ) -> Result<(), CredentialError> {
        let updated = {
            let mut metadata = self
                .refresh_metadata
                .write()
                .map_err(|_| CredentialError::Poisoned)?;
            metadata.insert(
                provider.to_owned(),
                RefreshMetadata {
                    state: "idle".to_owned(),
                    last_refresh_at_ms: None,
                    next_refresh_check_at_ms: refresh_supported.then_some(next_check_at_ms),
                    next_account_status_check_at_ms: Some(next_check_at_ms),
                    last_error: None,
                },
            );
            metadata.clone()
        };
        self.persist_refresh_metadata(&updated)
            .map_err(|error| CredentialError::Write {
                path: self.refresh_state_path().unwrap_or_default(),
                error,
            })
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn canonical_provider(provider: &str) -> Result<&'static str, CredentialError> {
    SUPPORTED_PROVIDERS
        .iter()
        .copied()
        .find(|candidate| *candidate == provider)
        .ok_or_else(|| CredentialError::UnknownProvider(provider.to_owned()))
}

fn validate_source(
    requested_provider: &str,
    credential: &ProviderCredential,
) -> Result<(), CredentialError> {
    if credential.provider() == requested_provider {
        return Ok(());
    }
    Err(CredentialError::ProviderMismatch {
        requested: requested_provider.to_owned(),
        actual: credential.provider().to_owned(),
    })
}

fn require_any_cookie(
    cookies: &BTreeMap<String, String>,
    name: &str,
    candidates: &[&str],
) -> Result<(), CredentialError> {
    if has_any_cookie(cookies, candidates) {
        return Ok(());
    }
    Err(CredentialError::Invalid(format!(
        "{name} cookie must not be empty"
    )))
}

fn require_allowed_cookie_names(
    cookies: &BTreeMap<String, String>,
    allowed: &[&str],
) -> Result<(), CredentialError> {
    if let Some(name) = cookies
        .keys()
        .find(|name| !allowed.contains(&name.as_str()))
    {
        return Err(CredentialError::Invalid(format!(
            "unsupported credential cookie field {name}"
        )));
    }
    Ok(())
}

fn has_any_cookie(cookies: &BTreeMap<String, String>, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| {
        cookies
            .get(*candidate)
            .is_some_and(|value| has_value(value))
    })
}

fn has_value(value: &str) -> bool {
    !value.trim().is_empty()
}

fn qq_refresh_ready(cookies: &BTreeMap<String, String>) -> bool {
    let web_ready = has_any_cookie(cookies, &["wxopenid"])
        && has_any_cookie(cookies, &["wxrefresh_token"])
        && has_any_cookie(cookies, &["qqmusic_key", "qm_keyst"])
        && has_any_cookie(cookies, &["uin", "wxuin"]);
    // QQ 网页登录（QQ OAuth）通常只下发 refresh_token，不保证下发
    // refresh_key；移动端续期接口接受两者任一作为已有登录凭据，并在响应中
    // 补发新的 refresh_token 与 refresh_key。因此任一存在即可刷新。
    let oauth_ready = has_any_cookie(cookies, &["uin", "wxuin"])
        && has_any_cookie(cookies, &["qqmusic_key", "qm_keyst"])
        && has_any_cookie(cookies, &["psrf_qqopenid", "openid"])
        && has_any_cookie(cookies, &["psrf_qqaccess_token", "access_token"])
        && (has_any_cookie(cookies, &["psrf_qqrefresh_token", "refresh_token"])
            || has_any_cookie(cookies, &["psrf_qqrefresh_key", "refresh_key"]));
    web_ready || oauth_ready
}

fn credential_path(directory: &Path, provider: &str) -> PathBuf {
    directory.join(format!("{provider}.json"))
}

fn load_refresh_metadata(
    directory: &Path,
) -> Result<BTreeMap<String, RefreshMetadata>, CredentialError> {
    let path = directory.join(REFRESH_STATE_FILE);
    restore_interrupted_write(&path)?;
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    secure_credential_file(&path)?;
    let text = fs::read_to_string(&path).map_err(|error| CredentialError::Read {
        path: path.clone(),
        error: error.to_string(),
    })?;
    let mut metadata: BTreeMap<String, RefreshMetadata> = serde_json::from_str(&text)
        .map_err(|error| CredentialError::Invalid(format!("{}: {error}", path.display())))?;
    // 旧版本可能将 provider 原始错误写入该文件。状态会直接被 Web 面板读取，
    // 因此加载时迁移为稳定错误码并立即落盘，避免升级后继续暴露旧的 URL 或凭据。
    let migrated = metadata.values_mut().fold(false, |changed, entry| {
        if entry.last_error.as_deref() == Some("refresh_failed") {
            changed
        } else if entry.last_error.is_some() {
            entry.last_error = Some("refresh_failed".to_owned());
            true
        } else {
            changed
        }
    });
    if migrated {
        let content = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| CredentialError::Invalid(error.to_string()))?;
        write_atomic(&path, &content)?;
    }
    Ok(metadata)
}

fn persist_credential(path: &Path, credential: &ProviderCredential) -> Result<(), CredentialError> {
    if let Some(parent) = path.parent() {
        ensure_credential_directory(parent)?;
    }
    let content = serde_json::to_vec_pretty(&CredentialEnvelope {
        schema_version: CREDENTIAL_SCHEMA_VERSION,
        credential: credential.clone(),
    })
    .map_err(|error| CredentialError::Invalid(error.to_string()))?;
    write_atomic(path, &content)
}

fn ensure_credential_directory(path: &Path) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .map_err(|error| CredentialError::Write {
                path: path.to_path_buf(),
                error: error.to_string(),
            })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            CredentialError::Write {
                path: path.to_path_buf(),
                error: error.to_string(),
            }
        })
    }
    #[cfg(windows)]
    {
        fs::create_dir_all(path).map_err(|error| CredentialError::Write {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
        secure_windows_path(path)
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        fs::create_dir_all(path).map_err(|error| CredentialError::Write {
            path: path.to_path_buf(),
            error: error.to_string(),
        })
    }
}

fn secure_credential_file(path: &Path) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            CredentialError::Write {
                path: path.to_path_buf(),
                error: error.to_string(),
            }
        })
    }
    #[cfg(windows)]
    {
        secure_windows_path(path)
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(windows)]
fn secure_windows_path(path: &Path) -> Result<(), CredentialError> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    fn write_error(path: &Path, operation: &str, error: impl std::fmt::Display) -> CredentialError {
        CredentialError::Write {
            path: path.to_path_buf(),
            error: format!("{operation}: {error}"),
        }
    }

    let mut token = HANDLE::default();
    // The SID buffer remains alive until SetNamedSecurityInfoW has copied the ACL.
    let result = (|| -> Result<(), CredentialError> {
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|error| write_error(path, "open current process token", error))?;

            let mut token_bytes = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut token_bytes);
            if token_bytes == 0 {
                return Err(write_error(
                    path,
                    "measure current user token",
                    "Windows returned an empty TOKEN_USER buffer",
                ));
            }
            let mut token_buffer = vec![0u8; token_bytes as usize];
            GetTokenInformation(
                token,
                TokenUser,
                Some(token_buffer.as_mut_ptr().cast::<c_void>()),
                token_bytes,
                &mut token_bytes,
            )
            .map_err(|error| write_error(path, "read current user token", error))?;
            let token_user = &*token_buffer.as_ptr().cast::<TOKEN_USER>();
            let sid: PSID = token_user.User.Sid;
            if sid.is_invalid() {
                return Err(write_error(
                    path,
                    "read current user SID",
                    "Windows returned a null SID",
                ));
            }

            let access = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS.0,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: if path.is_dir() {
                    SUB_CONTAINERS_AND_OBJECTS_INHERIT
                } else {
                    NO_INHERITANCE
                },
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    ptstrName: PWSTR(sid.0.cast::<u16>()),
                },
            };
            let mut acl = null_mut();
            let acl_status = SetEntriesInAclW(Some(&[access]), None, &mut acl);
            if acl_status != ERROR_SUCCESS {
                return Err(write_error(
                    path,
                    "build current-user credential ACL",
                    format!("Win32 error {}", acl_status.0),
                ));
            }

            let wide_path = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let status = SetNamedSecurityInfoW(
                PCWSTR(wide_path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl.cast_const()),
                None,
            );
            let _ = LocalFree(Some(HLOCAL(acl.cast())));
            if status != ERROR_SUCCESS {
                Err(write_error(
                    path,
                    "set current-user credential ACL",
                    format!("Win32 error {}", status.0),
                ))
            } else {
                Ok(())
            }
        }
    })();
    if !token.is_invalid() {
        unsafe {
            let _ = CloseHandle(token);
        }
    }
    result
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<(), CredentialError> {
    let temporary = path.with_extension("tmp");
    let backup = path.with_extension("bak");
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| CredentialError::Write {
            path: temporary.clone(),
            error: error.to_string(),
        })?;
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|error| CredentialError::Write {
            path: temporary.clone(),
            error: error.to_string(),
        })?;
    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup).map_err(|error| CredentialError::Write {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(CredentialError::Write {
            path: path.to_path_buf(),
            error: error.to_string(),
        });
    }
    let _ = fs::remove_file(backup);
    secure_credential_file(path)
}

fn restore_interrupted_write(path: &Path) -> Result<(), CredentialError> {
    if path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("bak");
    if backup.exists() {
        fs::rename(&backup, path).map_err(|error| CredentialError::Restore {
            path: path.to_path_buf(),
            backup,
            error: error.to_string(),
        })?;
    }
    Ok(())
}

fn remove_credential_file(path: &Path) -> Result<(), CredentialError> {
    for candidate in [
        path.to_path_buf(),
        path.with_extension("tmp"),
        path.with_extension("bak"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CredentialError::Write {
                    path: candidate,
                    error: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("unknown provider credential: {0}")]
    UnknownProvider(String),
    #[error("unsupported credential schema in {path}: expected {expected}, got {actual:?}")]
    UnsupportedSchema {
        path: PathBuf,
        expected: u32,
        actual: Option<u32>,
    },
    #[error("credential kind does not match provider {requested}: got {actual}")]
    ProviderMismatch { requested: String, actual: String },
    #[error("invalid provider credential: {0}")]
    Invalid(String),
    #[error("login session is missing or invalid")]
    LoginSessionInvalid,
    #[error("credential validation failed for {provider}: {message}")]
    ValidationFailed {
        provider: String,
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("cannot read provider credential {path}: {error}")]
    Read { path: PathBuf, error: String },
    #[error("cannot write provider credential {path}: {error}")]
    Write { path: PathBuf, error: String },
    #[error("cannot restore provider credential {path} from {backup}: {error}")]
    Restore {
        path: PathBuf,
        backup: PathBuf,
        error: String,
    },
    #[error("provider credential state is unavailable")]
    Poisoned,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(windows)]
    use std::path::Path;

    use super::{
        CredentialError, CredentialStore, DAILY_REFRESH_INTERVAL_MS, ProviderCredential,
        REFRESH_STATE_FILE, epoch_ms, qq_refresh_ready,
    };

    #[test]
    fn qq_refresh_requires_a_complete_supported_credential_set() {
        let basic = BTreeMap::from([
            ("uin".to_owned(), "123".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
        ]);
        let mut oauth = basic.clone();
        oauth.insert("psrf_qqopenid".to_owned(), "openid".to_owned());
        oauth.insert("psrf_qqaccess_token".to_owned(), "access".to_owned());
        assert!(!qq_refresh_ready(&oauth));
        // 网页登录只下发 refresh_token（无 refresh_key）时即可刷新。
        oauth.insert("psrf_qqrefresh_token".to_owned(), "refresh".to_owned());
        assert!(qq_refresh_ready(&oauth));
        oauth.remove("psrf_qqrefresh_token");
        oauth.insert("psrf_qqrefresh_key".to_owned(), "key".to_owned());
        assert!(qq_refresh_ready(&oauth));

        let mut web = basic;
        web.insert("wxopenid".to_owned(), "openid".to_owned());
        assert!(!qq_refresh_ready(&web));
        web.insert("wxrefresh_token".to_owned(), "refresh".to_owned());
        assert!(qq_refresh_ready(&web));
    }

    #[test]
    fn saving_a_credential_defers_daily_refresh_and_account_status_checks() {
        let store = CredentialStore::memory();
        let before = epoch_ms();

        let status = store
            .save(
                "kugou",
                ProviderCredential::Kugou {
                    token: "token".to_owned(),
                    userid: "42".to_owned(),
                    dfid: "device-42".to_owned(),
                    cookies: BTreeMap::new(),
                },
            )
            .unwrap();
        let after = epoch_ms();
        let next = status
            .next_refresh_check_at_ms
            .expect("refresh-capable credentials must receive a due time");

        assert!(next >= before.saturating_add(DAILY_REFRESH_INTERVAL_MS));
        assert!(next <= after.saturating_add(DAILY_REFRESH_INTERVAL_MS));
        assert!(!store.account_status_check_due("kugou", after).unwrap());
        assert!(store.account_status_check_due("kugou", next).unwrap());
    }

    #[test]
    fn saved_daily_schedule_survives_restart() {
        let directory = std::env::temp_dir().join(format!(
            "miliastra-credentials-schedule-{}",
            uuid::Uuid::new_v4()
        ));
        let store = CredentialStore::open(directory.clone()).unwrap();
        let status = store
            .save(
                "kugou",
                ProviderCredential::Kugou {
                    token: "token".to_owned(),
                    userid: "42".to_owned(),
                    dfid: "device-42".to_owned(),
                    cookies: BTreeMap::new(),
                },
            )
            .unwrap();
        let next = status.next_refresh_check_at_ms.unwrap();
        drop(store);

        let reopened = CredentialStore::open(directory.clone()).unwrap();
        assert_eq!(
            reopened.status("kugou").unwrap().next_refresh_check_at_ms,
            Some(next)
        );
        assert!(
            !reopened
                .account_status_check_due("kugou", epoch_ms())
                .unwrap()
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn conditional_refresh_save_cannot_restore_a_logged_out_credential() {
        let store = CredentialStore::memory();
        let original = ProviderCredential::QqMusic {
            cookies: BTreeMap::from([
                ("uin".to_owned(), "123".to_owned()),
                ("qqmusic_key".to_owned(), "old-key".to_owned()),
            ]),
        };
        store.save("qqmusic", original).unwrap();
        let snapshot = store.snapshot("qqmusic").unwrap().unwrap();

        store.remove("qqmusic").unwrap();
        let saved = store
            .save_if_revision(
                "qqmusic",
                snapshot.revision,
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "123".to_owned()),
                        ("qqmusic_key".to_owned(), "new-key".to_owned()),
                    ]),
                },
            )
            .unwrap();

        assert!(saved.is_none());
        assert!(store.get("qqmusic").unwrap().is_none());
    }

    #[test]
    fn refresh_lease_is_single_flight_and_never_persists_raw_errors() {
        let store = CredentialStore::memory();
        let lease = store
            .try_mark_refresh_started("qqmusic")
            .unwrap()
            .expect("first refresh should obtain a lease");
        assert!(store.try_mark_refresh_started("qqmusic").unwrap().is_none());
        store
            .mark_refresh_finished(
                "qqmusic",
                Err("request https://example.invalid/?token=secret-value failed".to_owned()),
                Some(123),
            )
            .unwrap();
        drop(lease);

        let status = store.status("qqmusic").unwrap();
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(status.last_refresh_error.as_deref(), Some("refresh_failed"));
        assert!(!serialized.contains("secret-value"));
        assert!(store.try_mark_refresh_started("qqmusic").unwrap().is_some());
    }

    #[test]
    fn failed_refresh_start_rolls_back_metadata_and_releases_its_lease() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(directory.clone()).unwrap();
        // `write_atomic` cannot create a file at an existing directory. This causes the durable
        // refresh-state write to fail without depending on platform-specific file permissions.
        fs::create_dir(directory.join("refresh-state.tmp")).unwrap();

        assert!(store.try_mark_refresh_started("qqmusic").is_err());
        let status = store.status("qqmusic").unwrap();
        assert_eq!(status.refresh_state, "unavailable");
        assert!(status.last_refresh_at_ms.is_none());
        assert!(status.last_refresh_error.is_none());

        let lease = store
            .try_acquire_refresh_lease("qqmusic")
            .unwrap()
            .expect("the failed start must not leave the provider leased");
        drop(lease);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saving_a_credential_requires_the_daily_schedule_to_persist_first() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(directory.clone()).unwrap();
        // `write_atomic` cannot create a temporary file at this directory.
        fs::create_dir(directory.join("refresh-state.tmp")).unwrap();

        let result = store.save(
            "qqmusic",
            ProviderCredential::QqMusic {
                cookies: BTreeMap::from([
                    ("uin".to_owned(), "42".to_owned()),
                    ("qqmusic_key".to_owned(), "key".to_owned()),
                ]),
            },
        );

        assert!(result.is_err());
        assert!(store.get("qqmusic").unwrap().is_none());
        assert!(!directory.join("qqmusic.json").exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_refresh_errors_are_sanitized_and_rewritten_on_open() {
        let directory = std::env::temp_dir().join(format!(
            "miliastra-refresh-metadata-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let secret =
            "https://provider.invalid/refresh?token=private-token-value&cookie=session-secret";
        fs::write(
            directory.join(REFRESH_STATE_FILE),
            format!(r#"{{"qqmusic":{{"state":"failed","last_error":"{secret}"}}}}"#),
        )
        .unwrap();

        let store = CredentialStore::open(directory.clone()).unwrap();
        let status = store.status("qqmusic").unwrap();
        assert_eq!(status.last_refresh_error.as_deref(), Some("refresh_failed"));

        let persisted = fs::read_to_string(directory.join(REFRESH_STATE_FILE)).unwrap();
        assert!(persisted.contains("refresh_failed"));
        assert!(!persisted.contains(secret));
        assert!(!persisted.contains("private-token-value"));
        assert!(!persisted.contains("session-secret"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn providers_are_saved_in_independent_plaintext_files() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(directory.clone()).unwrap();
        store
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "123".to_owned()),
                        ("qqmusic_key".to_owned(), "secret".to_owned()),
                    ]),
                },
            )
            .unwrap();
        store
            .save(
                "netease",
                ProviderCredential::Netease {
                    cookies: BTreeMap::from([("MUSIC_U".to_owned(), "netease-secret".to_owned())]),
                },
            )
            .unwrap();
        store
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("SESSDATA".to_owned(), "bili-secret".to_owned())]),
                    refresh_token: None,
                },
            )
            .unwrap();
        assert!(directory.join("qqmusic.json").exists());
        assert!(directory.join("netease.json").exists());
        assert!(directory.join("bilibili.json").exists());
        assert_eq!(store.statuses().unwrap().len(), 4);
        let persisted: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(directory.join("qqmusic.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["schemaVersion"], 2);
        assert_eq!(persisted["credential"]["kind"], "qqMusic");
        assert!(
            !serde_json::to_string(&store.status("qqmusic").unwrap())
                .unwrap()
                .contains("secret")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn kugou_credentials_require_all_session_fields_without_exposing_values() {
        let store = CredentialStore::memory();
        assert!(
            store
                .save(
                    "kugou",
                    ProviderCredential::Kugou {
                        token: "token".to_owned(),
                        userid: "123".to_owned(),
                        dfid: String::new(),
                        cookies: BTreeMap::new(),
                    },
                )
                .is_err()
        );
        let status = store
            .save(
                "kugou",
                ProviderCredential::Kugou {
                    token: "token-secret".to_owned(),
                    userid: "123".to_owned(),
                    dfid: "dfid-secret".to_owned(),
                    cookies: BTreeMap::from([("KugouGUID".to_owned(), "guid".to_owned())]),
                },
            )
            .unwrap();
        assert_eq!(status.fields.get("token"), Some(&true));
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("token-secret")
        );
    }

    #[cfg(windows)]
    #[test]
    fn credential_directory_and_files_are_restricted_to_the_current_user() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(directory.clone()).unwrap();
        store
            .save(
                "netease",
                ProviderCredential::Netease {
                    cookies: BTreeMap::from([("MUSIC_U".to_owned(), "secret".to_owned())]),
                },
            )
            .unwrap();

        assert_private_windows_acl(&directory);
        assert_private_windows_acl(&directory.join("netease.json"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    fn assert_private_windows_acl(path: &Path) {
        use std::ffi::c_void;
        use std::mem::size_of;
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;

        use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID,
            SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        };
        use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        use windows::core::PCWSTR;

        let wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let status = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(wide_path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "{}", path.display());
        assert!(!dacl.is_null(), "{}", path.display());

        let mut control = 0u16;
        let mut revision = 0u32;
        unsafe {
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision).unwrap();
        }
        assert_ne!(control & SE_DACL_PROTECTED.0, 0, "{}", path.display());

        let mut acl_info = ACL_SIZE_INFORMATION::default();
        unsafe {
            GetAclInformation(
                dacl,
                (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
            .unwrap();
        }
        assert_eq!(acl_info.AceCount, 1, "{}", path.display());
        let mut ace_pointer = null_mut();
        unsafe {
            GetAce(dacl, 0, &mut ace_pointer).unwrap();
        }
        let ace = unsafe { &*ace_pointer.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(ace.Mask, FILE_ALL_ACCESS.0, "{}", path.display());
        let ace_sid = PSID((&ace.SidStart as *const u32).cast_mut().cast::<c_void>());

        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap();
        }
        let mut token_bytes = 0;
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut token_bytes);
        }
        let mut token_buffer = vec![0u8; token_bytes as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(token_buffer.as_mut_ptr().cast::<c_void>()),
                token_bytes,
                &mut token_bytes,
            )
            .unwrap();
        }
        let token_user = unsafe { &*token_buffer.as_ptr().cast::<TOKEN_USER>() };
        unsafe {
            EqualSid(ace_sid, token_user.User.Sid).unwrap();
            CloseHandle(token).unwrap();
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
    }

    #[test]
    fn old_short_provider_names_are_rejected() {
        let store = CredentialStore::memory();
        assert!(store.status("tx").is_err());
        assert!(store.status("kuwo").is_err());
    }

    #[test]
    fn unversioned_credential_files_are_rejected_without_migration() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("netease.json"),
            r#"{"kind":"netease","cookies":{"MUSIC_U":"secret"}}"#,
        )
        .unwrap();

        let error = match CredentialStore::open(directory.clone()) {
            Ok(_) => panic!("unversioned credential file must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            CredentialError::UnsupportedSchema { actual: None, .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn credentials_reject_cookie_fields_outside_the_provider_allowlist() {
        let store = CredentialStore::memory();
        let error = store
            .save(
                "netease",
                ProviderCredential::Netease {
                    cookies: BTreeMap::from([
                        ("MUSIC_U".to_owned(), "secret".to_owned()),
                        ("unrelated".to_owned(), "must-not-persist".to_owned()),
                    ]),
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported credential cookie field")
        );
        assert!(!store.status("netease").unwrap().configured);
    }

    #[test]
    fn bilibili_credentials_require_sessdata_and_only_persist_known_fields() {
        let store = CredentialStore::memory();
        let error = store
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("bili_jct".to_owned(), "csrf".to_owned())]),
                    refresh_token: None,
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sessdata cookie must not be empty")
        );

        let status = store
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([
                        ("SESSDATA".to_owned(), "session".to_owned()),
                        ("bili_jct".to_owned(), "csrf".to_owned()),
                    ]),
                    refresh_token: Some("refresh".to_owned()),
                },
            )
            .unwrap();
        assert!(status.configured);
        assert_eq!(status.fields.get("sessdata"), Some(&true));
        assert_eq!(status.fields.get("refreshToken"), Some(&true));
    }

    #[test]
    fn bilibili_legacy_refresh_cookie_format_is_rejected() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("bilibili.json"),
            r#"{"schemaVersion":2,"credential":{"kind":"bilibili","cookies":{"SESSDATA":"session","ac_time_value":"legacy-refresh"}}}"#,
        )
        .unwrap();

        let error = match CredentialStore::open(directory.clone()) {
            Ok(_) => panic!("legacy Bilibili credential must be rejected"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("unsupported credential cookie field ac_time_value")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schema_version_one_credentials_are_rejected() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("bilibili.json"),
            r#"{"schemaVersion":1,"credential":{"kind":"bilibili","cookies":{"SESSDATA":"session"},"refreshToken":"refresh"}}"#,
        )
        .unwrap();

        let error = match CredentialStore::open(directory.clone()) {
            Ok(_) => panic!("schema version one must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            CredentialError::UnsupportedSchema {
                expected: 2,
                actual: Some(1),
                ..
            }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bilibili_persistence_keeps_refresh_token_out_of_cookies_and_status() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-credentials-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(directory.clone()).unwrap();
        store
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]),
                    refresh_token: Some("independent-refresh".to_owned()),
                },
            )
            .unwrap();

        let persisted = fs::read_to_string(directory.join("bilibili.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["credential"]["refreshToken"], "independent-refresh");
        assert!(
            value["credential"]["cookies"]
                .get("ac_time_value")
                .is_none()
        );
        let status = serde_json::to_string(&store.status("bilibili").unwrap()).unwrap();
        assert!(status.contains("\"refreshToken\":true"));
        assert!(!status.contains("independent-refresh"));
        fs::remove_dir_all(directory).unwrap();
    }
}
