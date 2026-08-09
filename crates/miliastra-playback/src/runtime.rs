use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, Id as TaskId, JoinError, JoinHandle as TokioJoinHandle, JoinSet};
use uuid::Uuid;

use crate::catalog::{
    BilibiliAdapter, CredentialRefreshAdapter, KugouAccountStatus, KugouAdapter, KugouListenReport,
    NeteaseAdapter, ProviderAccountStatus, ProviderId, ProviderRegistry, QqMusicAdapter,
    SourceAdapter, SourceCatalog,
};
use crate::core::{PlaybackCore, PlaybackCoreError};
use crate::credentials::{CredentialStatus, CredentialStore, ProviderCredential};
use crate::domain::{
    EndBehavior, EndCause, EngineState, Failure, PlaybackSnapshot as EngineSnapshot, SearchSpec,
    SessionRef,
};
use crate::engine::{FfmpegConfig, FfmpegEngine};
use crate::lyrics::TimedLyrics;
use crate::model::{PlayableTrack, SearchCandidate, SearchQuery};

const COMMAND_CAPACITY: usize = 32;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_CHECK_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
const REFRESH_FAILURE_BACKOFF_MS: u64 = 15 * 60 * 1000;

type Reply<T> = std_mpsc::SyncSender<Result<T, PlaybackError>>;

/// Owns the dedicated Tokio thread, provider adapters, and the sole FFmpeg engine.
pub struct PlaybackRuntime {
    handle: PlaybackHandle,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct PlaybackHandle {
    commands: mpsc::Sender<Command>,
}

pub struct PlaybackOperation {
    reply: std_mpsc::Receiver<Result<(), PlaybackError>>,
}

pub struct LoginOperation {
    reply: std_mpsc::Receiver<Result<CredentialStatus, PlaybackError>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSnapshot {
    pub runtime_identity: String,
    pub generation: u64,
    pub session_id: Option<Uuid>,
    pub state: EngineState,
    pub track: Option<PlayableTrack>,
    pub position_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lyric_line_text: Option<String>,
    pub volume: u8,
    pub last_end_cause: Option<EndCause>,
    pub end_behavior: Option<EndBehavior>,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginSession {
    pub session_id: Uuid,
    pub provider: ProviderId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("playback runtime is not running")]
    RuntimeStopped,
    #[error("playback runtime command queue is full")]
    Busy,
    #[error("there is no active playback session")]
    NoActiveSession,
    #[error("playback operation failed: {0:?}")]
    Failure(Failure),
    #[error("playback runtime initialization failed: {0}")]
    Startup(String),
    #[error("playback runtime internal error: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LoginOperationWaitError {
    #[error("login operation timed out")]
    TimedOut,
    #[error("playback runtime stopped before the login operation completed")]
    RuntimeStopped,
    #[error(transparent)]
    Playback(#[from] PlaybackError),
}

impl PlaybackError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RuntimeStopped => "playback_runtime_stopped",
            Self::Busy => "playback_busy",
            Self::NoActiveSession => "no_active_session",
            Self::Failure(failure) => match failure.code.as_str() {
                "track_unavailable" => "track_unavailable",
                "track_vip_required" => "track_vip_required",
                "track_no_copyright" => "track_no_copyright",
                "provider_auth_required" => "provider_auth_required",
                "relogin_required" => "relogin_required",
                "provider_rate_limited" => "provider_rate_limited",
                "provider_timeout" => "provider_timeout",
                "provider_transient" => "provider_transient",
                "login_in_progress" => "login_in_progress",
                "login_session_invalid" => "login_session_invalid",
                "unknown_provider" => "unknown_provider",
                "invalid_request" => "invalid_request",
                "playback_cancelled" => "playback_cancelled",
                _ => "playback_failed",
            },
            Self::Startup(_) => "playback_startup_failed",
            Self::Internal(_) => "playback_internal",
        }
    }
}

enum Command {
    Search(SearchQuery, Reply<Vec<SearchCandidate>>),
    Play(PlayableTrack, Reply<()>),
    /// 预加载音源解析结果（fire-and-forget，无回复；失败静默）。
    Preload(PlayableTrack),
    Pause(Reply<()>),
    Resume(Reply<()>),
    Stop(Reply<()>),
    SetVolume(u8, Reply<()>),
    Snapshot(Reply<PlaybackSnapshot>),
    Providers(Reply<Vec<ProviderId>>),
    CredentialStatuses(Reply<Vec<CredentialStatus>>),
    RefreshCredential(ProviderId, Reply<CredentialStatus>),
    KugouAccountStatus(Reply<KugouAccountStatus>),
    AccountStatus(ProviderId, Reply<Option<ProviderAccountStatus>>),
    KugouClaimVip(Reply<KugouListenReport>),
    KugouUpgradeVip(Reply<KugouListenReport>),
    SaveCredential(ProviderCredential, Reply<CredentialStatus>),
    Logout(ProviderId, Reply<CredentialStatus>),
    BeginLogin(ProviderId, Reply<LoginSession>),
    LoginStatus(Reply<LoginStatus>),
    CompleteLogin(Uuid, ProviderCredential, Reply<CredentialStatus>),
    CancelLogin(Uuid, Reply<()>),
    Shutdown(Reply<()>),
}

enum LyricState {
    Loading {
        generation: u64,
        task: TokioJoinHandle<Result<Option<TimedLyrics>, PlaybackCoreError>>,
    },
    Ready {
        generation: u64,
        lyrics: Option<TimedLyrics>,
    },
}

struct PendingPlay {
    reply: Reply<()>,
    cancellation: oneshot::Sender<()>,
    task: TokioJoinHandle<PlayCompletion>,
}

struct PlayCompletion {
    track: PlayableTrack,
    key: crate::domain::SongKey,
    resolver_locator: Option<crate::domain::ResolverLocator>,
    result: Result<crate::core::StartReceipt, PlaybackCoreError>,
}

struct PendingLoginValidation {
    session_id: Uuid,
    provider: ProviderId,
    credential: ProviderCredential,
    reply: Reply<CredentialStatus>,
    task_id: TaskId,
    abort: AbortHandle,
}

struct LoginValidationCompletion {
    result: Result<(), PlaybackCoreError>,
}

enum RuntimeEvent {
    Command(Option<Command>),
    RefreshTick,
    PlayCompleted(Result<PlayCompletion, JoinError>),
    LoginValidationCompleted(Option<Result<(TaskId, LoginValidationCompletion), JoinError>>),
    SearchCompleted(Option<Result<(), JoinError>>),
}

impl LyricState {
    fn generation(&self) -> u64 {
        match self {
            Self::Loading { generation, .. } | Self::Ready { generation, .. } => *generation,
        }
    }

    fn abort(self) {
        if let Self::Loading { task, .. } = self {
            task.abort();
        }
    }
}

impl PlaybackRuntime {
    pub fn start(
        credential_directory: impl Into<PathBuf>,
        kugou_api_base_url: &str,
        audio_cache_config: Option<crate::cache::AudioCacheConfig>,
    ) -> Result<Self, PlaybackError> {
        let credential_directory = credential_directory.into();
        let kugou_api_base_url = kugou_api_base_url.to_owned();
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("miliastra-playback".to_owned())
            .spawn(move || {
                run_runtime_thread(
                    credential_directory,
                    kugou_api_base_url,
                    audio_cache_config,
                    command_rx,
                    ready_tx,
                )
            })
            .map_err(|error| PlaybackError::Startup(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                handle: PlaybackHandle { commands },
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PlaybackError::Startup(
                    "playback thread exited during initialization".to_owned(),
                ))
            }
        }
    }

    pub fn handle(&self) -> PlaybackHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<(), PlaybackError> {
        let result = self.handle.shutdown_runtime();
        self.join_thread()?;
        result
    }

    fn join_thread(&mut self) -> Result<(), PlaybackError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| PlaybackError::Internal("playback thread panicked".to_owned()))
    }
}

impl Drop for PlaybackRuntime {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.handle.shutdown_runtime();
            let _ = self.join_thread();
        }
    }
}

impl PlaybackHandle {
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchCandidate>, PlaybackError> {
        self.request(|reply| Command::Search(query, reply))
    }

    pub fn play(&self, track: PlayableTrack) -> Result<PlaybackOperation, PlaybackError> {
        Ok(PlaybackOperation {
            reply: self.submit(|reply| Command::Play(track, reply))?,
        })
    }

    /// 预加载音源解析：后台执行、立即返回；结果进入解析缓存，
    /// 之后的 `play` 可直接使用缓存跳过网络解析。
    pub fn preload(&self, track: PlayableTrack) -> Result<(), PlaybackError> {
        self.commands
            .try_send(Command::Preload(track))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => PlaybackError::Busy,
                mpsc::error::TrySendError::Closed(_) => PlaybackError::RuntimeStopped,
            })
    }

    pub fn pause(&self) -> Result<(), PlaybackError> {
        self.request(Command::Pause)
    }

    pub fn resume(&self) -> Result<(), PlaybackError> {
        self.request(Command::Resume)
    }

    pub fn stop(&self) -> Result<(), PlaybackError> {
        self.request(Command::Stop)
    }

    pub fn set_volume(&self, volume: u8) -> Result<(), PlaybackError> {
        self.request(|reply| Command::SetVolume(volume, reply))
    }

    pub fn snapshot(&self) -> Result<PlaybackSnapshot, PlaybackError> {
        self.request(Command::Snapshot)
    }

    pub fn providers(&self) -> Result<Vec<ProviderId>, PlaybackError> {
        self.request(Command::Providers)
    }

    pub fn credential_statuses(&self) -> Result<Vec<CredentialStatus>, PlaybackError> {
        self.request(Command::CredentialStatuses)
    }

    pub fn refresh_credential(
        &self,
        provider: ProviderId,
    ) -> Result<CredentialStatus, PlaybackError> {
        self.request(|reply| Command::RefreshCredential(provider, reply))
    }

    pub fn kugou_account_status(&self) -> Result<KugouAccountStatus, PlaybackError> {
        self.request(Command::KugouAccountStatus)
    }

    /// 查询平台账号状态（QQ 音乐/网易云）。平台不支持时返回 None。
    pub fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<ProviderAccountStatus>, PlaybackError> {
        self.request(|reply| Command::AccountStatus(provider, reply))
    }

    pub fn kugou_claim_vip(&self) -> Result<KugouListenReport, PlaybackError> {
        self.request(Command::KugouClaimVip)
    }

    pub fn kugou_upgrade_vip(&self) -> Result<KugouListenReport, PlaybackError> {
        self.request(Command::KugouUpgradeVip)
    }

    pub fn save_credential(
        &self,
        credential: ProviderCredential,
    ) -> Result<CredentialStatus, PlaybackError> {
        self.request(|reply| Command::SaveCredential(credential, reply))
    }

    pub fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, PlaybackError> {
        self.request(|reply| Command::Logout(provider, reply))
    }

    pub fn begin_login(&self, provider: ProviderId) -> Result<LoginSession, PlaybackError> {
        self.request(|reply| Command::BeginLogin(provider, reply))
    }

    pub fn login_status(&self) -> Result<LoginStatus, PlaybackError> {
        self.request(Command::LoginStatus)
    }

    pub fn complete_login(
        &self,
        session_id: Uuid,
        credential: ProviderCredential,
    ) -> Result<LoginOperation, PlaybackError> {
        Ok(LoginOperation {
            reply: self.submit(|reply| Command::CompleteLogin(session_id, credential, reply))?,
        })
    }

    pub fn cancel_login(&self, session_id: Uuid) -> Result<(), PlaybackError> {
        self.request(|reply| Command::CancelLogin(session_id, reply))
    }

    fn shutdown_runtime(&self) -> Result<(), PlaybackError> {
        self.request(Command::Shutdown)
    }

    fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, PlaybackError> {
        self.submit(command)?
            .recv()
            .map_err(|_| PlaybackError::RuntimeStopped)?
    }

    fn submit<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<std_mpsc::Receiver<Result<T, PlaybackError>>, PlaybackError> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        self.commands
            .try_send(command(reply_tx))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => PlaybackError::Busy,
                mpsc::error::TrySendError::Closed(_) => PlaybackError::RuntimeStopped,
            })?;
        Ok(reply_rx)
    }
}

impl PlaybackOperation {
    pub fn wait(self) -> Result<(), PlaybackError> {
        self.reply
            .recv()
            .map_err(|_| PlaybackError::RuntimeStopped)?
    }
}

impl LoginOperation {
    pub fn wait(self) -> Result<CredentialStatus, PlaybackError> {
        self.reply
            .recv()
            .map_err(|_| PlaybackError::RuntimeStopped)?
    }

    pub fn wait_timeout(
        &self,
        timeout: Duration,
    ) -> Result<CredentialStatus, LoginOperationWaitError> {
        match self.reply.recv_timeout(timeout) {
            Ok(result) => result.map_err(LoginOperationWaitError::Playback),
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(LoginOperationWaitError::TimedOut),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                Err(LoginOperationWaitError::RuntimeStopped)
            }
        }
    }
}

fn run_runtime_thread(
    credential_directory: PathBuf,
    kugou_api_base_url: String,
    audio_cache_config: Option<crate::cache::AudioCacheConfig>,
    command_rx: mpsc::Receiver<Command>,
    ready: std_mpsc::SyncSender<Result<(), PlaybackError>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("miliastra-playback-worker")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(PlaybackError::Startup(error.to_string())));
            return;
        }
    };
    runtime.block_on(async move {
        let credentials = match CredentialStore::open(credential_directory) {
            Ok(credentials) => credentials,
            Err(error) => {
                let _ = ready.send(Err(PlaybackError::Startup(error.to_string())));
                return;
            }
        };
        let catalog = match build_catalog(&credentials, &kugou_api_base_url) {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let engine = match FfmpegEngine::spawn(FfmpegConfig::default()).await {
            Ok(engine) => Arc::new(engine),
            Err(error) => {
                let _ = ready.send(Err(PlaybackError::Startup(error.to_string())));
                return;
            }
        };
        let audio_cache = match audio_cache_config {
            Some(config) if config.enabled => match crate::cache::AudioCache::spawn(config).await {
                Ok(cache) => {
                    tracing::info!("音频数据缓存已启用");
                    Some(cache)
                }
                Err(error) => {
                    tracing::warn!(%error, "音频数据缓存启动失败，本次运行不启用缓存");
                    None
                }
            },
            _ => None,
        };
        let core = PlaybackCore::new_with_registry(
            catalog,
            ProviderRegistry,
            engine.clone(),
            SOURCE_TIMEOUT,
        );
        let core = match audio_cache {
            Some(cache) => core.with_audio_cache(cache),
            None => core,
        };
        if ready.send(Ok(())).is_err() {
            let _ = engine.shutdown().await;
            return;
        }
        let audio_cache = core.audio_cache.clone();
        let shutdown_reply = run_commands(command_rx, core, credentials).await;
        if let Some(cache) = audio_cache {
            cache.shutdown().await;
        }
        let shutdown_result = engine
            .shutdown()
            .await
            .map_err(|error| PlaybackError::Internal(error.to_string()));
        if let Some(reply) = shutdown_reply {
            let _ = reply.send(shutdown_result);
        }
    });
}

fn build_catalog(
    credentials: &CredentialStore,
    kugou_api_base_url: &str,
) -> Result<SourceCatalog, PlaybackError> {
    let qqmusic = Arc::new(
        QqMusicAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
            .map_err(|error| PlaybackError::Startup(error.to_string()))?,
    );
    let netease = NeteaseAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
        .map_err(|error| PlaybackError::Startup(error.to_string()))?;
    let bilibili = Arc::new(
        BilibiliAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
            .map_err(|error| PlaybackError::Startup(error.to_string()))?,
    );
    let kugou = Arc::new(
        KugouAdapter::new(credentials.clone(), SOURCE_TIMEOUT, kugou_api_base_url)
            .map_err(|error| PlaybackError::Startup(error.to_string()))?,
    );
    Ok(SourceCatalog::new([
        (
            ProviderId::QqMusic.to_string(),
            qqmusic.clone() as Arc<dyn SourceAdapter>,
        ),
        (
            ProviderId::Netease.to_string(),
            Arc::new(netease) as Arc<dyn SourceAdapter>,
        ),
        (
            ProviderId::Bilibili.to_string(),
            bilibili.clone() as Arc<dyn SourceAdapter>,
        ),
        (
            ProviderId::Kugou.to_string(),
            kugou.clone() as Arc<dyn SourceAdapter>,
        ),
    ])
    .with_kugou_account(kugou as Arc<dyn crate::catalog::KugouAccountAdapter>)
    .with_refresh_adapter(
        ProviderId::QqMusic.as_str(),
        qqmusic as Arc<dyn CredentialRefreshAdapter>,
    )
    .with_refresh_adapter(
        ProviderId::Bilibili.as_str(),
        bilibili as Arc<dyn CredentialRefreshAdapter>,
    ))
}

async fn run_commands(
    mut commands: mpsc::Receiver<Command>,
    core: PlaybackCore,
    credentials: CredentialStore,
) -> Option<Reply<()>> {
    let mut active_session: Option<SessionRef> = None;
    let mut active_track: Option<PlayableTrack> = None;
    let mut lyric_state: Option<LyricState> = None;
    let mut pending_play: Option<PendingPlay> = None;
    let mut pending_login: Option<PendingLoginValidation> = None;
    let mut searches = JoinSet::new();
    let mut login_validations = JoinSet::new();
    let mut refresh_tick = tokio::time::interval(Duration::from_secs(60));
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = if let Some(play) = pending_play.as_mut() {
            tokio::select! {
                biased;
                completion = &mut play.task => RuntimeEvent::PlayCompleted(completion),
                completion = login_validations.join_next_with_id(), if !login_validations.is_empty() => {
                    RuntimeEvent::LoginValidationCompleted(completion)
                }
                command = commands.recv() => RuntimeEvent::Command(command),
                _ = refresh_tick.tick() => RuntimeEvent::RefreshTick,
                completion = searches.join_next(), if !searches.is_empty() => {
                    RuntimeEvent::SearchCompleted(completion)
                }
            }
        } else {
            tokio::select! {
                biased;
                completion = login_validations.join_next_with_id(), if !login_validations.is_empty() => {
                    RuntimeEvent::LoginValidationCompleted(completion)
                }
                command = commands.recv() => RuntimeEvent::Command(command),
                _ = refresh_tick.tick() => RuntimeEvent::RefreshTick,
                completion = searches.join_next(), if !searches.is_empty() => {
                    RuntimeEvent::SearchCompleted(completion)
                }
            }
        };

        let command = match event {
            RuntimeEvent::Command(Some(command)) => command,
            RuntimeEvent::Command(None) => {
                cancel_pending_play(&mut pending_play, &core, "playback runtime stopped");
                cancel_pending_login(
                    &mut pending_login,
                    None,
                    "playback runtime stopped during credential validation",
                );
                searches.abort_all();
                login_validations.abort_all();
                clear_lyric_state(&mut lyric_state);
                return None;
            }
            RuntimeEvent::SearchCompleted(Some(Err(error))) if !error.is_cancelled() => {
                tracing::warn!(%error, "playback search task failed");
                continue;
            }
            RuntimeEvent::SearchCompleted(_) => continue,
            RuntimeEvent::RefreshTick => {
                refresh_due_credentials(&core, &credentials).await;
                continue;
            }
            RuntimeEvent::LoginValidationCompleted(completion) => {
                let task_id = match &completion {
                    Some(Ok((task_id, _))) => *task_id,
                    Some(Err(error)) => error.id(),
                    None => continue,
                };
                if pending_login
                    .as_ref()
                    .is_none_or(|pending| pending.task_id != task_id)
                {
                    continue;
                }
                let pending = pending_login
                    .take()
                    .expect("matching login validation exists");
                let result = match completion {
                    Some(Ok((_, completion))) => completion
                        .result
                        .map_err(PlaybackError::from)
                        .and_then(|()| {
                            credentials
                                .save(pending.provider.as_str(), pending.credential)
                                .map_err(internal_error)
                        }),
                    Some(Err(error)) if error.is_cancelled() => Err(PlaybackError::Failure(
                        Failure::new("playback_cancelled", "credential validation was cancelled"),
                    )),
                    Some(Err(error)) => Err(PlaybackError::Internal(format!(
                        "credential validation task failed: {error}"
                    ))),
                    None => unreachable!("empty login completion handled above"),
                };
                core.release_login(pending.session_id);
                let _ = pending.reply.send(result);
                continue;
            }
            RuntimeEvent::PlayCompleted(completion) => {
                let Some(play) = pending_play.take() else {
                    continue;
                };
                let result = match completion {
                    Ok(completion) => completion
                        .result
                        .map(|receipt| {
                            let lyric_core = core.clone();
                            let generation = receipt.generation;
                            lyric_state = Some(LyricState::Loading {
                                generation,
                                task: tokio::spawn(async move {
                                    lyric_core
                                        .lyrics(completion.key, completion.resolver_locator)
                                        .await
                                }),
                            });
                            active_session = Some(receipt.session_ref());
                            active_track = Some(completion.track);
                        })
                        .map_err(PlaybackError::from),
                    Err(error) => Err(PlaybackError::Internal(format!(
                        "playback resolution task failed: {error}"
                    ))),
                };
                let _ = play.reply.send(result);
                continue;
            }
        };

        match command {
            Command::Search(query, reply) => {
                let search_core = core.clone();
                searches.spawn(async move {
                    let _ = reply.send(search(&search_core, query).await);
                });
            }
            Command::Preload(track) => {
                let Ok(key) = track.track_ref.key.to_song_key() else {
                    continue;
                };
                let resolver_locator = track.track_ref.resolver_locator.clone();
                let preload_core = core.clone();
                tokio::spawn(async move {
                    if let Err(error) = preload_core.preload(key, resolver_locator).await {
                        tracing::debug!(%error, "音源预加载失败，播放时将重新解析");
                    }
                });
            }
            Command::Play(track, reply) => {
                let key = track.track_ref.key.to_song_key();
                let Ok(key) = key else {
                    let _ = reply.send(Err(PlaybackError::Internal(key.unwrap_err().to_string())));
                    continue;
                };
                cancel_pending_play(
                    &mut pending_play,
                    &core,
                    "play request was superseded by a newer request",
                );
                clear_lyric_state(&mut lyric_state);
                let resolver_locator = track.track_ref.resolver_locator.clone();
                let play_core = core.clone();
                let task_key = key.clone();
                let task_locator = resolver_locator.clone();
                let (cancellation, cancelled) = oneshot::channel();
                let task = tokio::spawn(async move {
                    let result = play_core
                        .play_cancellable(
                            task_key.clone(),
                            task_locator.clone(),
                            EndBehavior::NotifyController,
                            cancelled,
                        )
                        .await;
                    PlayCompletion {
                        track,
                        key: task_key,
                        resolver_locator: task_locator,
                        result,
                    }
                });
                pending_play = Some(PendingPlay {
                    reply,
                    cancellation,
                    task,
                });
            }
            Command::Pause(reply) => {
                let result = match active_session {
                    Some(session) => block_result(core.pause(session).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
                let _ = reply.send(result);
            }
            Command::Resume(reply) => {
                let result = match active_session {
                    Some(session) => block_result(core.resume(session).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
                let _ = reply.send(result);
            }
            Command::Stop(reply) => {
                let cancelled_pending = cancel_pending_play(
                    &mut pending_play,
                    &core,
                    "play request was cancelled by stop",
                );
                let result = match active_session {
                    Some(session) => block_result(core.stop(session).await),
                    None if cancelled_pending => Ok(()),
                    None => Err(PlaybackError::NoActiveSession),
                };
                if result.is_ok() {
                    clear_lyric_state(&mut lyric_state);
                }
                let _ = reply.send(result);
            }
            Command::SetVolume(volume, reply) => {
                let _ = reply.send(block_result(core.set_volume(volume).await));
            }
            Command::Snapshot(reply) => {
                let engine_snapshot = core.snapshot();
                let lyric_line_text =
                    lyric_line_for_snapshot(&mut lyric_state, &engine_snapshot).await;
                let snapshot =
                    public_snapshot(engine_snapshot, active_track.as_ref(), lyric_line_text);
                let _ = reply.send(Ok(snapshot));
            }
            Command::Providers(reply) => {
                let _ = reply.send(Ok(ProviderId::ALL.to_vec()));
            }
            Command::CredentialStatuses(reply) => {
                let _ = reply.send(credentials.statuses().map_err(internal_error));
            }
            Command::RefreshCredential(provider, reply) => {
                let result = match provider {
                    ProviderId::Kugou => refresh_kugou_credential(&core, &credentials).await,
                    ProviderId::QqMusic | ProviderId::Bilibili => {
                        refresh_manual_credential(&core, &credentials, provider).await
                    }
                    _ => Err(PlaybackError::Failure(Failure::new(
                        "unsupported_provider",
                        "该平台不支持凭据刷新",
                    ))),
                };
                let _ = reply.send(result);
            }
            Command::KugouAccountStatus(reply) => {
                let result = core
                    .kugou_account_status()
                    .await
                    .map_err(PlaybackError::from);
                let _ = reply.send(result);
            }
            Command::AccountStatus(provider, reply) => {
                let result = core
                    .account_status(provider)
                    .await
                    .map_err(PlaybackError::from);
                let _ = reply.send(result);
            }
            Command::KugouClaimVip(reply) => {
                let _ = reply.send(core.kugou_claim_vip().await.map_err(PlaybackError::from));
            }
            Command::KugouUpgradeVip(reply) => {
                let _ = reply.send(core.kugou_upgrade_vip().await.map_err(PlaybackError::from));
            }
            Command::SaveCredential(credential, reply) => {
                let provider = credential.provider();
                let result = credentials
                    .save(provider, credential)
                    .map_err(internal_error);
                let _ = reply.send(result);
            }
            Command::Logout(provider, reply) => {
                let _ = reply.send(
                    credentials
                        .remove(provider.as_str())
                        .map_err(internal_error),
                );
            }
            Command::BeginLogin(provider, reply) => {
                let result = core
                    .acquire_login(provider)
                    .map(|(session_id, provider)| LoginSession {
                        session_id,
                        provider,
                    })
                    .map_err(PlaybackError::Failure);
                let _ = reply.send(result);
            }
            Command::LoginStatus(reply) => {
                let status = core.login_coordinator().active().map_or_else(
                    LoginStatus::default,
                    |(session_id, provider)| LoginStatus {
                        active: true,
                        session_id: Some(session_id),
                        provider: Some(provider),
                    },
                );
                let _ = reply.send(Ok(status));
            }
            Command::CompleteLogin(session_id, credential, reply) => {
                let provider = credential.provider().parse::<ProviderId>();
                let Ok(provider) = provider else {
                    core.release_login(session_id);
                    let _ = reply.send(Err(PlaybackError::Failure(Failure::new(
                        "login_session_invalid",
                        "login session is missing or does not match the credential provider",
                    ))));
                    continue;
                };
                if pending_login.is_some() {
                    let _ = reply.send(Err(PlaybackError::Failure(Failure::new(
                        "login_in_progress",
                        "credential validation is already in progress",
                    ))));
                    continue;
                }
                if !core.owns_login(session_id, provider) {
                    core.release_login(session_id);
                    let _ = reply.send(Err(PlaybackError::Failure(Failure::new(
                        "login_session_invalid",
                        "login session is missing or does not match the credential provider",
                    ))));
                    continue;
                }
                let validation_core = core.clone();
                let validation_credential = credential.clone();
                let abort = login_validations.spawn(async move {
                    LoginValidationCompletion {
                        result: validation_core
                            .validate_credential(provider, &validation_credential)
                            .await,
                    }
                });
                pending_login = Some(PendingLoginValidation {
                    session_id,
                    provider,
                    credential,
                    reply,
                    task_id: abort.id(),
                    abort,
                });
            }
            Command::CancelLogin(session_id, reply) => {
                cancel_pending_login(
                    &mut pending_login,
                    Some(session_id),
                    "credential validation was cancelled",
                );
                core.release_login(session_id);
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown(reply) => {
                cancel_pending_play(
                    &mut pending_play,
                    &core,
                    "playback runtime is shutting down",
                );
                cancel_pending_login(
                    &mut pending_login,
                    None,
                    "playback runtime is shutting down during credential validation",
                );
                searches.abort_all();
                login_validations.abort_all();
                clear_lyric_state(&mut lyric_state);
                return Some(reply);
            }
        }
    }
}

fn cancel_pending_play(
    pending: &mut Option<PendingPlay>,
    core: &PlaybackCore,
    message: &'static str,
) -> bool {
    let Some(play) = pending.take() else {
        return false;
    };
    let _ = play.cancellation.send(());
    let cleanup_core = core.clone();
    tokio::spawn(async move {
        match play.task.await {
            Ok(completion) => {
                if let Ok(receipt) = completion.result
                    && let Err(error) = cleanup_core.stop(receipt.session_ref()).await
                {
                    tracing::debug!(%error, "cancelled playback receipt was already superseded");
                }
            }
            Err(error) if !error.is_cancelled() => {
                tracing::warn!(%error, "cancelled playback task failed during cleanup");
            }
            Err(_) => {}
        }
    });
    let _ = play.reply.send(Err(PlaybackError::Failure(Failure::new(
        "playback_cancelled",
        message,
    ))));
    true
}

fn cancel_pending_login(
    pending: &mut Option<PendingLoginValidation>,
    session_id: Option<Uuid>,
    message: &'static str,
) -> bool {
    if pending
        .as_ref()
        .is_none_or(|pending| session_id.is_some_and(|session_id| pending.session_id != session_id))
    {
        return false;
    }
    let pending = pending.take().expect("matching login validation exists");
    pending.abort.abort();
    let _ = pending.reply.send(Err(PlaybackError::Failure(Failure::new(
        "playback_cancelled",
        message,
    ))));
    true
}

fn clear_lyric_state(state: &mut Option<LyricState>) {
    if let Some(state) = state.take() {
        state.abort();
    }
}

async fn lyric_line_for_snapshot(
    state: &mut Option<LyricState>,
    snapshot: &EngineSnapshot,
) -> Option<String> {
    if state
        .as_ref()
        .is_some_and(|state| state.generation() != snapshot.generation)
    {
        clear_lyric_state(state);
        return None;
    }

    let finished = matches!(
        state.as_ref(),
        Some(LyricState::Loading { task, .. }) if task.is_finished()
    );
    if finished && let Some(LyricState::Loading { generation, task }) = state.take() {
        let lyrics = match task.await {
            Ok(Ok(lyrics)) => lyrics,
            Ok(Err(error)) => {
                tracing::warn!(generation, %error, "timed lyric loading failed");
                None
            }
            Err(error) if error.is_cancelled() => None,
            Err(error) => {
                tracing::warn!(generation, %error, "timed lyric task failed");
                None
            }
        };
        if snapshot.generation == generation {
            *state = Some(LyricState::Ready { generation, lyrics });
        }
    }

    let position_seconds = snapshot.position_seconds?;
    match state.as_ref()? {
        LyricState::Ready { lyrics, .. } => lyrics
            .as_ref()?
            .line_at_seconds(position_seconds)
            .map(str::to_owned),
        LyricState::Loading { .. } => None,
    }
}

async fn search(
    core: &PlaybackCore,
    query: SearchQuery,
) -> Result<Vec<SearchCandidate>, PlaybackError> {
    let limit = query.limit;
    let result = core
        .search(SearchSpec {
            keyword: query.keyword,
            sources: query
                .providers
                .into_iter()
                .map(|provider| provider.to_string())
                .collect(),
            limit,
        })
        .await
        .map_err(PlaybackError::from)?;
    let mut provider_candidates = result
        .outcomes
        .into_iter()
        .map(|outcome| VecDeque::from(outcome.candidates))
        .collect::<Vec<_>>();
    let mut selected = Vec::with_capacity(limit);
    while selected.len() < limit {
        let mut advanced = false;
        for candidates in &mut provider_candidates {
            if let Some(candidate) = candidates.pop_front() {
                selected.push(candidate);
                advanced = true;
                if selected.len() == limit {
                    break;
                }
            }
        }
        if !advanced {
            break;
        }
    }
    selected
        .into_iter()
        .map(|candidate| {
            SearchCandidate::from_song(candidate.song, candidate.eligibility)
                .map_err(|error| PlaybackError::Internal(error.to_string()))
        })
        .collect()
}

fn public_snapshot(
    snapshot: EngineSnapshot,
    active_track: Option<&PlayableTrack>,
    lyric_line_text: Option<String>,
) -> PlaybackSnapshot {
    let track = snapshot.song_key.as_ref().and_then(|song_key| {
        active_track
            .filter(|track| {
                track.track_ref.key.provider.as_str() == song_key.source
                    && track.track_ref.key.id == song_key.id
            })
            .cloned()
    });
    PlaybackSnapshot {
        runtime_identity: snapshot.runtime_identity,
        generation: snapshot.generation,
        session_id: snapshot.session_id,
        state: snapshot.state,
        track,
        position_seconds: snapshot.position_seconds,
        duration_seconds: snapshot.duration_seconds,
        lyric_line_text,
        volume: snapshot.volume,
        last_end_cause: snapshot.last_end_cause,
        end_behavior: snapshot.end_behavior,
        failure: snapshot.failure,
    }
}

/// 手动刷新酷狗凭据，并统一记录刷新元数据。
async fn refresh_kugou_credential(
    core: &PlaybackCore,
    credentials: &CredentialStore,
) -> Result<CredentialStatus, PlaybackError> {
    let provider = ProviderId::Kugou;
    let _ = credentials.mark_refresh_started(provider.as_str());
    let result = match core.refresh_kugou_credential().await {
        Ok(credential) => credentials
            .save(provider.as_str(), credential)
            .map_err(internal_error),
        Err(error) => Err(PlaybackError::from(error)),
    };
    finish_refresh(credentials, provider, result).await
}

/// 手动刷新支持通用刷新协议的平台凭据（QQ 音乐、哔哩哔哩）。
async fn refresh_manual_credential(
    core: &PlaybackCore,
    credentials: &CredentialStore,
    provider: ProviderId,
) -> Result<CredentialStatus, PlaybackError> {
    let _ = credentials.mark_refresh_started(provider.as_str());
    let result = match credentials.get(provider.as_str()) {
        Err(error) => Err(internal_error(error)),
        Ok(None) => Err(PlaybackError::Failure(Failure::new(
            "provider_auth_required",
            "该平台尚未登录",
        ))),
        Ok(Some(credential)) => match core.refresh_credential(provider, &credential).await {
            Ok(Some(refreshed)) => credentials
                .save(provider.as_str(), refreshed)
                .map_err(internal_error),
            // 凭据仍有效、无需刷新：视为正常完成，不记录失败状态，
            // 否则 WEB 面板会把「无需刷新」误显示为刷新失败。
            Ok(None) => credentials
                .status(provider.as_str())
                .map_err(internal_error),
            Err(error) => Err(PlaybackError::from(error)),
        },
    };
    finish_refresh(credentials, provider, result).await
}

/// 记录刷新结果元数据并返回原始结果。
async fn finish_refresh(
    credentials: &CredentialStore,
    provider: ProviderId,
    result: Result<CredentialStatus, PlaybackError>,
) -> Result<CredentialStatus, PlaybackError> {
    let metadata_result = result.as_ref().map(|_| ()).map_err(ToString::to_string);
    let next_check = Some(current_epoch_ms().saturating_add(if result.is_ok() {
        REFRESH_CHECK_INTERVAL_MS
    } else {
        REFRESH_FAILURE_BACKOFF_MS
    }));
    let _ = credentials.mark_refresh_finished(provider.as_str(), metadata_result, next_check);
    result
}

async fn refresh_due_credentials(core: &PlaybackCore, credentials: &CredentialStore) {
    let provider = ProviderId::Kugou;
    let Ok(status) = credentials.status(provider.as_str()) else {
        return;
    };
    let now = current_epoch_ms();
    if !status.refresh_ready
        || status.refresh_state == "failed"
        || status
            .next_refresh_check_at_ms
            .is_some_and(|next_check| next_check > now)
    {
        return;
    }
    if let Err(error) = credentials.mark_refresh_started(provider.as_str()) {
        tracing::warn!(%error, "记录酷狗自动刷新状态失败");
        return;
    }
    let result = match core.refresh_kugou_credential().await {
        Ok(credential) => credentials
            .save(provider.as_str(), credential)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let next_check = Some(now.saturating_add(if result.is_ok() {
        REFRESH_CHECK_INTERVAL_MS
    } else {
        REFRESH_FAILURE_BACKOFF_MS
    }));
    if let Err(error) = credentials.mark_refresh_finished(provider.as_str(), result, next_check) {
        tracing::warn!(%error, "保存酷狗自动刷新结果失败");
    }
}

fn current_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn block_result(result: Result<(), PlaybackCoreError>) -> Result<(), PlaybackError> {
    result.map_err(PlaybackError::from)
}

fn internal_error(error: impl std::fmt::Display) -> PlaybackError {
    PlaybackError::Internal(error.to_string())
}

impl From<PlaybackCoreError> for PlaybackError {
    fn from(error: PlaybackCoreError) -> Self {
        match error {
            PlaybackCoreError::Failure(failure) => Self::Failure(failure),
            PlaybackCoreError::Cancelled => Self::Failure(Failure::new(
                "playback_cancelled",
                "playback operation was cancelled",
            )),
            PlaybackCoreError::Catalog(error) => Self::Failure(error.as_failure(None)),
            PlaybackCoreError::SearchFailed { outcomes } => {
                let failure = outcomes
                    .into_iter()
                    .find_map(|outcome| outcome.failure)
                    .unwrap_or_else(|| Failure::new("search_failed", "no provider completed"));
                Self::Failure(failure)
            }
            PlaybackCoreError::InvalidRequest(message) => {
                Self::Failure(Failure::new("invalid_request", message))
            }
            PlaybackCoreError::UnknownSource(source) => Self::Failure(
                Failure::new("unknown_provider", "provider identifier is unknown")
                    .with_provider(source),
            ),
            PlaybackCoreError::Engine(error) => {
                Self::Failure(Failure::new("playback_engine_failed", error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::{Notify, watch};
    use url::Url;

    use super::*;
    use crate::catalog::{CatalogError, PlaybackEligibility, ProviderSearchCandidate};
    use crate::domain::{PlaybackSnapshot as EngineSnapshot, Song, SongKey, StreamSource};
    use crate::engine::{AudioEngine, EngineCommand, EngineError};
    use crate::lyrics::{TimedLyricLine, TimedLyrics};

    struct FakeSource;

    struct SearchSource {
        provider: ProviderId,
        count: usize,
    }

    #[async_trait]
    impl SourceAdapter for FakeSource {
        async fn search(
            &self,
            spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(vec![ProviderSearchCandidate {
                song: Song {
                    key: SongKey::new("qqmusic", "track-1").unwrap(),
                    resolver_locator: None,
                    title: spec.keyword.clone(),
                    artists: vec!["Singer".to_owned()],
                    album: Some("Album".to_owned()),
                    duration_ms: Some(123_000),
                },
                eligibility: PlaybackEligibility::Unknown,
            }])
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.m4a").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }

        async fn lyrics(
            &self,
            key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<Option<TimedLyrics>, CatalogError> {
            if key.id == "track-1" {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(TimedLyrics::new(vec![TimedLyricLine {
                start_ms: 1_000,
                text: key.id.clone(),
                translation: Some(format!("translated-{}", key.id)),
            }]))
        }
    }

    #[async_trait]
    impl SourceAdapter for SearchSource {
        async fn search(
            &self,
            spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok((0..self.count)
                .map(|index| ProviderSearchCandidate {
                    song: Song {
                        key: SongKey::new(
                            self.provider.as_str(),
                            format!("{}-{index}", self.provider.as_str()),
                        )
                        .unwrap(),
                        resolver_locator: None,
                        title: spec.keyword.clone(),
                        artists: vec!["Singer".to_owned()],
                        album: None,
                        duration_ms: Some(180_000),
                    },
                    eligibility: PlaybackEligibility::Eligible,
                })
                .collect())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.m4a").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    struct BlockingSource {
        resolve_started: Mutex<Option<std_mpsc::SyncSender<()>>>,
        release_resolve: Notify,
    }

    #[async_trait]
    impl SourceAdapter for BlockingSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            if let Some(started) = self.resolve_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.release_resolve.notified().await;
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.m4a").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    struct BlockingSearchSource {
        search_started: Mutex<Option<std_mpsc::SyncSender<()>>>,
        release_search: Notify,
    }

    struct BlockingCredentialSource {
        validation_started: Mutex<Option<std_mpsc::SyncSender<()>>>,
        release_validation: Notify,
    }

    #[async_trait]
    impl SourceAdapter for BlockingSearchSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            if let Some(started) = self.search_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.release_search.notified().await;
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            unreachable!("search responsiveness test does not resolve tracks")
        }
    }

    #[async_trait]
    impl SourceAdapter for BlockingCredentialSource {
        async fn validate_credential(
            &self,
            _candidate: &ProviderCredential,
        ) -> Result<(), CatalogError> {
            if let Some(started) = self.validation_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.release_validation.notified().await;
            Ok(())
        }

        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&crate::domain::ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            unreachable!("credential responsiveness test does not resolve tracks")
        }
    }

    struct FakeEngine {
        snapshot_tx: watch::Sender<EngineSnapshot>,
        snapshot: watch::Receiver<EngineSnapshot>,
        commands: Mutex<Vec<EngineCommand>>,
    }

    struct CommittingEngine {
        snapshot_tx: watch::Sender<EngineSnapshot>,
        snapshot: watch::Receiver<EngineSnapshot>,
        start_committed: Mutex<Option<std_mpsc::SyncSender<()>>>,
        release_start: Notify,
    }

    impl CommittingEngine {
        fn new(start_committed: std_mpsc::SyncSender<()>) -> Self {
            let (snapshot_tx, snapshot) = watch::channel(EngineSnapshot::default());
            Self {
                snapshot_tx,
                snapshot,
                start_committed: Mutex::new(Some(start_committed)),
                release_start: Notify::new(),
            }
        }
    }

    #[async_trait]
    impl AudioEngine for CommittingEngine {
        async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
            let mut snapshot = self.snapshot.borrow().clone();
            match command {
                EngineCommand::Start {
                    session_id,
                    generation,
                    song_key,
                    end_behavior,
                    ..
                } => {
                    snapshot.generation = generation;
                    snapshot.session_id = Some(session_id);
                    snapshot.song_key = Some(song_key);
                    snapshot.end_behavior = Some(end_behavior);
                    snapshot.state = EngineState::Playing;
                    self.snapshot_tx.send_replace(snapshot);
                    if let Some(committed) = self.start_committed.lock().unwrap().take() {
                        let _ = committed.send(());
                    }
                    self.release_start.notified().await;
                }
                EngineCommand::Stop { session } => {
                    if snapshot.generation == session.generation
                        && snapshot.session_id == Some(session.session_id)
                    {
                        snapshot.state = EngineState::Stopped;
                        self.snapshot_tx.send_replace(snapshot);
                    }
                }
                EngineCommand::Pause { .. }
                | EngineCommand::Resume { .. }
                | EngineCommand::SetVolume { .. }
                | EngineCommand::RefreshStream { .. }
                | EngineCommand::Seek { .. } => {}
            }
            Ok(())
        }

        fn subscribe(&self) -> watch::Receiver<EngineSnapshot> {
            self.snapshot.clone()
        }
    }

    impl FakeEngine {
        fn new() -> Self {
            let (snapshot_tx, snapshot) = watch::channel(EngineSnapshot::default());
            Self {
                snapshot_tx,
                snapshot,
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AudioEngine for FakeEngine {
        async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
            let mut snapshot = self.snapshot.borrow().clone();
            match &command {
                EngineCommand::Start {
                    session_id,
                    generation,
                    song_key,
                    end_behavior,
                    ..
                } => {
                    snapshot.generation = *generation;
                    snapshot.session_id = Some(*session_id);
                    snapshot.song_key = Some(song_key.clone());
                    snapshot.end_behavior = Some(*end_behavior);
                    snapshot.position_seconds = Some(1.5);
                    snapshot.state = EngineState::Playing;
                }
                EngineCommand::Pause { .. } => snapshot.state = EngineState::Paused,
                EngineCommand::Resume { .. } => snapshot.state = EngineState::Playing,
                EngineCommand::Stop { .. } => snapshot.state = EngineState::Stopped,
                EngineCommand::SetVolume { volume } => snapshot.volume = *volume,
                EngineCommand::RefreshStream { .. } | EngineCommand::Seek { .. } => {}
            }
            self.commands.lock().unwrap().push(command);
            self.snapshot_tx.send_replace(snapshot);
            Ok(())
        }

        fn subscribe(&self) -> watch::Receiver<EngineSnapshot> {
            self.snapshot.clone()
        }
    }

    fn test_runtime() -> PlaybackRuntime {
        test_runtime_with_source(Arc::new(FakeSource))
    }

    fn test_runtime_with_source(source: Arc<dyn SourceAdapter>) -> PlaybackRuntime {
        let credentials = CredentialStore::memory();
        let engine = Arc::new(FakeEngine::new());
        test_runtime_with_parts(source, engine, credentials)
    }

    fn test_runtime_with_parts(
        source: Arc<dyn SourceAdapter>,
        engine: Arc<dyn AudioEngine>,
        credentials: CredentialStore,
    ) -> PlaybackRuntime {
        test_runtime_with_catalog(
            vec![(ProviderId::QqMusic.to_string(), source)],
            engine,
            credentials,
        )
    }

    fn test_runtime_with_catalog(
        sources: Vec<(String, Arc<dyn SourceAdapter>)>,
        engine: Arc<dyn AudioEngine>,
        credentials: CredentialStore,
    ) -> PlaybackRuntime {
        let core = PlaybackCore::new_with_registry(
            SourceCatalog::new(sources),
            ProviderRegistry,
            engine,
            Duration::from_secs(1),
        );
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let thread = thread::spawn({
            let credentials = credentials.clone();
            move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        if let Some(reply) = run_commands(command_rx, core, credentials).await {
                            let _ = reply.send(Ok(()));
                        }
                    });
            }
        });
        PlaybackRuntime {
            handle: PlaybackHandle { commands },
            thread: Some(thread),
        }
    }

    #[test]
    fn multi_provider_search_preserves_candidates_from_each_provider() {
        let sources = ProviderId::ALL
            .into_iter()
            .map(|provider| {
                (
                    provider.to_string(),
                    Arc::new(SearchSource { provider, count: 3 }) as Arc<dyn SourceAdapter>,
                )
            })
            .collect();
        let runtime = test_runtime_with_catalog(
            sources,
            Arc::new(FakeEngine::new()),
            CredentialStore::memory(),
        );

        let candidates = runtime
            .handle()
            .search(SearchQuery {
                keyword: "Test Song".to_owned(),
                providers: ProviderId::ALL.to_vec(),
                limit: 4,
            })
            .unwrap();

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.track_ref.key.provider)
                .collect::<Vec<_>>(),
            ProviderId::ALL
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn blocking_resolution_keeps_the_actor_responsive_and_can_be_stopped() {
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let runtime = test_runtime_with_source(Arc::new(BlockingSource {
            resolve_started: Mutex::new(Some(started_tx)),
            release_resolve: Notify::new(),
        }));
        let handle = runtime.handle();
        let submit_started = std::time::Instant::now();
        let play = handle
            .play(PlayableTrack {
                track_ref: crate::model::TrackRef {
                    key: crate::model::TrackKey::new(ProviderId::QqMusic, "blocked").unwrap(),
                    resolver_locator: None,
                },
                metadata: crate::model::TrackMetadata {
                    title: "blocked".to_owned(),
                    artists: vec!["artist".to_owned()],
                    album: None,
                    duration_ms: Some(10_000),
                },
            })
            .unwrap();
        assert!(submit_started.elapsed() < Duration::from_millis(250));
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("play resolution started");

        let snapshot_started = std::time::Instant::now();
        handle.snapshot().expect("snapshot remains responsive");
        assert!(snapshot_started.elapsed() < Duration::from_millis(250));

        let stop_started = std::time::Instant::now();
        handle.stop().expect("stop cancels pending resolution");
        assert!(stop_started.elapsed() < Duration::from_millis(250));
        assert!(matches!(
            play.wait(),
            Err(PlaybackError::Failure(failure)) if failure.code == "playback_cancelled"
        ));

        runtime.shutdown().unwrap();
    }

    #[test]
    fn blocking_search_keeps_the_actor_responsive_and_shutdown_cancels_it() {
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let runtime = test_runtime_with_source(Arc::new(BlockingSearchSource {
            search_started: Mutex::new(Some(started_tx)),
            release_search: Notify::new(),
        }));
        let handle = runtime.handle();
        let search_handle = handle.clone();
        let search = thread::spawn(move || {
            search_handle.search(SearchQuery {
                keyword: "blocked".to_owned(),
                providers: vec![ProviderId::QqMusic],
                limit: 5,
            })
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("provider search started");

        let snapshot_started = std::time::Instant::now();
        handle.snapshot().expect("snapshot remains responsive");
        assert!(snapshot_started.elapsed() < Duration::from_millis(250));

        runtime.shutdown().unwrap();
        assert!(matches!(
            search.join().expect("search caller joins"),
            Err(PlaybackError::RuntimeStopped)
        ));
    }

    #[test]
    fn blocking_credential_validation_is_responsive_and_cancellable() {
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let credentials = CredentialStore::memory();
        let runtime = test_runtime_with_parts(
            Arc::new(BlockingCredentialSource {
                validation_started: Mutex::new(Some(started_tx)),
                release_validation: Notify::new(),
            }),
            Arc::new(FakeEngine::new()),
            credentials.clone(),
        );
        let handle = runtime.handle();
        let login = handle.begin_login(ProviderId::QqMusic).unwrap();
        let complete_handle = handle.clone();
        let completion = thread::spawn(move || {
            complete_handle
                .complete_login(
                    login.session_id,
                    ProviderCredential::QqMusic {
                        cookies: BTreeMap::from([
                            ("uin".to_owned(), "123".to_owned()),
                            ("qqmusic_key".to_owned(), "secret".to_owned()),
                        ]),
                    },
                )
                .and_then(LoginOperation::wait)
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("credential validation started");

        let snapshot_started = std::time::Instant::now();
        handle.snapshot().expect("snapshot remains responsive");
        assert!(snapshot_started.elapsed() < Duration::from_millis(250));

        let cancel_started = std::time::Instant::now();
        handle.cancel_login(login.session_id).unwrap();
        assert!(cancel_started.elapsed() < Duration::from_millis(250));
        assert!(matches!(
            completion.join().expect("completion caller joins"),
            Err(PlaybackError::Failure(failure)) if failure.code == "playback_cancelled"
        ));
        assert!(!handle.login_status().unwrap().active);
        assert!(credentials.get("qqmusic").unwrap().is_none());

        runtime.shutdown().unwrap();
    }

    #[test]
    fn stop_during_start_commit_cannot_leave_untracked_playback() {
        let (committed_tx, committed_rx) = std_mpsc::sync_channel(1);
        let runtime = test_runtime_with_parts(
            Arc::new(FakeSource),
            Arc::new(CommittingEngine::new(committed_tx)),
            CredentialStore::memory(),
        );
        let handle = runtime.handle();
        let play_handle = handle.clone();
        let play = thread::spawn(move || {
            play_handle
                .play(PlayableTrack {
                    track_ref: crate::model::TrackRef {
                        key: crate::model::TrackKey::new(ProviderId::QqMusic, "committed").unwrap(),
                        resolver_locator: None,
                    },
                    metadata: crate::model::TrackMetadata {
                        title: "committed".to_owned(),
                        artists: vec!["artist".to_owned()],
                        album: None,
                        duration_ms: Some(10_000),
                    },
                })
                .and_then(PlaybackOperation::wait)
        });
        committed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("engine start committed");

        handle.stop().expect("stop cancels the committing play");
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while std::time::Instant::now() < deadline
            && handle.snapshot().unwrap().state != EngineState::Stopped
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(handle.snapshot().unwrap().state, EngineState::Stopped);
        assert!(matches!(
            play.join().expect("play caller joins"),
            Err(PlaybackError::Failure(failure)) if failure.code == "playback_cancelled"
        ));

        runtime.shutdown().unwrap();
    }

    #[test]
    fn handle_drives_structured_playback_credentials_and_login() {
        let runtime = test_runtime();
        let handle = runtime.handle();
        let candidates = handle
            .search(SearchQuery {
                keyword: "Test Song".to_owned(),
                providers: vec![ProviderId::QqMusic],
                limit: 5,
            })
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].track_ref.key.id, "track-1");
        let track = PlayableTrack {
            track_ref: candidates[0].track_ref.clone(),
            metadata: candidates[0].metadata.clone(),
        };

        handle.play(track.clone()).unwrap().wait().unwrap();
        assert_eq!(handle.snapshot().unwrap().track, Some(track));
        handle.pause().unwrap();
        assert_eq!(handle.snapshot().unwrap().state, EngineState::Paused);
        handle.resume().unwrap();
        handle.set_volume(42).unwrap();
        assert_eq!(handle.snapshot().unwrap().volume, 42);
        handle.stop().unwrap();
        assert_eq!(handle.snapshot().unwrap().state, EngineState::Stopped);

        let login = handle.begin_login(ProviderId::QqMusic).unwrap();
        assert_eq!(
            handle.login_status().unwrap().provider,
            Some(ProviderId::QqMusic)
        );
        let status = handle
            .complete_login(
                login.session_id,
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "123".to_owned()),
                        ("qqmusic_key".to_owned(), "secret".to_owned()),
                    ]),
                },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(status.configured);
        assert!(!handle.login_status().unwrap().active);
        assert_eq!(handle.credential_statuses().unwrap().len(), 4);
        assert!(!handle.logout(ProviderId::QqMusic).unwrap().configured);

        runtime.shutdown().unwrap();
        assert!(matches!(
            handle.snapshot(),
            Err(PlaybackError::RuntimeStopped)
        ));
    }

    #[test]
    fn timed_lyrics_are_fenced_when_a_new_generation_replaces_a_track() {
        let runtime = test_runtime();
        let handle = runtime.handle();
        let first = PlayableTrack {
            track_ref: crate::model::TrackRef {
                key: crate::model::TrackKey::new(ProviderId::QqMusic, "track-1").unwrap(),
                resolver_locator: None,
            },
            metadata: crate::model::TrackMetadata {
                title: "first".to_owned(),
                artists: vec!["artist".to_owned()],
                album: None,
                duration_ms: Some(10_000),
            },
        };
        let second = PlayableTrack {
            track_ref: crate::model::TrackRef {
                key: crate::model::TrackKey::new(ProviderId::QqMusic, "track-2").unwrap(),
                resolver_locator: None,
            },
            metadata: crate::model::TrackMetadata {
                title: "second".to_owned(),
                artists: vec!["artist".to_owned()],
                album: None,
                duration_ms: Some(10_000),
            },
        };

        handle.play(first).unwrap().wait().unwrap();
        handle.play(second).unwrap().wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            line = handle.snapshot().unwrap().lyric_line_text;
            if line.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(line.as_deref(), Some("translated-track-2"));
        runtime.shutdown().unwrap();
    }
}
