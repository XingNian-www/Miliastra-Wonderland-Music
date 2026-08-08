use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SUPPORTED_PROVIDERS: &[&str] = &["qqmusic", "netease", "bilibili"];
const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Plaintext account state captured by the provider login helper. The store
/// never exposes secret values through its status APIs; native adapters read a
/// clone only when they need to make a provider request.
#[derive(Clone, Deserialize, Serialize)]
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
    Bilibili { cookies: BTreeMap<String, String> },
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
        }
    }

    pub fn cookies(&self) -> &BTreeMap<String, String> {
        match self {
            Self::QqMusic { cookies } | Self::Netease { cookies } | Self::Bilibili { cookies } => {
                cookies
            }
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
            Self::Bilibili { cookies } => {
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
                        // Bilibili stores this refresh value in localStorage. It is
                        // optional today, but may be retained by a future capture flow.
                        "ac_time_value",
                    ],
                )?;
                require_any_cookie(cookies, "sessdata", &["SESSDATA"])?;
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
            Self::Bilibili { cookies } => BTreeMap::from([
                ("sessdata", has_any_cookie(cookies, &["SESSDATA"])),
                ("csrf", has_any_cookie(cookies, &["bili_jct"])),
                ("userId", has_any_cookie(cookies, &["DedeUserID"])),
                ("refreshToken", has_any_cookie(cookies, &["ac_time_value"])),
                ("buvid3", has_any_cookie(cookies, &["buvid3"])),
            ]),
        };
        CredentialStatus {
            provider: self.provider(),
            configured: true,
            fields,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatus {
    pub provider: &'static str,
    pub configured: bool,
    pub fields: BTreeMap<&'static str, bool>,
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
            _ => BTreeMap::new(),
        };
        Self {
            provider,
            configured: false,
            fields,
        }
    }
}

#[derive(Clone)]
pub struct CredentialStore {
    directory: Option<PathBuf>,
    credentials: Arc<RwLock<BTreeMap<&'static str, ProviderCredential>>>,
}

impl CredentialStore {
    pub fn open(directory: PathBuf) -> Result<Self, CredentialError> {
        ensure_credential_directory(&directory)?;
        let mut credentials = BTreeMap::new();
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
            let credential =
                serde_json::from_str::<ProviderCredential>(&text).map_err(|error| {
                    CredentialError::Invalid(format!("{}: {error}", path.display()))
                })?;
            validate_source(provider, &credential)?;
            credential.validate()?;
            credentials.insert(*provider, credential);
        }
        Ok(Self {
            directory: Some(directory),
            credentials: Arc::new(RwLock::new(credentials)),
        })
    }

    pub fn memory() -> Self {
        Self {
            directory: None,
            credentials: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub fn status(&self, provider: &str) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        self.credentials
            .read()
            .map_err(|_| CredentialError::Poisoned)
            .map(|credentials| {
                credentials
                    .get(provider)
                    .map(ProviderCredential::presence)
                    .unwrap_or_else(|| CredentialStatus::empty(provider))
            })
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

    pub fn save(
        &self,
        provider: &str,
        credential: ProviderCredential,
    ) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        validate_source(provider, &credential)?;
        credential.validate()?;
        let mut credentials = self
            .credentials
            .write()
            .map_err(|_| CredentialError::Poisoned)?;
        if let Some(directory) = self.directory.as_ref() {
            persist_credential(&credential_path(directory, provider), &credential)?;
        }
        let status = credential.presence();
        credentials.insert(provider, credential);
        Ok(status)
    }

    pub fn remove(&self, provider: &str) -> Result<CredentialStatus, CredentialError> {
        let provider = canonical_provider(provider)?;
        let mut credentials = self
            .credentials
            .write()
            .map_err(|_| CredentialError::Poisoned)?;
        if let Some(directory) = self.directory.as_ref() {
            remove_credential_file(&credential_path(directory, provider))?;
        }
        credentials.remove(provider);
        Ok(CredentialStatus::empty(provider))
    }
}

fn canonical_provider(provider: &str) -> Result<&'static str, CredentialError> {
    SUPPORTED_PROVIDERS
        .iter()
        .copied()
        .find(|candidate| *candidate == provider)
        .ok_or_else(|| {
            if matches!(provider, "kuwo" | "kugou") {
                CredentialError::ProviderNotCompiled(provider.to_owned())
            } else {
                CredentialError::UnknownProvider(provider.to_owned())
            }
        })
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
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn credential_path(directory: &Path, provider: &str) -> PathBuf {
    directory.join(format!("{provider}.json"))
}

fn persist_credential(path: &Path, credential: &ProviderCredential) -> Result<(), CredentialError> {
    if let Some(parent) = path.parent() {
        ensure_credential_directory(parent)?;
    }
    let content = serde_json::to_vec_pretty(credential)
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
    #[cfg(not(unix))]
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
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
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
    #[error("provider credential adapter is not compiled: {0}")]
    ProviderNotCompiled(String),
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

    use super::{CredentialStore, ProviderCredential};

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
                },
            )
            .unwrap();
        assert!(directory.join("qqmusic.json").exists());
        assert!(directory.join("netease.json").exists());
        assert!(directory.join("bilibili.json").exists());
        assert!(
            !serde_json::to_string(&store.status("qqmusic").unwrap())
                .unwrap()
                .contains("secret")
        );
        assert_eq!(store.statuses().unwrap().len(), 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn old_short_provider_names_are_rejected() {
        let store = CredentialStore::memory();
        assert!(store.status("tx").is_err());
        assert!(store.status("kuwo").is_err());
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
                        ("ac_time_value".to_owned(), "refresh".to_owned()),
                    ]),
                },
            )
            .unwrap();
        assert!(status.configured);
        assert_eq!(status.fields.get("sessdata"), Some(&true));
        assert_eq!(status.fields.get("refreshToken"), Some(&true));
    }
}
