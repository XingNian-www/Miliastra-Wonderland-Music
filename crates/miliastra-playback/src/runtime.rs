use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::{AbortHandle, Id as TaskId, JoinError, JoinHandle as TokioJoinHandle, JoinSet};
use uuid::Uuid;

use crate::catalog::{
    BilibiliAdapter, CredentialRefreshAdapter, KugouAccountStatus, KugouAdapter, KugouListenReport,
    NeteaseAdapter, ProviderAccountStatus, ProviderId, ProviderRegistry, QqMusicAdapter,
    SourceAdapter, SourceCatalog,
};
use crate::core::{PlaybackCore, PlaybackCoreError};
use crate::credentials::{
    CredentialRefreshLease, CredentialStatus, CredentialStore, DAILY_REFRESH_INTERVAL_MS,
    ProviderCredential,
};
use crate::domain::{
    EndBehavior, EndCause, EngineState, Failure, PlaybackSnapshot as EngineSnapshot, SearchSpec,
    SessionRef,
};
use crate::engine::{FfmpegConfig, FfmpegEngine};
use crate::lyrics::TimedLyrics;
use crate::model::{PlayableTrack, SearchCandidate, SearchQuery, TrackKey};

const COMMAND_CAPACITY: usize = 32;
/// 预加载任务最大并发数：超出部分进入等待队列，避免无界 tokio::spawn。
const PRELOAD_CONCURRENCY_LIMIT: usize = 4;
/// 预加载等待队列长度上限：队列满时丢弃新的预加载请求（fire-and-forget）。
const PRELOAD_QUEUE_LIMIT: usize = 32;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_CHECK_INTERVAL_MS: u64 = DAILY_REFRESH_INTERVAL_MS;
const ACCOUNT_STATUS_CHECK_INTERVAL_MS: u64 = DAILY_REFRESH_INTERVAL_MS;
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
    Play(PlayableTrack, bool, Option<f64>, Reply<()>),
    /// 预加载音源解析结果（fire-and-forget，无回复；失败静默）。
    Preload(PlayableTrack),
    Pause(Reply<()>),
    Resume(Reply<()>),
    Stop(Reply<()>),
    /// 跳转到指定播放位置(秒)。
    Seek(f64, Reply<()>),
    SetVolume(u8, Reply<()>),
    Snapshot(Reply<PlaybackSnapshot>),
    ToggleLyrics(Reply<bool>),
    /// 明确设置歌词是否使用翻译（不等价于切换）：恢复播放时应用持久化模式。
    SetLyricsTranslation(bool, Reply<()>),
    Providers(Reply<Vec<ProviderId>>),
    CredentialStatuses(Reply<Vec<CredentialStatus>>),
    RefreshCredential(ProviderId, Reply<CredentialStatus>),
    KugouAccountStatus(Reply<KugouAccountStatus>),
    AccountStatus(ProviderId, Reply<Option<ProviderAccountStatus>>),
    RefreshAccountStatus(ProviderId, Reply<Option<ProviderAccountStatus>>),
    KugouClaimVip(Reply<KugouListenReport>),
    KugouUpgradeVip(Reply<KugouListenReport>),
    SaveCredential(ProviderCredential, Reply<CredentialStatus>),
    Logout(ProviderId, Reply<CredentialStatus>),
    BeginLogin(ProviderId, Reply<LoginSession>),
    LoginStatus(Reply<LoginStatus>),
    CompleteLogin(Uuid, ProviderCredential, Reply<CredentialStatus>),
    CancelLogin(Uuid, Reply<()>),
    /// 删除曲目音频缓存（解码失败后自愈，下次播放重新下载）。
    InvalidateAudioCache(crate::domain::SongKey, Reply<()>),
    /// 查询缓存统计与指定曲目的完整缓存状态。
    CacheStats(
        Vec<crate::domain::SongKey>,
        Reply<(
            Option<crate::cache::AudioCacheStats>,
            Vec<crate::cache::AudioCacheTrackStatus>,
        )>,
    ),
    /// 分页查询磁盘缓存歌曲列表（SQLite 查询走 spawn_blocking，不阻塞事件循环）。
    CachedTracks(usize, usize, Reply<crate::cache::CachedTrackPage>),
    /// 清零单曲统计；保留音频缓存、曲目元数据与歌词。
    ResetTrackStatistics(crate::domain::SongKey, Reply<bool>),
    Shutdown(Reply<()>),
}

enum LyricState {
    Loading {
        generation: u64,
        use_translation: bool,
        task: TokioJoinHandle<Result<Option<TimedLyrics>, PlaybackCoreError>>,
    },
    Ready {
        generation: u64,
        use_translation: bool,
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
    PreloadCompleted(Option<Result<(TaskId, TrackKey), JoinError>>),
}

impl LyricState {
    fn generation(&self) -> u64 {
        match self {
            Self::Loading { generation, .. } | Self::Ready { generation, .. } => *generation,
        }
    }

    fn toggle_translation(&mut self) -> bool {
        match self {
            Self::Loading {
                use_translation, ..
            }
            | Self::Ready {
                use_translation, ..
            } => {
                *use_translation = !*use_translation;
                *use_translation
            }
        }
    }

    /// 明确设置歌词是否使用翻译（幂等，不等价于切换）。
    fn set_translation(&mut self, use_translation: bool) {
        match self {
            Self::Loading {
                use_translation: current,
                ..
            }
            | Self::Ready {
                use_translation: current,
                ..
            } => {
                *current = use_translation;
            }
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
        self.play_with_origin(track, true)
    }

    pub fn play_with_origin(
        &self,
        track: PlayableTrack,
        requested: bool,
    ) -> Result<PlaybackOperation, PlaybackError> {
        self.play_with_seek(track, requested, None)
    }

    /// 播放并可选指定起始位置（秒）：`seek_seconds` 为 None 时从头播放。
    /// 用于重启恢复：新会话从上次可靠进度继续，而不是整首重播。
    pub fn play_with_seek(
        &self,
        track: PlayableTrack,
        requested: bool,
        seek_seconds: Option<f64>,
    ) -> Result<PlaybackOperation, PlaybackError> {
        Ok(PlaybackOperation {
            reply: self.submit(|reply| Command::Play(track, requested, seek_seconds, reply))?,
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

    /// 跳转到指定播放位置(秒);无活动会话时返回 NoActiveSession。
    pub fn seek(&self, position_seconds: f64) -> Result<(), PlaybackError> {
        self.request(|reply| Command::Seek(position_seconds, reply))
    }

    pub fn set_volume(&self, volume: u8) -> Result<(), PlaybackError> {
        self.request(|reply| Command::SetVolume(volume, reply))
    }

    pub fn snapshot(&self) -> Result<PlaybackSnapshot, PlaybackError> {
        self.request(Command::Snapshot)
    }

    /// 切换当前歌曲歌词的原文/翻译显示；返回切换后是否使用翻译。
    pub fn toggle_lyrics(&self) -> Result<bool, PlaybackError> {
        self.request(Command::ToggleLyrics)
    }

    /// 明确设置当前歌曲歌词是否使用翻译（不等价于切换），供恢复播放应用持久化模式。
    pub fn set_lyrics_translation(&self, use_translation: bool) -> Result<(), PlaybackError> {
        self.request(|reply| Command::SetLyricsTranslation(use_translation, reply))
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

    pub fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<ProviderAccountStatus>, PlaybackError> {
        self.request(|reply| Command::RefreshAccountStatus(provider, reply))
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

    /// 删除曲目的音频缓存（解码失败后自愈：下次播放重新下载）。
    pub fn invalidate_audio_cache(&self, key: &TrackKey) -> Result<(), PlaybackError> {
        let song_key = key
            .to_song_key()
            .map_err(|error| PlaybackError::Internal(error.to_string()))?;
        self.request(|reply| Command::InvalidateAudioCache(song_key, reply))
    }

    /// 查询缓存统计与指定曲目的完整缓存状态。
    pub fn cache_stats(
        &self,
        keys: &[TrackKey],
    ) -> Result<
        (
            Option<crate::cache::AudioCacheStats>,
            Vec<crate::cache::AudioCacheTrackStatus>,
        ),
        PlaybackError,
    > {
        let keys = keys
            .iter()
            .filter_map(|key| key.to_song_key().ok())
            .collect();
        self.request(|reply| Command::CacheStats(keys, reply))
    }

    /// 分页查询磁盘缓存歌曲列表（SQLite 查询在后台线程执行，不阻塞调用方）。
    pub fn cached_tracks(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<crate::cache::CachedTrackPage, PlaybackError> {
        self.request(|reply| Command::CachedTracks(offset, limit, reply))
    }

    /// 清零单曲播放与缓存统计。缓存资产、身份元数据和歌词不变。
    pub fn reset_track_statistics(&self, key: &TrackKey) -> Result<bool, PlaybackError> {
        let song_key = key
            .to_song_key()
            .map_err(|error| PlaybackError::Internal(error.to_string()))?;
        self.request(|reply| Command::ResetTrackStatistics(song_key, reply))
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
    // 预加载任务管理：JoinSet 统一跟踪、可整体取消；in-flight 集合实现
    // 按曲目单飞（重复请求合并）；等待队列配合并发上限限流。
    let mut preloads = JoinSet::new();
    let mut preload_inflight: HashSet<TrackKey> = HashSet::new();
    // 预加载任务 id -> 曲目 key：任务 panic/取消时 JoinError 不携带结果，
    // 必须按 id 找回 key 才能释放对应 in-flight 标记，防止该曲目被永久堵住。
    let mut preload_task_keys: HashMap<TaskId, TrackKey> = HashMap::new();
    let mut pending_preloads: VecDeque<PlayableTrack> = VecDeque::new();
    // A slow provider must not let later minute ticks start overlapping daily
    // background refresh passes against the same accounts.
    let background_refresh_lock = Arc::new(AsyncMutex::new(()));
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
                // 预加载完成事件优先于新命令：防止持续命令流饿死 in-flight 释放。
                completion = preloads.join_next_with_id(), if !preloads.is_empty() => {
                    RuntimeEvent::PreloadCompleted(completion)
                }
                _ = refresh_tick.tick() => RuntimeEvent::RefreshTick,
                command = commands.recv() => RuntimeEvent::Command(command),
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
                // 预加载完成事件优先于新命令：防止持续命令流饿死 in-flight 释放。
                completion = preloads.join_next_with_id(), if !preloads.is_empty() => {
                    RuntimeEvent::PreloadCompleted(completion)
                }
                _ = refresh_tick.tick() => RuntimeEvent::RefreshTick,
                command = commands.recv() => RuntimeEvent::Command(command),
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
                preloads.abort_all();
                pending_preloads.clear();
                preload_inflight.clear();
                preload_task_keys.clear();
                clear_lyric_state(&mut lyric_state);
                return None;
            }
            RuntimeEvent::SearchCompleted(Some(Err(error))) if !error.is_cancelled() => {
                tracing::warn!(%error, "playback search task failed");
                continue;
            }
            RuntimeEvent::SearchCompleted(_) => continue,
            RuntimeEvent::PreloadCompleted(completion) => {
                match completion {
                    Some(Ok((task_id, key))) => {
                        // 任务正常完成：释放 in-flight 标记，同曲目可再次预加载。
                        preload_inflight.remove(&key);
                        preload_task_keys.remove(&task_id);
                    }
                    Some(Err(error)) => {
                        // panic/取消：JoinError 不携带任务结果，按 TaskId 找回曲目 key
                        // 并释放 in-flight 标记；否则该 key 会被永久堵住，同曲目再也
                        // 无法预加载。
                        if let Some(key) = preload_task_keys.remove(&error.id()) {
                            preload_inflight.remove(&key);
                        }
                        if !error.is_cancelled() {
                            tracing::warn!(%error, "预加载任务异常结束");
                        }
                    }
                    // 空完成只出现在 JoinSet 清空后，关闭路径已整体清理，无需恢复标记。
                    None => {}
                }
                // 完成的任务空出槽位：按序启动等待队列中的下一个预加载。
                start_pending_preloads(
                    &mut pending_preloads,
                    &mut preloads,
                    &mut preload_task_keys,
                    &core,
                );
                continue;
            }
            RuntimeEvent::RefreshTick => {
                // 自动刷新含网络请求,放入独立任务避免阻塞命令循环。
                let core = core.clone();
                let credentials = credentials.clone();
                let background_refresh_lock = background_refresh_lock.clone();
                tokio::spawn(async move {
                    let Ok(_guard) = background_refresh_lock.try_lock() else {
                        return;
                    };
                    refresh_due_credentials(&core, &credentials).await;
                    refresh_due_account_statuses(&core, &credentials).await;
                });
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
                let Some(pending) = pending_login.take() else {
                    continue;
                };
                let result = match completion {
                    Some(Ok((_, completion))) => completion
                        .result
                        .map_err(PlaybackError::from)
                        .and_then(|()| {
                            let status = credentials
                                .save(pending.provider.as_str(), pending.credential)
                                .map_err(internal_error)?;
                            let _ = core.invalidate_account_status(pending.provider);
                            Ok(status)
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
                                use_translation: true,
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
                let key = track.track_ref.key.clone();
                let Ok(_) = key.to_song_key() else {
                    continue;
                };
                // 单飞：同一曲目已在运行或排队时，合并重复请求。
                if !preload_inflight.insert(key.clone()) {
                    continue;
                }
                // 等待队列满时丢弃新请求（fire-and-forget，失败静默）。
                if pending_preloads.len() >= PRELOAD_QUEUE_LIMIT {
                    preload_inflight.remove(&key);
                    tracing::debug!(?key, "预加载等待队列已满，丢弃请求");
                    continue;
                }
                pending_preloads.push_back(track);
                start_pending_preloads(
                    &mut pending_preloads,
                    &mut preloads,
                    &mut preload_task_keys,
                    &core,
                );
            }
            Command::Play(track, requested, seek_seconds, reply) => {
                let key = match track.track_ref.key.to_song_key() {
                    Ok(key) => key,
                    Err(error) => {
                        let _ = reply.send(Err(PlaybackError::Internal(error.to_string())));
                        continue;
                    }
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
                            Some(&track.metadata),
                            EndBehavior::NotifyController,
                            cancelled,
                            requested,
                            seek_seconds,
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
            Command::Seek(position_seconds, reply) => {
                let result = match active_session {
                    Some(session) => block_result(core.seek(session, position_seconds).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
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
            Command::ToggleLyrics(reply) => {
                let result = lyric_state
                    .as_mut()
                    .map(LyricState::toggle_translation)
                    .ok_or(PlaybackError::NoActiveSession);
                let _ = reply.send(result);
            }
            Command::SetLyricsTranslation(use_translation, reply) => {
                let result = lyric_state
                    .as_mut()
                    .map(|state| {
                        state.set_translation(use_translation);
                    })
                    .ok_or(PlaybackError::NoActiveSession);
                let _ = reply.send(result);
            }
            Command::Providers(reply) => {
                let _ = reply.send(Ok(ProviderId::ALL.to_vec()));
            }
            Command::CredentialStatuses(reply) => {
                let _ = reply.send(credentials.statuses().map_err(internal_error));
            }
            Command::RefreshCredential(provider, reply) => {
                // 网络请求(最长 SOURCE_TIMEOUT)放入独立任务,避免阻塞命令循环。
                let core = core.clone();
                let credentials = credentials.clone();
                tokio::spawn(async move {
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
                    if result.is_ok() {
                        let _ = core.invalidate_account_status(provider);
                    }
                    let _ = reply.send(result);
                });
            }
            Command::KugouAccountStatus(reply) => {
                let core = core.clone();
                tokio::spawn(async move {
                    let result = core
                        .kugou_account_status()
                        .await
                        .map_err(PlaybackError::from);
                    let _ = reply.send(result);
                });
            }
            Command::AccountStatus(provider, reply) => {
                let core = core.clone();
                tokio::spawn(async move {
                    let result = core
                        .account_status(provider)
                        .await
                        .map_err(PlaybackError::from);
                    let _ = reply.send(result);
                });
            }
            Command::RefreshAccountStatus(provider, reply) => {
                let core = core.clone();
                tokio::spawn(async move {
                    let result = core
                        .refresh_account_status(provider)
                        .await
                        .map_err(PlaybackError::from);
                    let _ = reply.send(result);
                });
            }
            Command::KugouClaimVip(reply) => {
                let core = core.clone();
                tokio::spawn(async move {
                    let result = core.kugou_claim_vip().await.map_err(PlaybackError::from);
                    let _ = core.invalidate_account_status(ProviderId::Kugou);
                    let _ = reply.send(result);
                });
            }
            Command::KugouUpgradeVip(reply) => {
                let core = core.clone();
                tokio::spawn(async move {
                    let result = core.kugou_upgrade_vip().await.map_err(PlaybackError::from);
                    let _ = core.invalidate_account_status(ProviderId::Kugou);
                    let _ = reply.send(result);
                });
            }
            Command::SaveCredential(credential, reply) => {
                let provider = credential.provider();
                let result = credentials
                    .save(provider, credential)
                    .map_err(internal_error)
                    .inspect(|_| {
                        if let Ok(provider) = provider.parse::<ProviderId>() {
                            let _ = core.invalidate_account_status(provider);
                        }
                    });
                let _ = reply.send(result);
            }
            Command::Logout(provider, reply) => {
                let result = credentials
                    .remove(provider.as_str())
                    .map_err(internal_error)
                    .inspect(|_| {
                        let _ = core.invalidate_account_status(provider);
                    });
                let _ = reply.send(result);
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
                    core.release_login(session_id);
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
            Command::InvalidateAudioCache(key, reply) => {
                let invalidated = core.invalidate_audio_cache(&key).await;
                let _ = reply.send(Ok(()));
                if invalidated {
                    tracing::info!("已清除音频缓存: {key}");
                }
            }
            Command::CacheStats(keys, reply) => match core.audio_cache.clone() {
                Some(cache) => {
                    tokio::task::spawn_blocking(move || {
                        let (stats, tracks) = cache.stats(&keys);
                        let _ = reply.send(Ok((Some(stats), tracks)));
                    });
                }
                None => {
                    let _ = reply.send(Ok((None, Vec::new())));
                }
            },
            Command::CachedTracks(offset, limit, reply) => match core.audio_cache.clone() {
                Some(cache) => {
                    // SQLite 查询可能耗时，移到阻塞线程池，避免阻塞播放事件循环。
                    tokio::task::spawn_blocking(move || {
                        let page = cache.cached_tracks(offset, limit);
                        let _ = reply.send(Ok(page));
                    });
                }
                None => {
                    let _ = reply.send(Ok(crate::cache::CachedTrackPage::empty(offset, limit)));
                }
            },
            Command::ResetTrackStatistics(key, reply) => match core.audio_cache.clone() {
                Some(cache) => {
                    tokio::spawn(async move {
                        let changed = cache.reset_statistics(&key).await;
                        let _ = reply.send(Ok(changed));
                    });
                }
                None => {
                    let _ = reply.send(Ok(false));
                }
            },
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
                preloads.abort_all();
                pending_preloads.clear();
                preload_inflight.clear();
                preload_task_keys.clear();
                clear_lyric_state(&mut lyric_state);
                return Some(reply);
            }
        }
    }
}

/// 从等待队列启动预加载任务，直到并发数达到上限。
/// 任务统一放入 `JoinSet`：关闭时可整体取消，完成事件由事件循环处理。
fn start_pending_preloads(
    pending: &mut VecDeque<PlayableTrack>,
    preloads: &mut JoinSet<TrackKey>,
    preload_task_keys: &mut HashMap<TaskId, TrackKey>,
    core: &PlaybackCore,
) {
    // 先检查空位再弹出：达到上限时保留队列中的任务，等待后续空位。
    while !pending.is_empty() && preloads.len() < PRELOAD_CONCURRENCY_LIMIT {
        let Some(track) = pending.pop_front() else {
            break;
        };
        let key = track.track_ref.key.clone();
        let Ok(song_key) = key.to_song_key() else {
            continue;
        };
        let resolver_locator = track.track_ref.resolver_locator.clone();
        let preload_core = core.clone();
        // 任务返回曲目 key：完成事件据此释放 in-flight 标记。
        let task_key = key.clone();
        let abort = preloads.spawn(async move {
            if let Err(error) = preload_core
                .preload(song_key, resolver_locator, Some(&track.metadata))
                .await
            {
                tracing::debug!(%error, "音源预加载失败，播放时将重新解析");
            }
            task_key
        });
        // 记录任务 id -> 曲目 key：任务 panic/取消时按 id 找回 key 释放 in-flight。
        preload_task_keys.insert(abort.id(), key);
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
    let Some(pending) = pending.take() else {
        return false;
    };
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
    if finished
        && let Some(LyricState::Loading {
            generation,
            use_translation,
            task,
        }) = state.take()
    {
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
            *state = Some(LyricState::Ready {
                generation,
                use_translation,
                lyrics,
            });
        }
    }

    let position_seconds = snapshot.position_seconds?;
    match state.as_ref()? {
        LyricState::Ready {
            lyrics,
            use_translation,
            ..
        } => lyrics
            .as_ref()?
            .line_at_seconds_with_translation(position_seconds, *use_translation)
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
    let Some(refresh_lease) = credentials
        .try_mark_refresh_started(provider.as_str())
        .map_err(internal_error)?
    else {
        return Err(refresh_in_progress(provider));
    };
    let snapshot = match credentials.snapshot(provider.as_str()) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            let result = Err(PlaybackError::Failure(Failure::new(
                "provider_auth_required",
                "该平台尚未登录",
            )));
            return finish_refresh(credentials, provider, None, false, refresh_lease, result).await;
        }
        Err(error) => {
            let result = Err(internal_error(error));
            return finish_refresh(credentials, provider, None, false, refresh_lease, result).await;
        }
    };
    let (result, credential_written) = match core
        .refresh_kugou_credential(&snapshot.credential)
        .await
    {
        Ok(credential) => {
            match save_refreshed_credential(credentials, provider, snapshot.revision, credential)
                .await
            {
                Ok(Some(status)) => (Ok(status), true),
                Ok(None) => (Err(refresh_superseded()), false),
                Err(error) => (Err(error), false),
            }
        }
        Err(error) => (Err(refresh_failure(error)), false),
    };
    finish_refresh(
        credentials,
        provider,
        Some(snapshot.revision),
        credential_written,
        refresh_lease,
        result,
    )
    .await
}

/// 手动刷新支持通用刷新协议的平台凭据（QQ 音乐、哔哩哔哩）。
async fn refresh_manual_credential(
    core: &PlaybackCore,
    credentials: &CredentialStore,
    provider: ProviderId,
) -> Result<CredentialStatus, PlaybackError> {
    let Some(refresh_lease) = credentials
        .try_mark_refresh_started(provider.as_str())
        .map_err(internal_error)?
    else {
        return Err(refresh_in_progress(provider));
    };
    let snapshot = match credentials.snapshot(provider.as_str()) {
        Err(error) => {
            let result = Err(internal_error(error));
            return finish_refresh(credentials, provider, None, false, refresh_lease, result).await;
        }
        Ok(None) => {
            let result = Err(PlaybackError::Failure(Failure::new(
                "provider_auth_required",
                "该平台尚未登录",
            )));
            return finish_refresh(credentials, provider, None, false, refresh_lease, result).await;
        }
        Ok(Some(snapshot)) => snapshot,
    };
    let (result, credential_written) = match core
        .refresh_credential(provider, &snapshot.credential)
        .await
    {
        Ok(Some(refreshed)) => {
            match save_refreshed_credential(credentials, provider, snapshot.revision, refreshed)
                .await
            {
                Ok(Some(status)) => (Ok(status), true),
                Ok(None) => (Err(refresh_superseded()), false),
                Err(error) => (Err(error), false),
            }
        }
        // 凭据仍有效、无需刷新：视为正常完成，不记录失败状态，
        // 否则 WEB 面板会把「无需刷新」误显示为刷新失败。
        Ok(None) => (
            credentials
                .status(provider.as_str())
                .map_err(internal_error),
            false,
        ),
        Err(error) => (Err(refresh_failure(error)), false),
    };
    finish_refresh(
        credentials,
        provider,
        Some(snapshot.revision),
        credential_written,
        refresh_lease,
        result,
    )
    .await
}

/// 记录刷新结果元数据并返回原始结果。
async fn finish_refresh(
    credentials: &CredentialStore,
    provider: ProviderId,
    started_revision: Option<u64>,
    credential_written: bool,
    _refresh_lease: CredentialRefreshLease,
    result: Result<CredentialStatus, PlaybackError>,
) -> Result<CredentialStatus, PlaybackError> {
    let metadata_result = result.as_ref().map(|_| ()).map_err(ToString::to_string);
    let next_check = Some(current_epoch_ms().saturating_add(if result.is_ok() {
        REFRESH_CHECK_INTERVAL_MS
    } else {
        REFRESH_FAILURE_BACKOFF_MS
    }));
    let Some(started_revision) = started_revision else {
        // No credential snapshot was captured. This can happen for an unconfigured provider or
        // when logout won the race; neither case may leave a failed refresh state behind.
        let _ = credentials.discard_refresh(provider.as_str());
        return result;
    };
    let expected_revision = if credential_written {
        started_revision.wrapping_add(1)
    } else {
        started_revision
    };
    // 网络请求期间若用户退出或重新登录，旧结果不能改写新的元数据或下次检查时间。
    let _ = credentials.mark_refresh_finished_if_current_revision(
        provider.as_str(),
        Some(expected_revision),
        metadata_result,
        next_check,
    );
    result
}

async fn save_refreshed_credential(
    credentials: &CredentialStore,
    provider: ProviderId,
    expected_revision: u64,
    credential: ProviderCredential,
) -> Result<Option<CredentialStatus>, PlaybackError> {
    let store = credentials.clone();
    tokio::task::spawn_blocking(move || {
        store.save_if_revision(provider.as_str(), expected_revision, credential)
    })
    .await
    .map_err(|error| {
        PlaybackError::Failure(Failure::new("credential_save_join", error.to_string()))
    })?
    .map_err(internal_error)
}

fn refresh_in_progress(provider: ProviderId) -> PlaybackError {
    PlaybackError::Failure(
        Failure::new("credential_refresh_in_progress", "该平台凭据正在刷新")
            .with_provider(provider.as_str()),
    )
}

fn refresh_superseded() -> PlaybackError {
    PlaybackError::Failure(Failure::new(
        "credential_changed",
        "登录状态已变更，未写入旧的刷新结果",
    ))
}

fn refresh_failure(error: PlaybackCoreError) -> PlaybackError {
    match error {
        PlaybackCoreError::Catalog(_) => PlaybackError::Failure(Failure::new(
            "credential_refresh_failed",
            "平台拒绝或未完成凭据刷新",
        )),
        error => PlaybackError::from(error),
    }
}

async fn refresh_due_credentials(core: &PlaybackCore, credentials: &CredentialStore) {
    for provider in [ProviderId::Kugou, ProviderId::QqMusic, ProviderId::Bilibili] {
        let Ok(status) = credentials.status(provider.as_str()) else {
            continue;
        };
        let now = current_epoch_ms();
        if !status.refresh_ready
            || status
                .next_refresh_check_at_ms
                .is_some_and(|next_check| next_check > now)
        {
            continue;
        }
        let result = match provider {
            ProviderId::Kugou => refresh_kugou_credential(core, credentials).await,
            ProviderId::QqMusic | ProviderId::Bilibili => {
                refresh_manual_credential(core, credentials, provider).await
            }
            _ => continue,
        };
        if result.is_ok() {
            let _ = core.invalidate_account_status(provider);
        }
        if let Err(error) = result {
            if !matches!(
                error,
                PlaybackError::Failure(ref failure) if failure.code == "credential_refresh_in_progress"
            ) {
                tracing::warn!(provider = %provider, error = %error, "自动刷新凭据失败");
            }
        }
    }
}

/// Daily forced refresh of the account/VIP view for every configured provider.
///
/// This only reads existing credential snapshots and never starts a login flow.
/// A stale/error response is retried on the short refresh backoff; a confirmed
/// status is deferred for the normal daily interval.
async fn refresh_due_account_statuses(core: &PlaybackCore, credentials: &CredentialStore) {
    for provider in ProviderId::ALL {
        let now = current_epoch_ms();
        let snapshot = match credentials.snapshot(provider.as_str()) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(provider = %provider, %error, "读取账号状态刷新凭据失败");
                continue;
            }
        };
        let due = match credentials.account_status_check_due(provider.as_str(), now) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(provider = %provider, %error, "读取账号状态刷新调度失败");
                continue;
            }
        };
        if !due {
            continue;
        }

        let result = core.refresh_account_status(provider).await;
        let delay = match &result {
            Ok(Some(status)) if status.stale => REFRESH_FAILURE_BACKOFF_MS,
            Ok(_) => ACCOUNT_STATUS_CHECK_INTERVAL_MS,
            Err(_) => REFRESH_FAILURE_BACKOFF_MS,
        };
        let next_check_at_ms = current_epoch_ms().saturating_add(delay);
        if let Err(error) = credentials.mark_account_status_check_finished_if_current_revision(
            provider.as_str(),
            snapshot.revision,
            next_check_at_ms,
        ) {
            tracing::warn!(provider = %provider, %error, "保存账号状态刷新调度失败");
        }
        if let Err(error) = result {
            tracing::warn!(provider = %provider, %error, "自动刷新账号状态失败");
        }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::{Notify, oneshot, watch};
    use url::Url;

    use super::*;
    use crate::catalog::{
        CatalogError, KugouAccountAdapter, KugouAccountStatus, KugouListenReport,
        PlaybackEligibility, ProviderAccountStatus, ProviderSearchCandidate,
    };
    use crate::domain::{PlaybackSnapshot as EngineSnapshot, Song, SongKey, StreamSource};
    use crate::engine::{AudioEngine, EngineCommand, EngineError};
    use crate::lyrics::{TimedLyricLine, TimedLyrics};

    struct FakeSource;

    struct DailyAccountStatusSource {
        forced_calls: Arc<AtomicUsize>,
        stale: bool,
    }

    struct SearchSource {
        provider: ProviderId,
        count: usize,
    }

    #[test]
    fn invalid_resolver_locator_is_exposed_as_track_unavailable() {
        let error = PlaybackError::from(crate::core::PlaybackCoreError::Catalog(
            CatalogError::InvalidResolverLocator("bad album id".to_owned()),
        ));

        assert_eq!(error.code(), "track_unavailable");
        assert!(matches!(
            error,
            PlaybackError::Failure(failure)
                if failure.code == "track_unavailable"
                    && failure.message == "track resolver metadata is invalid"
        ));
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
    impl SourceAdapter for DailyAccountStatusSource {
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
            unreachable!("daily account refresh test does not resolve tracks")
        }

        async fn refresh_account_status(
            &self,
        ) -> Result<Option<ProviderAccountStatus>, CatalogError> {
            self.forced_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(ProviderAccountStatus {
                logged_in: true,
                vip_known: true,
                stale: self.stale,
                ..ProviderAccountStatus::default()
            }))
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

    /// 预加载测试用假源：resolve 进入时计数、可阻塞，逐个释放后计数完成；
    /// `fail_mode` 为 true 时 resolve 直接失败（不阻塞、不写解析缓存）。
    struct GatedResolveSource {
        resolve_entries: Arc<AtomicUsize>,
        resolve_completed: Arc<AtomicUsize>,
        release_resolve: Arc<Notify>,
        fail_mode: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SourceAdapter for GatedResolveSource {
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
            self.resolve_entries.fetch_add(1, Ordering::SeqCst);
            if self.fail_mode.load(Ordering::SeqCst) {
                return Err(CatalogError::Unavailable("gated failure".to_owned()));
            }
            self.release_resolve.notified().await;
            self.resolve_completed.fetch_add(1, Ordering::SeqCst);
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.m4a").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    struct BlockingCredentialSource {
        validation_started: Mutex<Option<std_mpsc::SyncSender<()>>>,
        release_validation: Notify,
    }

    struct BlockingKugouRefresh {
        refresh_started: Mutex<Option<oneshot::Sender<ProviderCredential>>>,
        release_refresh: Arc<Notify>,
        refreshed: ProviderCredential,
    }

    /// 预加载 panic 测试用假源：首次 resolve 直接 panic（模拟任务异常），
    /// 之后 resolve 阻塞等待释放（用于验证 panic 后同曲目可再次预加载）。
    struct PanicResolveSource {
        resolve_entries: Arc<AtomicUsize>,
        release_resolve: Arc<Notify>,
        panic_mode: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SourceAdapter for PanicResolveSource {
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
            self.resolve_entries.fetch_add(1, Ordering::SeqCst);
            // 仅首次 panic；panic 后自动退出 panic 模式，后续解析正常执行。
            if self.panic_mode.swap(false, Ordering::SeqCst) {
                panic!("模拟预加载任务 panic");
            }
            self.release_resolve.notified().await;
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.m4a").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
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

    #[async_trait]
    impl KugouAccountAdapter for BlockingKugouRefresh {
        async fn refresh_token(
            &self,
            credential: &ProviderCredential,
        ) -> Result<ProviderCredential, CatalogError> {
            if let Some(started) = self.refresh_started.lock().unwrap().take() {
                let _ = started.send(credential.clone());
            }
            self.release_refresh.notified().await;
            Ok(self.refreshed.clone())
        }

        async fn account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
            Ok(KugouAccountStatus::default())
        }

        async fn claim_vip(&self) -> Result<KugouListenReport, CatalogError> {
            Ok(KugouListenReport::default())
        }

        async fn upgrade_vip(&self) -> Result<KugouListenReport, CatalogError> {
            Ok(KugouListenReport::default())
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

    /// 构造测试曲目。
    fn playable_track(id: &str) -> PlayableTrack {
        PlayableTrack {
            track_ref: crate::model::TrackRef {
                key: crate::model::TrackKey::new(ProviderId::QqMusic, id).unwrap(),
                resolver_locator: None,
            },
            metadata: crate::model::TrackMetadata {
                title: id.to_owned(),
                artists: vec!["artist".to_owned()],
                album: None,
                duration_ms: Some(10_000),
            },
        }
    }

    /// 轮询等待条件成立，超时返回 false。
    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    fn kugou_credential(token: &str) -> ProviderCredential {
        ProviderCredential::Kugou {
            token: token.to_owned(),
            userid: "42".to_owned(),
            dfid: "device-42".to_owned(),
            cookies: BTreeMap::new(),
        }
    }

    fn save_all_provider_credentials(credentials: &CredentialStore) {
        credentials
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "42".to_owned()),
                        ("qqmusic_key".to_owned(), "key".to_owned()),
                    ]),
                },
            )
            .unwrap();
        credentials
            .save(
                "netease",
                ProviderCredential::Netease {
                    cookies: BTreeMap::from([("MUSIC_U".to_owned(), "session".to_owned())]),
                },
            )
            .unwrap();
        credentials
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]),
                    refresh_token: None,
                },
            )
            .unwrap();
        credentials
            .save("kugou", kugou_credential("token"))
            .unwrap();
    }

    fn account_refresh_core(forced_calls: Arc<AtomicUsize>, stale: bool) -> PlaybackCore {
        let source: Arc<dyn SourceAdapter> = Arc::new(DailyAccountStatusSource {
            forced_calls,
            stale,
        });
        let sources = ProviderId::ALL
            .into_iter()
            .map(|provider| (provider.to_string(), source.clone()))
            .collect::<Vec<_>>();
        PlaybackCore::new(
            SourceCatalog::new(sources),
            Arc::new(FakeEngine::new()),
            Duration::from_secs(1),
        )
    }

    fn mark_account_checks_due(credentials: &CredentialStore) {
        for provider in ProviderId::ALL {
            let snapshot = credentials
                .snapshot(provider.as_str())
                .unwrap()
                .expect("configured credential must have a snapshot");
            assert!(
                credentials
                    .mark_account_status_check_finished_if_current_revision(
                        provider.as_str(),
                        snapshot.revision,
                        0,
                    )
                    .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn kugou_refresh_uses_the_original_snapshot_and_cannot_overwrite_newer_login() {
        let credentials = CredentialStore::memory();
        let old_credential = kugou_credential("old-token");
        let newer_credential = kugou_credential("newer-token");
        credentials.save("kugou", old_credential.clone()).unwrap();

        let (refresh_started_tx, refresh_started_rx) = oneshot::channel();
        let release_refresh = Arc::new(Notify::new());
        let refresh_adapter = Arc::new(BlockingKugouRefresh {
            refresh_started: Mutex::new(Some(refresh_started_tx)),
            release_refresh: release_refresh.clone(),
            refreshed: kugou_credential("refreshed-old-token"),
        });
        let catalog = SourceCatalog::new(Vec::<(String, Arc<dyn SourceAdapter>)>::new())
            .with_kugou_account(refresh_adapter as Arc<dyn KugouAccountAdapter>);
        let core = Arc::new(PlaybackCore::new(
            catalog,
            Arc::new(FakeEngine::new()),
            Duration::from_secs(1),
        ));

        let task_core = core.clone();
        let task_credentials = credentials.clone();
        let refresh =
            tokio::spawn(
                async move { refresh_kugou_credential(&task_core, &task_credentials).await },
            );
        let received = tokio::time::timeout(Duration::from_secs(1), refresh_started_rx)
            .await
            .expect("refresh should start")
            .expect("refresh adapter should receive a credential");
        assert_eq!(received, old_credential);

        // 新登录在旧刷新请求返回前完成；版本条件写入必须保留这个新凭据。
        credentials.save("kugou", newer_credential.clone()).unwrap();
        release_refresh.notify_one();

        assert!(matches!(
            refresh.await.unwrap(),
            Err(PlaybackError::Failure(failure)) if failure.code == "credential_changed"
        ));
        assert_eq!(
            credentials.get("kugou").unwrap(),
            Some(newer_credential),
            "旧刷新结果不得覆盖新登录凭据"
        );
    }

    #[tokio::test]
    async fn newly_saved_credentials_are_not_immediately_due_for_background_refresh() {
        let credentials = CredentialStore::memory();
        credentials
            .save("kugou", kugou_credential("token"))
            .unwrap();
        let (refresh_started_tx, mut refresh_started_rx) = oneshot::channel();
        let refresh_adapter = Arc::new(BlockingKugouRefresh {
            refresh_started: Mutex::new(Some(refresh_started_tx)),
            release_refresh: Arc::new(Notify::new()),
            refreshed: kugou_credential("refreshed-token"),
        });
        let core = PlaybackCore::new(
            SourceCatalog::new(Vec::<(String, Arc<dyn SourceAdapter>)>::new())
                .with_kugou_account(refresh_adapter as Arc<dyn KugouAccountAdapter>),
            Arc::new(FakeEngine::new()),
            Duration::from_secs(1),
        );

        tokio::time::timeout(
            Duration::from_millis(100),
            refresh_due_credentials(&core, &credentials),
        )
        .await
        .expect("a newly saved credential should be skipped without a network refresh");
        assert!(refresh_started_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn daily_account_refresh_checks_each_configured_provider_once() {
        let credentials = CredentialStore::memory();
        save_all_provider_credentials(&credentials);
        mark_account_checks_due(&credentials);
        let forced_calls = Arc::new(AtomicUsize::new(0));
        let core = account_refresh_core(forced_calls.clone(), false);

        refresh_due_account_statuses(&core, &credentials).await;

        assert_eq!(forced_calls.load(Ordering::SeqCst), ProviderId::ALL.len());
        for provider in ProviderId::ALL {
            assert!(
                !credentials
                    .account_status_check_due(provider.as_str(), current_epoch_ms())
                    .unwrap()
            );
        }
        refresh_due_account_statuses(&core, &credentials).await;
        assert_eq!(forced_calls.load(Ordering::SeqCst), ProviderId::ALL.len());
    }

    #[tokio::test]
    async fn stale_daily_account_status_uses_the_short_retry_backoff() {
        let credentials = CredentialStore::memory();
        credentials
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "42".to_owned()),
                        ("qqmusic_key".to_owned(), "key".to_owned()),
                    ]),
                },
            )
            .unwrap();
        let snapshot = credentials.snapshot("qqmusic").unwrap().unwrap();
        credentials
            .mark_account_status_check_finished_if_current_revision("qqmusic", snapshot.revision, 0)
            .unwrap();
        let forced_calls = Arc::new(AtomicUsize::new(0));
        let core = account_refresh_core(forced_calls.clone(), true);
        let before_refresh = current_epoch_ms();

        refresh_due_account_statuses(&core, &credentials).await;

        assert_eq!(forced_calls.load(Ordering::SeqCst), 1);
        assert!(
            !credentials
                .account_status_check_due(
                    "qqmusic",
                    before_refresh.saturating_add(REFRESH_FAILURE_BACKOFF_MS - 1),
                )
                .unwrap()
        );
        assert!(
            credentials
                .account_status_check_due(
                    "qqmusic",
                    current_epoch_ms().saturating_add(REFRESH_FAILURE_BACKOFF_MS),
                )
                .unwrap()
        );
    }

    #[tokio::test]
    async fn refreshing_an_unconfigured_provider_does_not_record_a_failed_state() {
        let credentials = CredentialStore::memory();
        let core = PlaybackCore::new(
            SourceCatalog::new(Vec::<(String, Arc<dyn SourceAdapter>)>::new()),
            Arc::new(FakeEngine::new()),
            Duration::from_secs(1),
        );

        let result = refresh_manual_credential(&core, &credentials, ProviderId::QqMusic).await;

        assert!(matches!(
            result,
            Err(PlaybackError::Failure(failure)) if failure.code == "provider_auth_required"
        ));
        let status = credentials.status("qqmusic").unwrap();
        assert!(!status.configured);
        assert_eq!(status.refresh_state, "unavailable");
        assert!(status.last_refresh_at_ms.is_none());
        assert!(status.last_refresh_error.is_none());
    }

    #[test]
    fn duplicate_preloads_are_coalesced_and_slot_released_on_completion() {
        let entries = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        // 首次解析失败（不写解析缓存），使「任务完成后同曲目再次解析」可观测。
        let fail_mode = Arc::new(AtomicBool::new(true));
        let runtime = test_runtime_with_source(Arc::new(GatedResolveSource {
            resolve_entries: entries.clone(),
            resolve_completed: completed.clone(),
            release_resolve: release.clone(),
            fail_mode: fail_mode.clone(),
        }));
        let handle = runtime.handle();

        // 同一曲目连续预加载三次：单飞合并，只启动一次解析。
        let track = playable_track("track-dup");
        handle.preload(track.clone()).unwrap();
        handle.preload(track.clone()).unwrap();
        handle.preload(track).unwrap();
        assert!(wait_until(Duration::from_secs(1), || entries
            .load(Ordering::SeqCst)
            >= 1));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(entries.load(Ordering::SeqCst), 1, "重复请求应被合并");

        // 首个任务（失败）完成后 in-flight 标记释放：同曲目再次预加载被接受并重新解析。
        // 轮询重发：in-flight 释放前的请求仍会被合并丢弃。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && entries.load(Ordering::SeqCst) < 2 {
            match handle.preload(playable_track("track-dup")) {
                Ok(()) | Err(PlaybackError::Busy) => {}
                Err(error) => panic!("预加载命令发送失败: {error:?}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            entries.load(Ordering::SeqCst) >= 2,
            "任务完成后应释放 in-flight，同曲目可再次预加载"
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn preloads_respect_concurrency_limit_and_drain_queue_on_completion() {
        let entries = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let runtime = test_runtime_with_source(Arc::new(GatedResolveSource {
            resolve_entries: entries.clone(),
            resolve_completed: completed.clone(),
            release_resolve: release.clone(),
            fail_mode: Arc::new(AtomicBool::new(false)),
        }));
        let handle = runtime.handle();

        // 发送超过并发上限的不同曲目：多余请求进入等待队列。
        let total = PRELOAD_CONCURRENCY_LIMIT + 2;
        for index in 0..total {
            handle
                .preload(playable_track(&format!("track-{index}")))
                .unwrap();
        }
        assert!(wait_until(Duration::from_secs(1), || entries
            .load(Ordering::SeqCst)
            >= PRELOAD_CONCURRENCY_LIMIT));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            entries.load(Ordering::SeqCst),
            PRELOAD_CONCURRENCY_LIMIT,
            "超出上限的预加载必须排队等待"
        );

        // 排队中的曲目重复预加载：同样合并，不重复解析。
        handle
            .preload(playable_track(&format!(
                "track-{}",
                PRELOAD_CONCURRENCY_LIMIT
            )))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(entries.load(Ordering::SeqCst), PRELOAD_CONCURRENCY_LIMIT);

        // 逐个释放：完成的任务空出槽位，等待队列按序启动。
        for expected in (PRELOAD_CONCURRENCY_LIMIT + 1)..=total {
            release.notify_one();
            assert!(
                wait_until(Duration::from_secs(1), || {
                    entries.load(Ordering::SeqCst) >= expected
                }),
                "任务完成后应启动队列中的下一个预加载"
            );
        }
        // 释放剩余全部任务，等待全部完成。
        for _ in 0..PRELOAD_CONCURRENCY_LIMIT {
            release.notify_one();
        }
        assert!(wait_until(Duration::from_secs(1), || completed
            .load(Ordering::SeqCst)
            >= total));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            entries.load(Ordering::SeqCst),
            total,
            "不得重复解析同一曲目"
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn panicked_preload_releases_inflight_so_the_key_can_be_preloaded_again() {
        let entries = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let runtime = test_runtime_with_source(Arc::new(PanicResolveSource {
            resolve_entries: entries.clone(),
            release_resolve: release.clone(),
            panic_mode: Arc::new(AtomicBool::new(true)),
        }));
        let handle = runtime.handle();

        // 首次预加载：任务 panic（JoinError 且未取消）。in-flight 标记必须被释放，
        // 否则该曲目被永久堵住，同曲目再也无法预加载。
        handle.preload(playable_track("track-panic")).unwrap();
        assert!(wait_until(Duration::from_secs(1), || entries
            .load(Ordering::SeqCst)
            >= 1));

        // 轮询重发同曲目：panic 任务的完成事件被事件循环处理后 in-flight 释放，
        // 第二次解析被接受并启动（阻塞等待释放）。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && entries.load(Ordering::SeqCst) < 2 {
            match handle.preload(playable_track("track-panic")) {
                Ok(()) | Err(PlaybackError::Busy) => {}
                Err(error) => panic!("预加载命令发送失败: {error:?}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            entries.load(Ordering::SeqCst) >= 2,
            "预加载任务 panic 后应释放 in-flight，同曲目可再次预加载"
        );

        // 释放第二次解析，随后关闭（关闭取消仍在运行的任务，不等待阻塞）。
        release.notify_one();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_aborts_in_flight_and_queued_preloads() {
        let entries = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let runtime = test_runtime_with_source(Arc::new(GatedResolveSource {
            resolve_entries: entries.clone(),
            resolve_completed: Arc::new(AtomicUsize::new(0)),
            release_resolve: release.clone(),
            fail_mode: Arc::new(AtomicBool::new(false)),
        }));
        let handle = runtime.handle();

        // 一个任务运行中阻塞，两个请求排队。
        handle.preload(playable_track("track-a")).unwrap();
        handle.preload(playable_track("track-b")).unwrap();
        handle.preload(playable_track("track-c")).unwrap();
        assert!(wait_until(Duration::from_secs(1), || entries
            .load(Ordering::SeqCst)
            >= 1));

        // 关闭时取消运行中的任务并丢弃排队请求，不等待阻塞中的解析。
        let started = std::time::Instant::now();
        runtime.shutdown().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "关闭不得等待阻塞中的预加载"
        );
        assert!(matches!(
            handle.preload(playable_track("track-d")),
            Err(PlaybackError::RuntimeStopped)
        ));
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
    fn play_with_seek_forwards_the_restored_position_to_the_engine_start() {
        let source: Arc<dyn SourceAdapter> = Arc::new(FakeSource);
        let credentials = CredentialStore::memory();
        let engine = Arc::new(FakeEngine::new());
        let runtime = test_runtime_with_parts(source, engine.clone(), credentials);
        let handle = runtime.handle();
        let track = playable_track("track-1");

        handle
            .play_with_seek(track.clone(), false, Some(73.5))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(handle.snapshot().unwrap().track, Some(track));

        // 恢复起始位置必须随 Start 命令到达引擎（新会话从该进度续播）。
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EngineCommand::Start { seek_seconds: Some(seek), .. }] if (*seek - 73.5).abs() < 1e-9
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

    #[test]
    fn set_lyrics_translation_applies_the_mode_without_toggling() {
        let runtime = test_runtime();
        let handle = runtime.handle();
        let track = playable_track("track-1");

        handle.play(track).unwrap().wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            line = handle.snapshot().unwrap().lyric_line_text;
            if line.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // 起播默认使用翻译。
        assert_eq!(line.as_deref(), Some("translated-track-1"));

        // 明确设置为原文（不依赖 toggle 的翻转计数）。
        handle.set_lyrics_translation(false).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            line = handle.snapshot().unwrap().lyric_line_text;
            if line == Some("track-1".to_owned()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(line.as_deref(), Some("track-1"));

        // 再明确设置为翻译：直接生效，不翻转。
        handle.set_lyrics_translation(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            line = handle.snapshot().unwrap().lyric_line_text;
            if line == Some("translated-track-1".to_owned()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(line.as_deref(), Some("translated-track-1"));

        // 无活动会话时明确失败（与 toggle 一致）。
        handle.stop().unwrap();
        assert!(matches!(
            handle.set_lyrics_translation(true),
            Err(PlaybackError::NoActiveSession)
        ));

        runtime.shutdown().unwrap();
    }
}
