use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use miliastra_login_protocol::encode_message;
use miliastra_login_protocol::{
    CredentialPayload, LoginHelperMessage, MAX_LOGIN_MESSAGE_BYTES, ProtocolError, decode_message,
};
#[cfg(test)]
use miliastra_playback::Failure;
use miliastra_playback::{
    CredentialStatus, LoginOperation, LoginOperationWaitError, LoginSession, PlaybackError,
    PlaybackHandle, ProviderCredential, ProviderId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const REAPER_POLL: Duration = Duration::from_millis(25);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
/// 等待登录助手输出线程退出的上限;超过则分离(孙进程可能持有管道写端)。
const READER_JOIN_GRACE: Duration = Duration::from_secs(3);
const PROFILE_ROOT_NAME: &str = ".login-profiles";

/// The application owns the login lease and credential store. Keeping this
/// small trait here makes the process lifecycle testable without starting the
/// FFmpeg runtime or a WebView2 window.
trait LoginPlaybackPort: Send + Sync {
    fn credential_statuses(&self) -> Result<Vec<CredentialStatus>, PlaybackError>;
    fn refresh_credential(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError>;
    fn kugou_status(&self) -> Result<miliastra_playback::KugouAccountStatus, PlaybackError>;
    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError>;
    fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError>;
    fn kugou_claim_vip(&self) -> Result<miliastra_playback::KugouListenReport, PlaybackError>;
    fn kugou_upgrade_vip(&self) -> Result<miliastra_playback::KugouListenReport, PlaybackError>;
    fn begin_login(&self, provider: ProviderId) -> Result<LoginSession, PlaybackError>;
    fn complete_login(
        &self,
        session_id: Uuid,
        credential: ProviderCredential,
    ) -> Result<Box<dyn PendingLoginCompletion>, PlaybackError>;
    fn cancel_login(&self, session_id: Uuid) -> Result<(), PlaybackError>;
    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError>;
}

enum PendingLoginPoll {
    Completed(Result<CredentialStatus, PlaybackError>),
    TimedOut,
}

trait PendingLoginCompletion: Send {
    fn wait_timeout(&self, timeout: Duration) -> PendingLoginPoll;
}

struct NativeLoginCompletion(LoginOperation);

impl PendingLoginCompletion for NativeLoginCompletion {
    fn wait_timeout(&self, timeout: Duration) -> PendingLoginPoll {
        match self.0.wait_timeout(timeout) {
            Ok(status) => PendingLoginPoll::Completed(Ok(status)),
            Err(LoginOperationWaitError::TimedOut) => PendingLoginPoll::TimedOut,
            Err(LoginOperationWaitError::RuntimeStopped) => {
                PendingLoginPoll::Completed(Err(PlaybackError::RuntimeStopped))
            }
            Err(LoginOperationWaitError::Playback(error)) => {
                PendingLoginPoll::Completed(Err(error))
            }
        }
    }
}

impl LoginPlaybackPort for PlaybackHandle {
    fn credential_statuses(&self) -> Result<Vec<CredentialStatus>, PlaybackError> {
        PlaybackHandle::credential_statuses(self)
    }

    fn refresh_credential(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError> {
        PlaybackHandle::refresh_credential(self, provider)
    }

    fn kugou_status(&self) -> Result<miliastra_playback::KugouAccountStatus, PlaybackError> {
        PlaybackHandle::kugou_account_status(self)
    }

    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError> {
        PlaybackHandle::account_status(self, provider)
    }

    fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError> {
        PlaybackHandle::refresh_account_status(self, provider)
    }

    fn kugou_claim_vip(&self) -> Result<miliastra_playback::KugouListenReport, PlaybackError> {
        PlaybackHandle::kugou_claim_vip(self)
    }

    fn kugou_upgrade_vip(&self) -> Result<miliastra_playback::KugouListenReport, PlaybackError> {
        PlaybackHandle::kugou_upgrade_vip(self)
    }

    fn begin_login(&self, provider: ProviderId) -> Result<LoginSession, PlaybackError> {
        PlaybackHandle::begin_login(self, provider)
    }

    fn complete_login(
        &self,
        session_id: Uuid,
        credential: ProviderCredential,
    ) -> Result<Box<dyn PendingLoginCompletion>, PlaybackError> {
        PlaybackHandle::complete_login(self, session_id, credential)
            .map(|operation| Box::new(NativeLoginCompletion(operation)) as Box<_>)
    }

    fn cancel_login(&self, session_id: Uuid) -> Result<(), PlaybackError> {
        PlaybackHandle::cancel_login(self, session_id)
    }

    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError> {
        PlaybackHandle::logout(self, provider)
    }
}

trait ManagedChild: Send {
    /// Returns `Some(success)` once the child has been reaped.
    fn try_wait(&mut self) -> io::Result<Option<bool>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<bool>;
}

struct SpawnedHelper {
    child: Box<dyn ManagedChild>,
    stdout: Box<dyn Read + Send>,
    /// 持续读取以避免 sidecar 的 stderr 管道写满后阻塞。
    stderr: Box<dyn Read + Send>,
}

trait HelperLauncher: Send + Sync {
    fn spawn(
        &self,
        provider: ProviderId,
        profile: &Path,
        timeout: Duration,
    ) -> io::Result<SpawnedHelper>;
}

trait KugouDeviceRegistrar: Send + Sync {
    fn register(
        &self,
        token: &str,
        userid: &str,
        cookies: &BTreeMap<String, String>,
    ) -> Result<String, LoginHelperFailure>;
}

struct DirectKugouDeviceRegistrar;

impl KugouDeviceRegistrar for DirectKugouDeviceRegistrar {
    fn register(
        &self,
        token: &str,
        userid: &str,
        cookies: &BTreeMap<String, String>,
    ) -> Result<String, LoginHelperFailure> {
        miliastra_playback::kugou_register_device(token, userid, cookies).map_err(|_error| {
            LoginHelperFailure {
                code: "kugou_device_registration_failed".to_owned(),
                message: "酷狗设备注册失败，请重试".to_owned(),
                provider: Some(ProviderId::Kugou),
            }
        })
    }
}

struct CommandHelperLauncher {
    executable: PathBuf,
    credential_directory: PathBuf,
}

const KUGOU_DEVICE_FILE: &str = "kugou-device.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KugouDeviceIdentity {
    guid: String,
    dev: String,
    mac: String,
}

/// Keep the lite device identity stable across helper processes and restarts.
/// The file shape is compatible with the former sidecar implementation.
fn load_or_create_kugou_device(directory: &Path) -> io::Result<KugouDeviceIdentity> {
    fs::create_dir_all(directory)?;
    let path = directory.join(KUGOU_DEVICE_FILE);
    if path.is_file() {
        return read_kugou_device(&path);
    }

    let guid = Uuid::new_v4().to_string();
    let device = KugouDeviceIdentity {
        guid: miliastra_playback::kugou_normalize_guid(&guid),
        dev: Uuid::new_v4().to_string().to_ascii_uppercase(),
        mac: "02:00:00:00:00:00".to_owned(),
    };
    let content = serde_json::to_vec_pretty(&device)
        .map_err(|error| io::Error::other(format!("serialize KuGou device identity: {error}")))?;
    // `create_new` makes the first writer the owner.  A second helper process
    // must never overwrite an identity that is already in use by playback.
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&content)?;
            file.sync_all()?;
            Ok(device)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // The winning process may still be writing.  Give it a short,
            // bounded window to finish before parsing the shared file.
            for _ in 0..10 {
                match read_kugou_device(&path) {
                    Ok(device) => return Ok(device),
                    Err(read_error)
                        if matches!(
                            read_error.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::InvalidData
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(read_error) => return Err(read_error),
                }
            }
            read_kugou_device(&path)
        }
        Err(error) => Err(error),
    }
}

fn read_kugou_device(path: &Path) -> io::Result<KugouDeviceIdentity> {
    let text = fs::read_to_string(path)?;
    let device = serde_json::from_str::<KugouDeviceIdentity>(&text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid KuGou device identity: {error}"),
        )
    })?;
    if device.guid.trim().is_empty() || device.dev.trim().is_empty() || device.mac.trim().is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KuGou device identity contains an empty field",
        ));
    }
    Ok(KugouDeviceIdentity {
        guid: miliastra_playback::kugou_normalize_guid(&device.guid),
        dev: device.dev.trim().to_ascii_uppercase(),
        mac: device.mac.trim().to_ascii_uppercase(),
    })
}

impl HelperLauncher for CommandHelperLauncher {
    fn spawn(
        &self,
        provider: ProviderId,
        profile: &Path,
        timeout: Duration,
    ) -> io::Result<SpawnedHelper> {
        let mut command = Command::new(&self.executable);
        command
            .arg(provider.as_str())
            .arg("--profile")
            .arg(profile)
            .arg("--timeout-seconds")
            .arg(timeout.as_secs().max(1).to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // 持续读取 stderr，避免 sidecar 因管道写满而阻塞。其内容可能含有
            // 二维码 URL 或认证参数，不能转发到主程序日志。
            .stderr(Stdio::piped());
        if provider == ProviderId::Kugou {
            let device = load_or_create_kugou_device(&self.credential_directory)?;
            let mid = miliastra_playback::kugou_calculate_mid(&device.guid);
            command
                .env("KUGOU_API_GUID", device.guid)
                .env("KUGOU_API_DEV", device.dev)
                .env("KUGOU_API_MAC", device.mac.clone())
                .env("KUGOU_API_MID", mid);
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "login helper stdout is unavailable",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "login helper stderr is unavailable",
            )
        })?;
        Ok(SpawnedHelper {
            child: Box::new(CommandChild { child }),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        })
    }
}

struct CommandChild {
    child: Child,
}

impl ManagedChild for CommandChild {
    fn try_wait(&mut self) -> io::Result<Option<bool>> {
        self.child
            .try_wait()
            .map(|status| status.map(|status| status.success()))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    fn wait(&mut self) -> io::Result<bool> {
        self.child.wait().map(|status| status.success())
    }
}

type SharedChild = Arc<Mutex<Option<Box<dyn ManagedChild>>>>;

#[derive(Clone)]
pub(crate) struct LoginHelperManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    playback: Arc<dyn LoginPlaybackPort>,
    launcher: Arc<dyn HelperLauncher>,
    kugou_device_registrar: Arc<dyn KugouDeviceRegistrar>,
    credential_directory: PathBuf,
    profile_root: PathBuf,
    timeout: Duration,
    state: Mutex<ManagerState>,
    changed: Condvar,
}

struct ManagerState {
    active: Option<ActiveLogin>,
    worker: Option<JoinHandle<()>>,
    phase: LoginPhase,
    started_at_ms: Option<u64>,
    deadline_at_ms: Option<u64>,
    last_error: Option<LoginErrorView>,
}

struct ActiveLogin {
    session: LoginSession,
    cancel: Arc<AtomicBool>,
    child: SharedChild,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LoginPhase {
    Idle,
    Starting,
    WaitingForUser,
    ValidatingCredential,
    Canceling,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderView {
    pub provider: ProviderId,
    pub display_name: &'static str,
    pub configured: bool,
    pub fields: BTreeMap<String, bool>,
    pub required_fields: Vec<String>,
    pub present_field_count: usize,
    pub total_field_count: usize,
    pub refresh_supported: bool,
    pub manual_refresh_supported: bool,
    pub refresh_ready: bool,
    pub refresh_state: &'static str,
    pub last_refresh_at_ms: Option<u64>,
    pub next_refresh_check_at_ms: Option<u64>,
    pub last_refresh_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoginErrorView {
    pub code: String,
    pub message: String,
    pub provider: Option<ProviderId>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoginManagerStatus {
    pub active: bool,
    pub phase: LoginPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LoginErrorView>,
}

#[derive(Clone, Debug)]
pub(crate) struct LoginHelperFailure {
    pub code: String,
    pub message: String,
    pub provider: Option<ProviderId>,
}

impl std::fmt::Display for LoginHelperFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LoginHelperFailure {}

impl LoginHelperFailure {
    fn view(&self) -> LoginErrorView {
        LoginErrorView {
            code: self.code.to_owned(),
            message: self.message.to_owned(),
            provider: self.provider,
        }
    }
}

impl LoginHelperManager {
    pub(crate) fn new(
        playback: PlaybackHandle,
        executable: PathBuf,
        credential_directory: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self::new_with_dependencies(
            Arc::new(playback),
            Arc::new(CommandHelperLauncher {
                executable,
                credential_directory: credential_directory.clone(),
            }),
            Arc::new(DirectKugouDeviceRegistrar),
            credential_directory,
            timeout,
        )
    }

    fn new_with_dependencies(
        playback: Arc<dyn LoginPlaybackPort>,
        launcher: Arc<dyn HelperLauncher>,
        kugou_device_registrar: Arc<dyn KugouDeviceRegistrar>,
        credential_directory: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                playback,
                launcher,
                kugou_device_registrar,
                credential_directory: credential_directory.clone(),
                profile_root: credential_directory.join(PROFILE_ROOT_NAME),
                timeout: timeout.max(Duration::from_millis(1)),
                state: Mutex::new(ManagerState {
                    active: None,
                    worker: None,
                    phase: LoginPhase::Idle,
                    started_at_ms: None,
                    deadline_at_ms: None,
                    last_error: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn providers(&self) -> Result<Vec<ProviderView>, LoginHelperFailure> {
        let statuses = self
            .inner
            .playback
            .credential_statuses()
            .map_err(|error| playback_failure(error, None))?;
        Ok(ProviderId::ALL
            .into_iter()
            .map(|provider| {
                let status = statuses
                    .iter()
                    .find(|status| status.provider == provider.as_str())
                    .cloned()
                    .unwrap_or_else(|| CredentialStatus::empty(provider.as_str()));
                let fields: BTreeMap<String, bool> = status
                    .fields
                    .into_iter()
                    .map(|(name, present)| (name.to_owned(), present))
                    .collect();
                ProviderView {
                    provider,
                    display_name: provider_display_name(provider),
                    configured: status.configured,
                    required_fields: fields.keys().cloned().collect(),
                    present_field_count: fields.values().filter(|present| **present).count(),
                    total_field_count: fields.len(),
                    refresh_supported: status.refresh_supported,
                    manual_refresh_supported: status.manual_refresh_supported,
                    refresh_ready: status.refresh_ready,
                    refresh_state: status.refresh_state,
                    last_refresh_at_ms: status.last_refresh_at_ms,
                    next_refresh_check_at_ms: status.next_refresh_check_at_ms,
                    last_refresh_error: status.last_refresh_error,
                    fields,
                }
            })
            .collect())
    }

    pub(crate) fn status(&self) -> LoginManagerStatus {
        let Ok(state) = self.inner.state.lock() else {
            return LoginManagerStatus {
                active: false,
                phase: LoginPhase::Failed,
                session_id: None,
                provider: None,
                started_at_ms: None,
                deadline_at_ms: None,
                last_error: Some(
                    manager_failure("login_manager_unavailable", "登录管理器不可用", None).view(),
                ),
            };
        };
        let (session_id, provider) = state
            .active
            .as_ref()
            .map(|active| {
                (
                    Some(active.session.session_id),
                    Some(active.session.provider),
                )
            })
            .unwrap_or((None, None));
        LoginManagerStatus {
            active: session_id.is_some(),
            phase: state.phase,
            session_id,
            provider,
            started_at_ms: state.started_at_ms,
            deadline_at_ms: state.deadline_at_ms,
            last_error: state.last_error.clone(),
        }
    }

    pub(crate) fn start(&self, provider: ProviderId) -> Result<LoginSession, LoginHelperFailure> {
        self.join_finished_worker()?;
        let started_at_ms = unix_time_ms();
        let deadline_at_ms = started_at_ms.saturating_add(duration_ms(self.inner.timeout));
        {
            let mut state = self.inner.state.lock().map_err(|_| {
                manager_failure(
                    "login_manager_unavailable",
                    "登录管理器不可用",
                    Some(provider),
                )
            })?;
            if state.active.is_some()
                || matches!(
                    state.phase,
                    LoginPhase::Starting
                        | LoginPhase::WaitingForUser
                        | LoginPhase::ValidatingCredential
                        | LoginPhase::Canceling
                )
            {
                return Err(manager_failure(
                    "login_in_progress",
                    "已有登录任务正在进行",
                    Some(provider),
                ));
            }
            state.phase = LoginPhase::Starting;
            state.started_at_ms = Some(started_at_ms);
            state.deadline_at_ms = Some(deadline_at_ms);
            state.last_error = None;
        }

        let session = match self.inner.playback.begin_login(provider) {
            Ok(session) => session,
            Err(error) => {
                let failure = playback_failure(error, Some(provider));
                self.record_start_failure(&failure);
                return Err(failure);
            }
        };
        let profile = match self.create_profile(session.session_id, provider) {
            Ok(profile) => profile,
            Err(error) => {
                let _ = self.inner.playback.cancel_login(session.session_id);
                self.record_start_failure(&error);
                return Err(error);
            }
        };
        let spawned = match self
            .inner
            .launcher
            .spawn(provider, &profile, self.inner.timeout)
        {
            Ok(spawned) => spawned,
            Err(_) => {
                let _ = self.inner.playback.cancel_login(session.session_id);
                cleanup_profile(&profile);
                let failure =
                    manager_failure("helper_start_failed", "登录助手启动失败", Some(provider));
                self.record_start_failure(&failure);
                return Err(failure);
            }
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(spawned.child)));
        let worker_child = Arc::clone(&child);
        let worker_cancel = Arc::clone(&cancel);
        let worker_inner = Arc::clone(&self.inner);
        let worker_session = session.clone();
        let worker_profile = profile.clone();
        let mut state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(_) => {
                // The child has already been spawned at this point.  Do not
                // leak it (or its temporary profile) when the manager mutex
                // is poisoned between spawn and worker registration.
                terminate_child(&child);
                let _ = self.inner.playback.cancel_login(session.session_id);
                cleanup_profile(&profile);
                return Err(manager_failure(
                    "login_manager_unavailable",
                    "登录管理器不可用",
                    Some(provider),
                ));
            }
        };
        state.active = Some(ActiveLogin {
            session: session.clone(),
            cancel: Arc::clone(&cancel),
            child: Arc::clone(&child),
        });
        state.phase = LoginPhase::WaitingForUser;
        let worker = thread::Builder::new()
            .name("login-helper-manager".to_owned())
            .spawn(move || {
                run_helper(
                    worker_inner,
                    worker_session,
                    worker_cancel,
                    worker_child,
                    spawned.stdout,
                    spawned.stderr,
                    worker_profile,
                );
            });
        match worker {
            Ok(worker) => {
                state.worker = Some(worker);
                Ok(session)
            }
            Err(_) => {
                let failure = manager_failure(
                    "helper_start_failed",
                    "登录助手线程启动失败",
                    Some(provider),
                );
                state.active = None;
                state.phase = LoginPhase::Failed;
                state.last_error = Some(failure.view());
                self.inner.changed.notify_all();
                drop(state);
                terminate_child(&child);
                let _ = self.inner.playback.cancel_login(session.session_id);
                cleanup_profile(&profile);
                Err(failure)
            }
        }
    }

    pub(crate) fn cancel(&self, session_id: Uuid) -> Result<(), LoginHelperFailure> {
        let (cancel, child, provider) = {
            let mut state = self.inner.state.lock().map_err(|_| {
                manager_failure("login_manager_unavailable", "登录管理器不可用", None)
            })?;
            let Some(active) = state.active.as_ref() else {
                return Err(manager_failure(
                    "login_not_active",
                    "没有正在进行的登录任务",
                    None,
                ));
            };
            if active.session.session_id != session_id {
                return Err(manager_failure(
                    "login_session_invalid",
                    "登录会话无效",
                    None,
                ));
            }
            let cancel = Arc::clone(&active.cancel);
            let child = Arc::clone(&active.child);
            let provider = active.session.provider;
            state.phase = LoginPhase::Canceling;
            (cancel, child, provider)
        };
        cancel.store(true, Ordering::Release);
        terminate_child(&child);
        let _ = self.inner.playback.cancel_login(session_id);
        self.finish_and_join(session_id, provider)
    }

    pub(crate) fn refresh_credential(
        &self,
        provider: ProviderId,
    ) -> Result<CredentialStatus, LoginHelperFailure> {
        if self.status().active {
            return Err(manager_failure(
                "login_in_progress",
                "登录进行中，不能刷新凭据",
                Some(provider),
            ));
        }
        self.inner
            .playback
            .refresh_credential(provider)
            .map_err(|error| playback_failure(error, Some(provider)))
    }

    pub(crate) fn kugou_status(
        &self,
    ) -> Result<miliastra_playback::KugouAccountStatus, LoginHelperFailure> {
        self.inner
            .playback
            .kugou_status()
            .map_err(|error| playback_failure(error, Some(ProviderId::Kugou)))
    }

    pub(crate) fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, LoginHelperFailure> {
        self.inner
            .playback
            .account_status(provider)
            .map_err(|error| playback_failure(error, Some(provider)))
    }

    pub(crate) fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, LoginHelperFailure> {
        self.inner
            .playback
            .refresh_account_status(provider)
            .map_err(|error| playback_failure(error, Some(provider)))
    }

    pub(crate) fn kugou_claim_vip(
        &self,
    ) -> Result<miliastra_playback::KugouListenReport, LoginHelperFailure> {
        self.inner
            .playback
            .kugou_claim_vip()
            .map_err(|error| playback_failure(error, Some(ProviderId::Kugou)))
    }

    pub(crate) fn kugou_upgrade_vip(
        &self,
    ) -> Result<miliastra_playback::KugouListenReport, LoginHelperFailure> {
        self.inner
            .playback
            .kugou_upgrade_vip()
            .map_err(|error| playback_failure(error, Some(ProviderId::Kugou)))
    }

    pub(crate) fn logout(
        &self,
        provider: ProviderId,
    ) -> Result<CredentialStatus, LoginHelperFailure> {
        let active = self.status();
        if active.active {
            if active.provider == Some(provider) {
                if let Some(session_id) = active.session_id {
                    self.cancel(session_id)?;
                }
            } else {
                return Err(manager_failure(
                    "login_in_progress",
                    "另一个平台的登录任务正在进行",
                    active.provider,
                ));
            }
        }
        self.inner
            .playback
            .logout(provider)
            .map_err(|error| playback_failure(error, Some(provider)))
    }

    pub(crate) fn shutdown(&self) -> Result<(), LoginHelperFailure> {
        let session_id = self.status().session_id;
        let cancel_error = session_id.and_then(|session_id| self.cancel(session_id).err());
        let join_error = self.join_finished_worker().err();
        cancel_error.or(join_error).map_or(Ok(()), Err)
    }

    fn record_start_failure(&self, failure: &LoginHelperFailure) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.phase = LoginPhase::Failed;
            state.last_error = Some(failure.view());
        }
    }

    fn create_profile(
        &self,
        session_id: Uuid,
        provider: ProviderId,
    ) -> Result<PathBuf, LoginHelperFailure> {
        fs::create_dir_all(&self.inner.profile_root).map_err(|_| {
            manager_failure(
                "profile_create_failed",
                "登录 profile 创建失败",
                Some(provider),
            )
        })?;
        let profile = self
            .inner
            .profile_root
            .join(format!("{}-{}", provider.as_str(), session_id));
        fs::create_dir(&profile).map_err(|_| {
            manager_failure(
                "profile_create_failed",
                "登录 profile 创建失败",
                Some(provider),
            )
        })?;
        Ok(profile)
    }

    fn join_finished_worker(&self) -> Result<(), LoginHelperFailure> {
        let worker = {
            let mut state = self.inner.state.lock().map_err(|_| {
                manager_failure("login_manager_unavailable", "登录管理器不可用", None)
            })?;
            if state.active.is_some() {
                return Ok(());
            }
            state.worker.take()
        };
        if let Some(worker) = worker {
            worker.join().map_err(|_| {
                manager_failure("login_worker_failed", "登录任务线程异常退出", None)
            })?;
        }
        Ok(())
    }

    fn finish_and_join(
        &self,
        session_id: Uuid,
        provider: ProviderId,
    ) -> Result<(), LoginHelperFailure> {
        let deadline = Instant::now() + TERMINATION_GRACE + Duration::from_secs(1);
        let mut state = self.inner.state.lock().map_err(|_| {
            manager_failure(
                "login_manager_unavailable",
                "登录管理器不可用",
                Some(provider),
            )
        })?;
        while state
            .active
            .as_ref()
            .is_some_and(|active| active.session.session_id == session_id)
            && Instant::now() < deadline
        {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(REAPER_POLL);
            let (next, _) = self.inner.changed.wait_timeout(state, wait).map_err(|_| {
                manager_failure(
                    "login_manager_unavailable",
                    "登录管理器不可用",
                    Some(provider),
                )
            })?;
            state = next;
        }
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.session.session_id == session_id)
        {
            // A WebView2 descendant can keep stdout/stderr inherited after the
            // helper has been killed. Do not leave the manager permanently
            // active waiting for that pipe to close: the session is cancelled,
            // so detach the bounded worker and let its final cleanup observe
            // the session-id fence below. A later login can start immediately.
            let worker = state.worker.take();
            state.active = None;
            state.phase = LoginPhase::Failed;
            let failure = manager_failure("login_cancel_timeout", "取消登录超时", Some(provider));
            state.last_error = Some(failure.view());
            self.inner.changed.notify_all();
            drop(state);
            drop(worker);
            return Err(failure);
        }
        let worker = state.worker.take();
        drop(state);
        if let Some(worker) = worker {
            worker.join().map_err(|_| {
                manager_failure(
                    "login_worker_failed",
                    "登录任务线程异常退出",
                    Some(provider),
                )
            })?;
        }
        Ok(())
    }
}

fn run_helper(
    inner: Arc<ManagerInner>,
    session: LoginSession,
    cancel: Arc<AtomicBool>,
    child: SharedChild,
    mut stdout: Box<dyn Read + Send>,
    stderr: Box<dyn Read + Send>,
    profile: PathBuf,
) {
    let deadline = Instant::now() + inner.timeout;
    let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("login-helper-output".to_owned())
        .spawn(move || {
            let result = read_limited(&mut *stdout);
            let _ = output_tx.send(result);
        });
    // 登录助手可能在 stderr 写入二维码 URL 或认证参数。持续排空该管道，
    // 但绝不记录原文，避免敏感信息进入主程序日志。
    {
        let mut stderr = stderr;
        thread::Builder::new()
            .name("login-helper-diagnostic".to_owned())
            .spawn(move || {
                drain_helper_stderr(&mut *stderr);
            })
            .ok();
    }

    let mut failure = if reader.is_err() {
        Some(manager_failure(
            "helper_output_unavailable",
            "登录助手输出不可用",
            Some(session.provider),
        ))
    } else {
        None
    };
    let mut message = None;
    if failure.is_none() {
        loop {
            if cancel.load(Ordering::Acquire) {
                terminate_child(&child);
                failure = Some(manager_failure(
                    "login_cancelled",
                    "登录任务已取消",
                    Some(session.provider),
                ));
                break;
            }
            if Instant::now() >= deadline {
                terminate_child(&child);
                failure = Some(manager_failure(
                    "login_timeout",
                    "登录等待超时",
                    Some(session.provider),
                ));
                break;
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(REAPER_POLL);
            match output_rx.recv_timeout(wait) {
                Ok(Ok(bytes)) => {
                    match decode_message(&bytes) {
                        Ok(decoded) => message = Some(decoded),
                        Err(error) => {
                            failure = Some(protocol_failure(error, session.provider));
                        }
                    }
                    break;
                }
                Ok(Err(code)) => {
                    failure = Some(manager_failure(
                        code,
                        helper_error_text(code),
                        Some(session.provider),
                    ));
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    failure = Some(manager_failure(
                        "helper_output_unavailable",
                        "登录助手输出不可用",
                        Some(session.provider),
                    ));
                    break;
                }
            }
        }
    }

    let process_deadline = deadline.min(Instant::now() + TERMINATION_GRACE);
    let process_success = wait_child_until(&child, process_deadline).unwrap_or(false);
    if failure.is_none() && !process_success {
        failure = Some(if Instant::now() >= deadline {
            manager_failure("login_timeout", "登录等待超时", Some(session.provider))
        } else {
            manager_failure(
                "helper_process_failed",
                "登录助手异常退出",
                Some(session.provider),
            )
        });
    }
    if let Ok(reader) = reader {
        // 有界等待 reader 退出:WebView2 等孙进程可能继承 stdout 管道写端,
        // 导致 read() 永不返回 EOF、join 永久阻塞。超时则分离 join 线程。
        let (reader_done_tx, reader_done_rx) = std::sync::mpsc::sync_channel(1);
        let reaper = thread::Builder::new()
            .name("login-helper-output-reaper".to_owned())
            .spawn(move || {
                let _ = reader_done_tx.send(reader.join());
            });
        match reader_done_rx.recv_timeout(READER_JOIN_GRACE) {
            Ok(_) => {}
            Err(_) => {
                log::error!("登录助手输出线程 join 超时,已分离(孙进程可能持有 stdout 管道写端)");
            }
        }
        let _ = reaper;
    }

    let result = if let Some(failure) = failure {
        Err(failure)
    } else if let Some(message) = message {
        process_message(&inner, &session, message, &cancel, deadline)
    } else {
        Err(manager_failure(
            "helper_output_unavailable",
            "登录助手输出不可用",
            Some(session.provider),
        ))
    };
    if result.is_err() {
        let _ = inner.playback.cancel_login(session.session_id);
    }
    cleanup_profile(&profile);

    let mut state = match inner.state.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.session.session_id == session.session_id)
    {
        state.active = None;
        match result {
            Ok(()) => {
                state.phase = LoginPhase::Idle;
                state.started_at_ms = None;
                state.deadline_at_ms = None;
                state.last_error = None;
            }
            Err(error) => {
                state.phase = LoginPhase::Failed;
                state.last_error = Some(error.view());
            }
        }
        inner.changed.notify_all();
    }
}

fn process_message(
    inner: &ManagerInner,
    session: &LoginSession,
    message: LoginHelperMessage,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<(), LoginHelperFailure> {
    match message {
        LoginHelperMessage::Success {
            provider,
            credential,
            ..
        } => {
            let actual = parse_helper_provider(&provider, session.provider)?;
            if actual != session.provider || credential.provider() != session.provider.as_str() {
                return Err(manager_failure(
                    "invalid_helper_provider",
                    "登录助手返回的平台与请求不匹配",
                    Some(session.provider),
                ));
            }
            set_active_phase(inner, session.session_id, LoginPhase::ValidatingCredential);
            let credential =
                credential_from_payload_with_device_registration(inner, credential, cancel)?;
            complete_login_until(inner, session, credential, cancel, deadline)
        }
        LoginHelperMessage::Error { provider, code, .. } => {
            let actual = parse_helper_provider(&provider, session.provider)?;
            if actual != session.provider {
                return Err(manager_failure(
                    "invalid_helper_provider",
                    "登录助手返回的平台与请求不匹配",
                    Some(session.provider),
                ));
            }
            let code = stable_helper_code(&code);
            Err(manager_failure(
                code,
                helper_error_text(code),
                Some(session.provider),
            ))
        }
    }
}

fn complete_login_until(
    inner: &ManagerInner,
    session: &LoginSession,
    credential: ProviderCredential,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<(), LoginHelperFailure> {
    if cancel.load(Ordering::Acquire) {
        return Err(manager_failure(
            "login_cancelled",
            "登录任务已取消",
            Some(session.provider),
        ));
    }
    if Instant::now() >= deadline {
        return Err(manager_failure(
            "login_timeout",
            "登录等待超时",
            Some(session.provider),
        ));
    }

    let completion = inner
        .playback
        .complete_login(session.session_id, credential)
        .map_err(|error| playback_failure(error, Some(session.provider)))?;

    loop {
        if let PendingLoginPoll::Completed(result) = completion.wait_timeout(Duration::ZERO) {
            return map_login_completion(result, session.provider);
        }

        let cancelled = cancel.load(Ordering::Acquire);
        let timed_out = Instant::now() >= deadline;
        if cancelled || timed_out {
            let terminal_failure = if cancelled {
                manager_failure("login_cancelled", "登录任务已取消", Some(session.provider))
            } else {
                manager_failure("login_timeout", "登录等待超时", Some(session.provider))
            };
            let _ = inner.playback.cancel_login(session.session_id);
            return match completion.wait_timeout(TERMINATION_GRACE) {
                PendingLoginPoll::Completed(Ok(_)) => Ok(()),
                PendingLoginPoll::Completed(Err(error)) => {
                    if error.code() == "playback_cancelled" {
                        Err(terminal_failure)
                    } else {
                        Err(playback_failure(error, Some(session.provider)))
                    }
                }
                PendingLoginPoll::TimedOut => Err(terminal_failure),
            };
        }

        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(REAPER_POLL);
        match completion.wait_timeout(wait) {
            PendingLoginPoll::Completed(result) => {
                return map_login_completion(result, session.provider);
            }
            PendingLoginPoll::TimedOut => {}
        }
    }
}

fn set_active_phase(inner: &ManagerInner, session_id: Uuid, phase: LoginPhase) {
    if let Ok(mut state) = inner.state.lock()
        && state
            .active
            .as_ref()
            .is_some_and(|active| active.session.session_id == session_id)
    {
        state.phase = phase;
        inner.changed.notify_all();
    }
}

fn map_login_completion(
    result: Result<CredentialStatus, PlaybackError>,
    provider: ProviderId,
) -> Result<(), LoginHelperFailure> {
    result
        .map(|_| ())
        .map_err(|error| playback_failure(error, Some(provider)))
}

fn parse_helper_provider(
    provider: &str,
    requested: ProviderId,
) -> Result<ProviderId, LoginHelperFailure> {
    provider.parse::<ProviderId>().map_err(|_| {
        manager_failure(
            "invalid_helper_provider",
            "登录助手返回的平台无效",
            Some(requested),
        )
    })
}

fn credential_from_payload_with_device_registration(
    inner: &ManagerInner,
    payload: CredentialPayload,
    cancel: &AtomicBool,
) -> Result<ProviderCredential, LoginHelperFailure> {
    match payload {
        CredentialPayload::Kugou {
            token,
            userid,
            mut cookies,
        } => {
            if cancel.load(Ordering::Acquire) {
                return Err(manager_failure(
                    "login_cancelled",
                    "登录任务已取消",
                    Some(ProviderId::Kugou),
                ));
            }
            let device =
                load_or_create_kugou_device(&inner.credential_directory).map_err(|_error| {
                    manager_failure(
                        "kugou_device_identity_failed",
                        "酷狗设备标识初始化失败",
                        Some(ProviderId::Kugou),
                    )
                })?;
            // The QR request and device registration must use exactly the
            // identity that will be persisted with the new credential.
            cookies.insert("KUGOU_API_GUID".to_owned(), device.guid.clone());
            cookies.insert("KUGOU_API_DEV".to_owned(), device.dev.clone());
            cookies.insert("KUGOU_API_MAC".to_owned(), device.mac.clone());
            cookies.insert(
                "KUGOU_API_MID".to_owned(),
                miliastra_playback::kugou_calculate_mid(&device.guid),
            );
            if cancel.load(Ordering::Acquire) {
                return Err(manager_failure(
                    "login_cancelled",
                    "登录任务已取消",
                    Some(ProviderId::Kugou),
                ));
            }
            let dfid = inner
                .kugou_device_registrar
                .register(&token, &userid, &cookies)?;
            if cancel.load(Ordering::Acquire) {
                return Err(manager_failure(
                    "login_cancelled",
                    "登录任务已取消",
                    Some(ProviderId::Kugou),
                ));
            }
            // dfid 已作为独立凭据字段保存；移除 cookie 副本以通过
            // 凭据 cookie 白名单校验。
            cookies.remove("dfid");
            Ok(ProviderCredential::Kugou {
                token,
                userid,
                dfid,
                cookies,
            })
        }
        payload => Ok(credential_from_payload(payload)),
    }
}

fn credential_from_payload(payload: CredentialPayload) -> ProviderCredential {
    match payload {
        CredentialPayload::QqMusic { cookies } => ProviderCredential::QqMusic { cookies },
        CredentialPayload::Netease { cookies } => ProviderCredential::Netease { cookies },
        CredentialPayload::Bilibili {
            cookies,
            refresh_token,
        } => ProviderCredential::Bilibili {
            cookies,
            refresh_token: (!refresh_token.trim().is_empty()).then_some(refresh_token),
        },
        CredentialPayload::Kugou { .. } => {
            unreachable!("酷狗凭据必须先完成设备注册")
        }
    }
}

fn protocol_failure(error: ProtocolError, provider: ProviderId) -> LoginHelperFailure {
    let (code, message) = match error {
        ProtocolError::TooLarge(_) => ("helper_output_too_large", "登录助手输出超过大小限制"),
        ProtocolError::NotSingleMessage
        | ProtocolError::UnsupportedVersion(_)
        | ProtocolError::Json(_) => ("invalid_helper_message", "登录助手返回无效结果"),
    };
    manager_failure(code, message, Some(provider))
}

/// 播放层错误映射：透传真实错误码与原始信息（失败原因一目了然），
/// 仅在原始信息为空时回落到中文说明。
fn playback_failure(error: PlaybackError, provider: Option<ProviderId>) -> LoginHelperFailure {
    match error {
        PlaybackError::Failure(failure) => {
            let message = if failure.message.trim().is_empty() {
                playback_code_message(&failure.code).to_owned()
            } else {
                failure.message
            };
            LoginHelperFailure {
                code: failure.code,
                message,
                provider,
            }
        }
        other => {
            let code = other.code();
            LoginHelperFailure {
                code: code.to_owned(),
                message: playback_code_message(code).to_owned(),
                provider,
            }
        }
    }
}

fn playback_code_message(code: &str) -> &'static str {
    match code {
        "playback_runtime_stopped" => "播放运行时未启动",
        "playback_busy" => "播放运行时繁忙，请稍后重试",
        "no_active_session" => "没有活动的播放会话",
        "track_unavailable" => "曲目不可用",
        "provider_auth_required" => "该平台尚未登录",
        "relogin_required" => "登录凭据已失效，请重新登录",
        "provider_rate_limited" => "上游接口触发限流，请稍后重试",
        "provider_timeout" => "上游接口超时，请稍后重试",
        "provider_transient" => "上游服务暂时不可用，请稍后重试",
        "login_in_progress" => "已有登录任务正在进行",
        "login_session_invalid" => "登录会话无效",
        "unknown_provider" => "未知平台",
        "invalid_request" => "请求参数无效",
        "playback_cancelled" => "操作已取消",
        "playback_failed" => "播放操作失败",
        _ => "播放操作失败",
    }
}

fn provider_display_name(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::QqMusic => "QQ音乐",
        ProviderId::Netease => "网易云音乐",
        ProviderId::Bilibili => "哔哩哔哩",
        ProviderId::Kugou => "酷狗音乐",
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn manager_failure(
    code: &'static str,
    message: &'static str,
    provider: Option<ProviderId>,
) -> LoginHelperFailure {
    LoginHelperFailure {
        code: code.to_owned(),
        message: message.to_owned(),
        provider,
    }
}

fn stable_helper_code(code: &str) -> &'static str {
    match code {
        "unsupported_provider" => "unsupported_provider",
        "webview_runtime_unavailable" => "webview_runtime_unavailable",
        "login_timeout" => "login_timeout",
        "login_cancelled" => "login_cancelled",
        _ => "credential_capture_failed",
    }
}

fn helper_error_text(code: &str) -> &'static str {
    match code {
        "unsupported_provider" => "不支持该登录平台",
        "webview_runtime_unavailable" => "WebView2 Runtime 不可用",
        "login_timeout" => "登录等待超时",
        "login_cancelled" => "登录窗口已关闭",
        "helper_output_too_large" => "登录助手输出超过大小限制",
        "invalid_helper_message" => "登录助手返回无效结果",
        "invalid_helper_provider" => "登录助手返回的平台无效",
        "helper_process_failed" => "登录助手异常退出",
        "helper_output_unavailable" => "登录助手输出不可用",
        _ => "未能取得有效登录凭据",
    }
}

fn read_limited(reader: &mut dyn Read) -> Result<Vec<u8>, &'static str> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| "helper_output_unavailable")?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_LOGIN_MESSAGE_BYTES {
            return Err("helper_output_too_large");
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn drain_helper_stderr(reader: &mut dyn Read) {
    let mut buffer = [0_u8; 2048];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
    }
}

fn terminate_child(child: &SharedChild) {
    if let Ok(mut child) = child.lock()
        && let Some(child) = child.as_mut()
    {
        let _ = child.kill();
    }
}

fn wait_child_until(child: &SharedChild, deadline: Instant) -> io::Result<bool> {
    loop {
        let poll = child
            .lock()
            .map_err(|_| io::Error::other("login helper process state is unavailable"))?;
        let mut poll = poll;
        let result = match poll.as_mut() {
            Some(child) => child.try_wait()?,
            None => Some(true),
        };
        if let Some(success) = result {
            if poll.is_some() {
                let _ = poll.take();
            }
            return Ok(success);
        }
        drop(poll);
        if Instant::now() >= deadline {
            let mut process = child
                .lock()
                .map_err(|_| io::Error::other("login helper process state is unavailable"))?;
            if let Some(mut process) = process.take() {
                let _ = process.kill();
                return process.wait();
            }
            return Ok(true);
        }
        thread::sleep(REAPER_POLL);
    }
}

fn cleanup_profile(path: &Path) {
    for _ in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakePlayback {
        state: Arc<Mutex<FakePlaybackState>>,
        changed: Arc<Condvar>,
        completion_started: Arc<Mutex<Option<std::sync::mpsc::SyncSender<()>>>>,
        completion_behavior: FakeCompletionBehavior,
    }

    struct ChannelPendingLoginCompletion {
        result: std::sync::mpsc::Receiver<Result<CredentialStatus, PlaybackError>>,
        worker: Mutex<Option<JoinHandle<()>>>,
    }

    impl ChannelPendingLoginCompletion {
        fn join_worker(&self) {
            if let Some(worker) = self.worker.lock().unwrap().take() {
                let _ = worker.join();
            }
        }
    }

    impl PendingLoginCompletion for ChannelPendingLoginCompletion {
        fn wait_timeout(&self, timeout: Duration) -> PendingLoginPoll {
            match self.result.recv_timeout(timeout) {
                Ok(result) => {
                    self.join_worker();
                    PendingLoginPoll::Completed(result)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => PendingLoginPoll::TimedOut,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.join_worker();
                    PendingLoginPoll::Completed(Err(PlaybackError::RuntimeStopped))
                }
            }
        }
    }

    impl Drop for ChannelPendingLoginCompletion {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.get_mut().unwrap().take() {
                let _ = worker.join();
            }
        }
    }

    #[derive(Clone, Copy)]
    enum FakeCompletionBehavior {
        Immediate,
        BlockUntilCancelled,
        CommitThenDelay(Duration),
    }

    struct FakePlaybackState {
        active: Option<LoginSession>,
        completed: usize,
        cancelled: usize,
    }

    impl FakePlayback {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(Mutex::new(FakePlaybackState {
                    active: None,
                    completed: 0,
                    cancelled: 0,
                })),
                changed: Arc::new(Condvar::new()),
                completion_started: Arc::new(Mutex::new(None)),
                completion_behavior: FakeCompletionBehavior::Immediate,
            })
        }

        fn blocking_completion(started: std::sync::mpsc::SyncSender<()>) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(Mutex::new(FakePlaybackState {
                    active: None,
                    completed: 0,
                    cancelled: 0,
                })),
                changed: Arc::new(Condvar::new()),
                completion_started: Arc::new(Mutex::new(Some(started))),
                completion_behavior: FakeCompletionBehavior::BlockUntilCancelled,
            })
        }

        fn committed_then_delayed(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(Mutex::new(FakePlaybackState {
                    active: None,
                    completed: 0,
                    cancelled: 0,
                })),
                changed: Arc::new(Condvar::new()),
                completion_started: Arc::new(Mutex::new(None)),
                completion_behavior: FakeCompletionBehavior::CommitThenDelay(delay),
            })
        }
    }

    impl LoginPlaybackPort for FakePlayback {
        fn credential_statuses(&self) -> Result<Vec<CredentialStatus>, PlaybackError> {
            Ok(ProviderId::ALL
                .iter()
                .map(|provider| CredentialStatus::empty(provider.as_str()))
                .collect())
        }

        fn refresh_credential(
            &self,
            provider: ProviderId,
        ) -> Result<CredentialStatus, PlaybackError> {
            Ok(CredentialStatus::empty(provider.as_str()))
        }

        fn kugou_status(&self) -> Result<miliastra_playback::KugouAccountStatus, PlaybackError> {
            Ok(Default::default())
        }

        fn account_status(
            &self,
            _provider: ProviderId,
        ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError> {
            Ok(None)
        }

        fn refresh_account_status(
            &self,
            _provider: ProviderId,
        ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, PlaybackError> {
            Ok(None)
        }

        fn kugou_claim_vip(&self) -> Result<miliastra_playback::KugouListenReport, PlaybackError> {
            Ok(Default::default())
        }

        fn kugou_upgrade_vip(
            &self,
        ) -> Result<miliastra_playback::KugouListenReport, PlaybackError> {
            Ok(Default::default())
        }

        fn begin_login(&self, provider: ProviderId) -> Result<LoginSession, PlaybackError> {
            let mut state = self.state.lock().unwrap();
            if state.active.is_some() {
                return Err(PlaybackError::Failure(Failure::new(
                    "login_in_progress",
                    "busy",
                )));
            }
            let session = LoginSession {
                session_id: Uuid::new_v4(),
                provider,
            };
            state.active = Some(session.clone());
            Ok(session)
        }

        fn complete_login(
            &self,
            session_id: Uuid,
            credential: ProviderCredential,
        ) -> Result<Box<dyn PendingLoginCompletion>, PlaybackError> {
            if let ProviderCredential::Kugou { dfid, .. } = &credential {
                assert_eq!(dfid, "test-dfid");
            }
            let state = Arc::clone(&self.state);
            let changed = Arc::clone(&self.changed);
            let completion_started = Arc::clone(&self.completion_started);
            let behavior = self.completion_behavior;
            let (result_sender, result) = std::sync::mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                if let Some(started) = completion_started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let mut state = state.lock().unwrap();
                let result = if state.active.as_ref().map(|session| session.session_id)
                    != Some(session_id)
                {
                    Err(PlaybackError::Failure(Failure::new(
                        "login_session_invalid",
                        "invalid",
                    )))
                } else {
                    match behavior {
                        FakeCompletionBehavior::Immediate => {
                            state.active = None;
                            state.completed += 1;
                            Ok(CredentialStatus::empty("qqmusic"))
                        }
                        FakeCompletionBehavior::BlockUntilCancelled => {
                            while state.active.as_ref().map(|session| session.session_id)
                                == Some(session_id)
                            {
                                state = changed.wait(state).unwrap();
                            }
                            Err(PlaybackError::Failure(Failure::new(
                                "playback_cancelled",
                                "cancelled",
                            )))
                        }
                        FakeCompletionBehavior::CommitThenDelay(delay) => {
                            state.active = None;
                            state.completed += 1;
                            drop(state);
                            thread::sleep(delay);
                            Ok(CredentialStatus::empty("qqmusic"))
                        }
                    }
                };
                let _ = result_sender.send(result);
            });
            Ok(Box::new(ChannelPendingLoginCompletion {
                result,
                worker: Mutex::new(Some(worker)),
            }))
        }

        fn cancel_login(&self, session_id: Uuid) -> Result<(), PlaybackError> {
            let mut state = self.state.lock().unwrap();
            if state.active.as_ref().map(|session| session.session_id) == Some(session_id) {
                state.active = None;
                state.cancelled += 1;
                self.changed.notify_all();
            }
            Ok(())
        }

        fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError> {
            Ok(CredentialStatus::empty(provider.as_str()))
        }
    }

    #[derive(Clone)]
    struct FakeProcess {
        state: Arc<(Mutex<FakeProcessState>, Condvar)>,
    }

    struct FakeProcessState {
        exited: bool,
        success: bool,
    }

    impl FakeProcess {
        fn new(exited: bool, success: bool) -> (Self, Arc<(Mutex<FakeProcessState>, Condvar)>) {
            let state = Arc::new((
                Mutex::new(FakeProcessState { exited, success }),
                Condvar::new(),
            ));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl ManagedChild for FakeProcess {
        fn try_wait(&mut self) -> io::Result<Option<bool>> {
            let state = self.state.0.lock().unwrap();
            Ok(state.exited.then_some(state.success))
        }

        fn kill(&mut self) -> io::Result<()> {
            let mut state = self.state.0.lock().unwrap();
            state.exited = true;
            state.success = false;
            self.state.1.notify_all();
            Ok(())
        }

        fn wait(&mut self) -> io::Result<bool> {
            let mut state = self.state.0.lock().unwrap();
            while !state.exited {
                state = self.state.1.wait(state).unwrap();
            }
            Ok(state.success)
        }
    }

    struct BlockingReader {
        state: Arc<(Mutex<FakeProcessState>, Condvar)>,
    }

    impl Read for BlockingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.0.lock().unwrap();
            while !state.exited {
                state = self.state.1.wait(state).unwrap();
            }
            Ok(0)
        }
    }

    enum FakeScenario {
        Message(Vec<u8>),
        Oversized,
        Blocking,
    }

    struct FakeKugouDeviceRegistrar;

    impl KugouDeviceRegistrar for FakeKugouDeviceRegistrar {
        fn register(
            &self,
            _token: &str,
            _userid: &str,
            _cookies: &BTreeMap<String, String>,
        ) -> Result<String, LoginHelperFailure> {
            Ok("test-dfid".to_owned())
        }
    }

    struct FakeLauncher {
        scenarios: Mutex<VecDeque<FakeScenario>>,
    }

    impl FakeLauncher {
        fn new(scenarios: impl IntoIterator<Item = FakeScenario>) -> Arc<Self> {
            Arc::new(Self {
                scenarios: Mutex::new(scenarios.into_iter().collect()),
            })
        }
    }

    impl HelperLauncher for FakeLauncher {
        fn spawn(
            &self,
            _provider: ProviderId,
            _profile: &Path,
            _timeout: Duration,
        ) -> io::Result<SpawnedHelper> {
            let scenario = self.scenarios.lock().unwrap().pop_front().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no fake helper scenario")
            })?;
            let (child, state) =
                FakeProcess::new(!matches!(&scenario, FakeScenario::Blocking), true);
            let stdout: Box<dyn Read + Send> = match scenario {
                FakeScenario::Message(bytes) => Box::new(Cursor::new(bytes)),
                FakeScenario::Oversized => {
                    Box::new(Cursor::new(vec![b'x'; MAX_LOGIN_MESSAGE_BYTES + 1]))
                }
                FakeScenario::Blocking => Box::new(BlockingReader { state }),
            };
            Ok(SpawnedHelper {
                child: Box::new(child),
                stdout,
                stderr: Box::new(Cursor::new(Vec::new())),
            })
        }
    }

    fn profile_root(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "miliastra-login-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn legacy_kugou_device_is_normalized_without_rewriting_the_file() {
        let directory = profile_root("kugou-device-migration");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(KUGOU_DEVICE_FILE);
        let legacy_guid = "550e8400-e29b-41d4-a716-446655440000";
        fs::write(
            &path,
            serde_json::to_vec_pretty(&KugouDeviceIdentity {
                guid: legacy_guid.to_owned(),
                dev: "550e8400-e29b-41d4-a716-446655440001".to_owned(),
                mac: "aa:bb:cc:dd:ee:ff".to_owned(),
            })
            .unwrap(),
        )
        .unwrap();

        let loaded = load_or_create_kugou_device(&directory).unwrap();
        assert_eq!(
            loaded.guid,
            miliastra_playback::kugou_normalize_guid(legacy_guid)
        );
        assert_eq!(loaded.dev, "550E8400-E29B-41D4-A716-446655440001");
        assert_eq!(loaded.mac, "AA:BB:CC:DD:EE:FF");
        let persisted: KugouDeviceIdentity =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.guid, legacy_guid);

        fs::remove_dir_all(directory).unwrap();
    }

    fn manager(
        playback: Arc<FakePlayback>,
        launcher: Arc<FakeLauncher>,
        name: &str,
        timeout: Duration,
    ) -> LoginHelperManager {
        LoginHelperManager::new_with_dependencies(
            playback,
            launcher,
            Arc::new(FakeKugouDeviceRegistrar),
            profile_root(name),
            timeout,
        )
    }

    fn wait_idle(manager: &LoginHelperManager) -> LoginManagerStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = manager.status();
            if !status.active || Instant::now() >= deadline {
                return status;
            }
            thread::sleep(REAPER_POLL);
        }
    }

    fn valid_success(provider: &str) -> Vec<u8> {
        encode_message(&LoginHelperMessage::success(
            provider,
            CredentialPayload::QqMusic {
                cookies: BTreeMap::from([
                    ("uin".to_owned(), "123".to_owned()),
                    ("qqmusic_key".to_owned(), "key".to_owned()),
                ]),
            },
        ))
        .unwrap()
    }

    fn valid_kugou_success() -> Vec<u8> {
        encode_message(&LoginHelperMessage::success(
            "kugou",
            CredentialPayload::Kugou {
                token: "token".to_owned(),
                userid: "123".to_owned(),
                cookies: BTreeMap::new(),
            },
        ))
        .unwrap()
    }

    #[test]
    fn helper_stderr_is_drained_without_interpreting_its_contents() {
        let diagnostic = b"https://login.example.invalid/qr?token=secret-token\n";
        let mut stderr = Cursor::new(diagnostic.to_vec());

        drain_helper_stderr(&mut stderr);

        assert_eq!(stderr.position(), diagnostic.len() as u64);
    }

    #[test]
    fn kugou_helper_payload_registers_device_before_completion() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Message(valid_kugou_success())]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "kugou-device",
            Duration::from_secs(1),
        );
        manager.start(ProviderId::Kugou).unwrap();
        let status = wait_idle(&manager);
        assert!(!status.active);
        assert!(status.last_error.is_none());
        assert_eq!(playback.state.lock().unwrap().completed, 1);
        manager.shutdown().unwrap();
    }

    #[test]
    fn successful_helper_message_completes_the_matching_session() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Message(valid_success("qqmusic"))]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "success",
            Duration::from_secs(1),
        );
        manager.start(ProviderId::QqMusic).unwrap();
        let status = wait_idle(&manager);
        assert!(!status.active);
        assert_eq!(status.phase, LoginPhase::Idle);
        assert!(status.started_at_ms.is_none());
        assert!(status.deadline_at_ms.is_none());
        assert!(status.last_error.is_none());
        assert_eq!(playback.state.lock().unwrap().completed, 1);
        manager.shutdown().unwrap();
    }

    #[test]
    fn oversized_output_is_rejected_and_profile_is_removed() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Oversized]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "oversized",
            Duration::from_secs(1),
        );
        manager.start(ProviderId::QqMusic).unwrap();
        let status = wait_idle(&manager);
        assert_eq!(
            status.last_error.as_ref().unwrap().code,
            "helper_output_too_large"
        );
        assert_eq!(playback.state.lock().unwrap().cancelled, 1);
        manager.shutdown().unwrap();
    }

    #[test]
    fn helper_provider_mismatch_is_rejected_without_accepting_credentials() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Message(valid_success("netease"))]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "provider",
            Duration::from_secs(1),
        );
        manager.start(ProviderId::QqMusic).unwrap();
        let status = wait_idle(&manager);
        assert_eq!(
            status.last_error.as_ref().unwrap().code,
            "invalid_helper_provider"
        );
        assert_eq!(playback.state.lock().unwrap().completed, 0);
        manager.shutdown().unwrap();
    }

    #[test]
    fn cancellation_kills_the_helper_and_joins_the_worker() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Blocking]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "cancel",
            Duration::from_secs(10),
        );
        let session = manager.start(ProviderId::QqMusic).unwrap();
        let active = manager.status();
        assert_eq!(active.phase, LoginPhase::WaitingForUser);
        assert!(active.started_at_ms.is_some());
        assert!(active.deadline_at_ms > active.started_at_ms);
        manager.cancel(session.session_id).unwrap();
        let status = manager.status();
        assert!(!status.active);
        assert_eq!(status.phase, LoginPhase::Failed);
        assert_eq!(status.last_error.as_ref().unwrap().code, "login_cancelled");
        assert_eq!(playback.state.lock().unwrap().cancelled, 1);
        manager.shutdown().unwrap();
    }

    #[test]
    fn helper_timeout_is_bounded_and_releases_the_login_lease() {
        let playback = FakePlayback::new();
        let launcher = FakeLauncher::new([FakeScenario::Blocking]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "timeout",
            Duration::from_millis(40),
        );
        manager.start(ProviderId::QqMusic).unwrap();
        let started = Instant::now();
        let status = wait_idle(&manager);
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(status.last_error.as_ref().unwrap().code, "login_timeout");
        assert_eq!(playback.state.lock().unwrap().cancelled, 1);
        manager.shutdown().unwrap();
    }

    #[test]
    fn helper_timeout_includes_credential_validation_and_persistence() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let playback = FakePlayback::blocking_completion(started_tx);
        let launcher = FakeLauncher::new([FakeScenario::Message(valid_success("qqmusic"))]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "validation-timeout",
            Duration::from_millis(40),
        );
        manager.start(ProviderId::QqMusic).unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("credential validation started");
        assert_eq!(manager.status().phase, LoginPhase::ValidatingCredential);

        let started = Instant::now();
        let status = wait_idle(&manager);

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(status.last_error.as_ref().unwrap().code, "login_timeout");
        let state = playback.state.lock().unwrap();
        assert_eq!(state.completed, 0);
        assert_eq!(state.cancelled, 1);
        drop(state);
        manager.shutdown().unwrap();
    }

    #[test]
    fn committed_credential_result_wins_over_deadline_reporting() {
        let playback = FakePlayback::committed_then_delayed(Duration::from_millis(80));
        let launcher = FakeLauncher::new([FakeScenario::Message(valid_success("qqmusic"))]);
        let manager = manager(
            Arc::clone(&playback),
            launcher,
            "committed-before-deadline",
            Duration::from_millis(40),
        );

        manager.start(ProviderId::QqMusic).unwrap();
        let status = wait_idle(&manager);

        assert!(status.last_error.is_none());
        let state = playback.state.lock().unwrap();
        assert_eq!(state.completed, 1);
        assert_eq!(state.cancelled, 0);
        drop(state);
        manager.shutdown().unwrap();
    }
}
