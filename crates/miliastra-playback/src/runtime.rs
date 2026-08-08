use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::catalog::{
    BilibiliAdapter, NeteaseAdapter, ProviderId, ProviderRegistry, QqMusicAdapter, SourceAdapter,
    SourceCatalog,
};
use crate::credentials::{CredentialStatus, CredentialStore, ProviderCredential};
use crate::daemon::{DaemonError, PlayerDaemon};
use crate::domain::{
    EndBehavior, EndCause, EngineState, Failure, PlaybackSnapshot as EngineSnapshot, SearchSpec,
    SessionRef,
};
use crate::engine::{FfmpegConfig, FfmpegEngine};
use crate::model::{PlayableTrack, SearchCandidate, SearchQuery};

const COMMAND_CAPACITY: usize = 32;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);

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

impl PlaybackError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RuntimeStopped => "playback_runtime_stopped",
            Self::Busy => "playback_busy",
            Self::NoActiveSession => "no_active_session",
            Self::Failure(failure) => match failure.code.as_str() {
                "track_unavailable" => "track_unavailable",
                "provider_auth_required" => "provider_auth_required",
                "relogin_required" => "relogin_required",
                "provider_rate_limited" => "provider_rate_limited",
                "provider_timeout" => "provider_timeout",
                "provider_transient" => "provider_transient",
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
    Pause(Reply<()>),
    Resume(Reply<()>),
    Stop(Reply<()>),
    SetVolume(u8, Reply<()>),
    Snapshot(Reply<PlaybackSnapshot>),
    Providers(Reply<Vec<ProviderId>>),
    CredentialStatuses(Reply<Vec<CredentialStatus>>),
    SaveCredential(ProviderCredential, Reply<CredentialStatus>),
    Logout(ProviderId, Reply<CredentialStatus>),
    BeginLogin(ProviderId, Reply<LoginSession>),
    LoginStatus(Reply<LoginStatus>),
    CompleteLogin(Uuid, ProviderCredential, Reply<CredentialStatus>),
    CancelLogin(Uuid, Reply<()>),
    Shutdown(Reply<()>),
}

impl PlaybackRuntime {
    pub fn start(credential_directory: impl Into<PathBuf>) -> Result<Self, PlaybackError> {
        let credential_directory = credential_directory.into();
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("miliastra-playback".to_owned())
            .spawn(move || run_runtime_thread(credential_directory, command_rx, ready_tx))
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

    pub fn play(&self, track: PlayableTrack) -> Result<(), PlaybackError> {
        self.request(|reply| Command::Play(track, reply))
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
    ) -> Result<CredentialStatus, PlaybackError> {
        self.request(|reply| Command::CompleteLogin(session_id, credential, reply))
    }

    pub fn cancel_login(&self, session_id: Uuid) -> Result<(), PlaybackError> {
        self.request(|reply| Command::CancelLogin(session_id, reply))
    }

    fn shutdown_runtime(&self) -> Result<(), PlaybackError> {
        self.request(Command::Shutdown)
    }

    fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, PlaybackError> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        self.commands
            .try_send(command(reply_tx))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => PlaybackError::Busy,
                mpsc::error::TrySendError::Closed(_) => PlaybackError::RuntimeStopped,
            })?;
        reply_rx.recv().map_err(|_| PlaybackError::RuntimeStopped)?
    }
}

fn run_runtime_thread(
    credential_directory: PathBuf,
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
        let adapters = match build_catalog(&credentials) {
            Ok(adapters) => adapters,
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
        let daemon = PlayerDaemon::new_with_registry(
            SourceCatalog::new(adapters),
            ProviderRegistry,
            engine.clone(),
            SOURCE_TIMEOUT,
        );
        if ready.send(Ok(())).is_err() {
            let _ = engine.shutdown().await;
            return;
        }
        let shutdown_reply = run_commands(command_rx, daemon, credentials).await;
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
) -> Result<Vec<Arc<dyn SourceAdapter>>, PlaybackError> {
    let qqmusic = QqMusicAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
        .map_err(|error| PlaybackError::Startup(error.to_string()))?;
    let netease = NeteaseAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
        .map_err(|error| PlaybackError::Startup(error.to_string()))?;
    let bilibili = BilibiliAdapter::new(credentials.clone(), SOURCE_TIMEOUT)
        .map_err(|error| PlaybackError::Startup(error.to_string()))?;
    Ok(vec![
        Arc::new(qqmusic),
        Arc::new(netease),
        Arc::new(bilibili),
    ])
}

async fn run_commands(
    mut commands: mpsc::Receiver<Command>,
    daemon: PlayerDaemon,
    credentials: CredentialStore,
) -> Option<Reply<()>> {
    let mut active_session: Option<SessionRef> = None;
    let mut active_track: Option<PlayableTrack> = None;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Search(query, reply) => {
                let result = search(&daemon, query).await;
                let _ = reply.send(result);
            }
            Command::Play(track, reply) => {
                let key = track.track_ref.key.to_song_key();
                let result = match key {
                    Ok(key) => daemon
                        .play(
                            key,
                            track.track_ref.resolver_locator.clone(),
                            EndBehavior::NotifyController,
                        )
                        .await
                        .map(|receipt| {
                            active_session = Some(receipt.session_ref());
                            active_track = Some(track);
                        })
                        .map_err(PlaybackError::from),
                    Err(error) => Err(PlaybackError::Internal(error.to_string())),
                };
                let _ = reply.send(result);
            }
            Command::Pause(reply) => {
                let result = match active_session {
                    Some(session) => block_result(daemon.pause(session).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
                let _ = reply.send(result);
            }
            Command::Resume(reply) => {
                let result = match active_session {
                    Some(session) => block_result(daemon.resume(session).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
                let _ = reply.send(result);
            }
            Command::Stop(reply) => {
                let result = match active_session {
                    Some(session) => block_result(daemon.stop(session).await),
                    None => Err(PlaybackError::NoActiveSession),
                };
                let _ = reply.send(result);
            }
            Command::SetVolume(volume, reply) => {
                let _ = reply.send(block_result(daemon.set_volume(volume).await));
            }
            Command::Snapshot(reply) => {
                let snapshot = public_snapshot(daemon.snapshot(), active_track.as_ref());
                let _ = reply.send(Ok(snapshot));
            }
            Command::Providers(reply) => {
                let _ = reply.send(Ok(ProviderId::ALL.to_vec()));
            }
            Command::CredentialStatuses(reply) => {
                let _ = reply.send(credentials.statuses().map_err(internal_error));
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
                let result = daemon
                    .acquire_login(provider)
                    .map(|(session_id, provider)| LoginSession {
                        session_id,
                        provider,
                    })
                    .map_err(PlaybackError::Failure);
                let _ = reply.send(result);
            }
            Command::LoginStatus(reply) => {
                let status = daemon.login_coordinator().active().map_or_else(
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
                let result = match provider {
                    Ok(provider) if daemon.owns_login(session_id, provider) => daemon
                        .validate_credential(provider, &credential)
                        .await
                        .map_err(PlaybackError::from)
                        .and_then(|()| {
                            credentials
                                .save(provider.as_str(), credential)
                                .map_err(internal_error)
                        }),
                    _ => Err(PlaybackError::Failure(Failure::new(
                        "login_session_invalid",
                        "login session is missing or does not match the credential provider",
                    ))),
                };
                daemon.release_login(session_id);
                let _ = reply.send(result);
            }
            Command::CancelLogin(session_id, reply) => {
                daemon.release_login(session_id);
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown(reply) => {
                return Some(reply);
            }
        }
    }
    None
}

async fn search(
    daemon: &PlayerDaemon,
    query: SearchQuery,
) -> Result<Vec<SearchCandidate>, PlaybackError> {
    let limit = query.limit;
    let result = daemon
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
    result
        .outcomes
        .into_iter()
        .flat_map(|outcome| outcome.candidates)
        .take(limit)
        .map(|candidate| {
            SearchCandidate::from_song(candidate.song, candidate.eligibility)
                .map_err(|error| PlaybackError::Internal(error.to_string()))
        })
        .collect()
}

fn public_snapshot(
    snapshot: EngineSnapshot,
    active_track: Option<&PlayableTrack>,
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
        volume: snapshot.volume,
        last_end_cause: snapshot.last_end_cause,
        end_behavior: snapshot.end_behavior,
        failure: snapshot.failure,
    }
}

fn block_result(result: Result<(), DaemonError>) -> Result<(), PlaybackError> {
    result.map_err(PlaybackError::from)
}

fn internal_error(error: impl std::fmt::Display) -> PlaybackError {
    PlaybackError::Internal(error.to_string())
}

impl From<DaemonError> for PlaybackError {
    fn from(error: DaemonError) -> Self {
        match error {
            DaemonError::Failure(failure) => Self::Failure(failure),
            DaemonError::Catalog(error) => Self::Failure(error.as_failure(None)),
            DaemonError::SearchFailed { outcomes } => {
                let failure = outcomes
                    .into_iter()
                    .find_map(|outcome| outcome.failure)
                    .unwrap_or_else(|| Failure::new("search_failed", "no provider completed"));
                Self::Failure(failure)
            }
            DaemonError::InvalidRequest(message) => {
                Self::Failure(Failure::new("invalid_request", message))
            }
            DaemonError::UnknownSource(source) => Self::Failure(
                Failure::new("unknown_provider", "provider identifier is unknown")
                    .with_provider(source),
            ),
            DaemonError::Engine(error) => {
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
    use tokio::sync::watch;
    use url::Url;

    use super::*;
    use crate::catalog::CatalogError;
    use crate::domain::{PlaybackSnapshot as EngineSnapshot, Song, SongKey, StreamSource};
    use crate::engine::{AudioEngine, EngineCommand, EngineError};

    struct FakeSource;

    #[async_trait]
    impl SourceAdapter for FakeSource {
        fn source_code(&self) -> &'static str {
            "qqmusic"
        }

        async fn search(&self, spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            Ok(vec![Song {
                key: SongKey::new("qqmusic", "track-1").unwrap(),
                resolver_locator: None,
                title: spec.keyword.clone(),
                artists: vec!["Singer".to_owned()],
                album: Some("Album".to_owned()),
                duration_ms: Some(123_000),
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
    }

    struct FakeEngine {
        snapshot_tx: watch::Sender<EngineSnapshot>,
        snapshot: watch::Receiver<EngineSnapshot>,
        commands: Mutex<Vec<EngineCommand>>,
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
        let credentials = CredentialStore::memory();
        let engine = Arc::new(FakeEngine::new());
        let daemon = PlayerDaemon::new_with_registry(
            SourceCatalog::new([Arc::new(FakeSource) as Arc<dyn SourceAdapter>]),
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
                        if let Some(reply) = run_commands(command_rx, daemon, credentials).await {
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

        handle.play(track.clone()).unwrap();
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
            .unwrap();
        assert!(status.configured);
        assert!(!handle.login_status().unwrap().active);
        assert_eq!(handle.credential_statuses().unwrap().len(), 3);
        assert!(!handle.logout(ProviderId::QqMusic).unwrap().configured);

        runtime.shutdown().unwrap();
        assert!(matches!(
            handle.snapshot(),
            Err(PlaybackError::RuntimeStopped)
        ));
    }
}
