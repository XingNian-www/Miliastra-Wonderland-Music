use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

use crate::catalog::{
    CatalogError, Failure, ProviderRegistry, ProviderSearchOutcome, SourceCatalog,
};
use crate::credentials::ProviderCredential;
use crate::domain::{
    EndBehavior, EndCause, EngineState, PlaybackSnapshot, ResolverLocator, SearchSpec, SessionRef,
    Song, SongKey,
};
use crate::engine::{AudioEngine, EngineCommand, EngineError};
use crate::login::LoginCoordinator;

#[derive(Clone)]
pub struct PlayerDaemon {
    catalog: SourceCatalog,
    registry: Option<ProviderRegistry>,
    engine: Arc<dyn AudioEngine>,
    source_timeout: Duration,
    generation: Arc<AtomicU64>,
    runtime_identity: Arc<str>,
    login: LoginCoordinator,
}

impl PlayerDaemon {
    pub fn new(
        catalog: SourceCatalog,
        engine: Arc<dyn AudioEngine>,
        source_timeout: Duration,
    ) -> Self {
        Self {
            catalog,
            registry: None,
            engine,
            source_timeout,
            generation: Arc::new(AtomicU64::new(0)),
            runtime_identity: Uuid::new_v4().to_string().into(),
            login: LoginCoordinator::default(),
        }
    }

    pub fn new_with_registry(
        catalog: SourceCatalog,
        registry: ProviderRegistry,
        engine: Arc<dyn AudioEngine>,
        source_timeout: Duration,
    ) -> Self {
        Self {
            catalog,
            registry: Some(registry),
            engine,
            source_timeout,
            generation: Arc::new(AtomicU64::new(0)),
            runtime_identity: Uuid::new_v4().to_string().into(),
            login: LoginCoordinator::default(),
        }
    }

    pub async fn search(&self, spec: SearchSpec) -> Result<CatalogSearchResult, DaemonError> {
        let keyword = spec.keyword.trim();
        if keyword.is_empty() {
            return Err(DaemonError::InvalidRequest(
                "keyword must not be empty".to_owned(),
            ));
        }
        if spec.limit == 0 || spec.limit > 50 {
            return Err(DaemonError::InvalidRequest(
                "limit must be between 1 and 50".to_owned(),
            ));
        }

        let sources = self.requested_sources(spec.sources)?;
        let requests = sources.iter().cloned().map(|source| {
            let request_source = source.clone();
            let catalog = self.catalog.clone();
            let registry = self.registry.clone();
            let keyword = keyword.to_owned();
            let request_timeout = self.source_timeout;
            let per_source_limit = spec.limit;
            let task = tokio::spawn(async move {
                if let Some(registry) = registry.as_ref()
                    && let Err(failure) = registry.require_enabled(&request_source)
                {
                    return Err(failure);
                }
                let Some(adapter) = catalog.get(&request_source) else {
                    return Err(CatalogError::UnknownSource(request_source.clone())
                        .as_failure(Some(&request_source)));
                };
                let source_spec = SearchSpec {
                    keyword,
                    sources: vec![request_source.clone()],
                    limit: per_source_limit,
                };
                timeout(request_timeout, adapter.search_candidates(&source_spec))
                    .await
                    .map_err(|_| {
                        CatalogError::TimedOut(request_source.clone())
                            .as_failure(Some(&request_source))
                    })?
                    .map_err(|error| error.as_failure(Some(&request_source)))
            });
            (source, task)
        });

        let mut outcomes = Vec::with_capacity(sources.len());
        for (source, task) in requests {
            let result = task.await.unwrap_or_else(|error| {
                Err(Failure::new(
                    "provider_transient",
                    format!("{source} search task failed: {error}"),
                )
                .with_provider(source.clone()))
            });
            match result {
                Ok(candidates) => outcomes.push(ProviderSearchOutcome::success(source, candidates)),
                Err(error) => outcomes.push(ProviderSearchOutcome::failed(source.clone(), error)),
            }
        }
        let successful = outcomes
            .iter()
            .filter(|outcome| outcome.is_success())
            .count();
        if self.registry.is_some() && successful == 0 {
            return Err(DaemonError::SearchFailed { outcomes });
        }
        let songs = outcomes
            .iter()
            .flat_map(|outcome| {
                outcome
                    .candidates
                    .iter()
                    .map(|candidate| candidate.song.clone())
            })
            .take(spec.limit)
            .collect::<Vec<_>>();
        let failures = outcomes
            .iter()
            .filter_map(|outcome| {
                outcome.failure.as_ref().map(|failure| SourceFailure {
                    source: outcome.provider.clone(),
                    error: failure.message.clone(),
                })
            })
            .collect::<Vec<_>>();
        Ok(CatalogSearchResult {
            outcomes,
            songs,
            failures,
        })
    }

    pub async fn play(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        end_behavior: EndBehavior,
    ) -> Result<StartReceipt, DaemonError> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = Uuid::new_v4();
        if let Some(registry) = self.registry.as_ref() {
            let provider = registry
                .require_enabled(&song_key.source)
                .map_err(DaemonError::Failure)?;
            if provider.as_str() != song_key.source {
                return Err(DaemonError::Failure(
                    Failure::new("invalid_request", "song key provider is not canonical")
                        .with_provider(song_key.source.clone()),
                ));
            }
        }
        let adapter = self
            .catalog
            .get(&song_key.source)
            .ok_or_else(|| DaemonError::UnknownSource(song_key.source.clone()))?;
        let stream = timeout(
            self.source_timeout,
            adapter.resolve(&song_key, resolver_locator.as_ref()),
        )
        .await
        .map_err(|_| DaemonError::Catalog(CatalogError::TimedOut(song_key.source.clone())))??;

        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(DaemonError::Engine(EngineError::Rejected(
                "play request was superseded by a newer request".to_owned(),
            )));
        }

        self.engine
            .command(EngineCommand::Start {
                session_id,
                generation,
                song_key: song_key.clone(),
                stream,
                end_behavior,
            })
            .await?;

        self.spawn_stream_retry(session_id, generation, song_key, resolver_locator);

        Ok(StartReceipt {
            session_id,
            generation,
        })
    }

    pub async fn pause(&self, session: SessionRef) -> Result<(), DaemonError> {
        self.engine
            .command(EngineCommand::Pause { session })
            .await?;
        Ok(())
    }

    pub async fn resume(&self, session: SessionRef) -> Result<(), DaemonError> {
        self.engine
            .command(EngineCommand::Resume { session })
            .await?;
        Ok(())
    }

    pub async fn stop(&self, session: SessionRef) -> Result<(), DaemonError> {
        self.engine.command(EngineCommand::Stop { session }).await?;
        Ok(())
    }

    pub async fn seek(
        &self,
        session: SessionRef,
        position_seconds: f64,
    ) -> Result<(), DaemonError> {
        if !position_seconds.is_finite() || position_seconds < 0.0 {
            return Err(DaemonError::InvalidRequest(
                "positionSeconds must be a finite non-negative number".to_owned(),
            ));
        }
        self.engine
            .command(EngineCommand::Seek {
                session,
                position_seconds,
            })
            .await?;
        Ok(())
    }

    pub async fn set_volume(&self, volume: u8) -> Result<(), DaemonError> {
        if volume > 100 {
            return Err(DaemonError::InvalidRequest(
                "volume must be between 0 and 100".to_owned(),
            ));
        }
        self.engine
            .command(EngineCommand::SetVolume { volume })
            .await?;
        Ok(())
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        let receiver = self.engine.subscribe();
        let mut snapshot = receiver.borrow().clone();
        snapshot.runtime_identity = self.runtime_identity.to_string();
        snapshot
    }

    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }

    pub fn subscribe(&self) -> watch::Receiver<PlaybackSnapshot> {
        self.engine.subscribe()
    }

    pub fn sources(&self) -> Vec<String> {
        self.catalog.sources()
    }

    pub fn providers(&self) -> Option<ProviderRegistry> {
        self.registry.clone()
    }

    pub fn login_coordinator(&self) -> LoginCoordinator {
        self.login.clone()
    }

    pub fn acquire_login(
        &self,
        provider: crate::catalog::ProviderId,
    ) -> Result<(Uuid, crate::catalog::ProviderId), Failure> {
        if let Some(registry) = self.registry.as_ref() {
            registry.require_enabled(provider.as_str())?;
        }
        self.login.acquire(provider)
    }

    pub fn release_login(&self, session_id: Uuid) {
        self.login.release_id(session_id);
    }

    pub fn owns_login(&self, session_id: Uuid, provider: crate::catalog::ProviderId) -> bool {
        self.login.owns(session_id, provider)
    }

    pub async fn validate_credential(
        &self,
        provider: crate::catalog::ProviderId,
        candidate: &ProviderCredential,
    ) -> Result<(), DaemonError> {
        if candidate.provider() != provider.as_str() {
            return Err(DaemonError::Failure(
                Failure::new(
                    "invalid_request",
                    "credential candidate provider does not match requested provider",
                )
                .with_provider(provider.to_string()),
            ));
        }
        let adapter = self
            .catalog
            .get(provider.as_str())
            .ok_or_else(|| DaemonError::UnknownSource(provider.to_string()))?;
        timeout(self.source_timeout, adapter.validate_credential(candidate))
            .await
            .map_err(|_| DaemonError::Catalog(CatalogError::TimedOut(provider.to_string())))??;
        Ok(())
    }

    fn requested_sources(&self, requested: Vec<String>) -> Result<Vec<String>, DaemonError> {
        if let Some(registry) = self.registry.as_ref() {
            for source in &requested {
                if !registry.known(source) {
                    return Err(DaemonError::Failure(
                        Failure::new("unknown_provider", "provider identifier is unknown")
                            .with_provider(source.clone()),
                    ));
                }
            }
            let sources = if requested.is_empty() {
                registry.enabled()
            } else {
                requested
            };
            return Ok(deduplicate(sources));
        }
        Ok(requested_sources(&self.catalog, requested))
    }

    fn spawn_stream_retry(
        &self,
        session_id: Uuid,
        generation: u64,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
    ) {
        let catalog = self.catalog.clone();
        let engine = self.engine.clone();
        let source_timeout = self.source_timeout;
        let latest_generation = self.generation.clone();
        tokio::spawn(async move {
            let mut snapshots = engine.subscribe();
            loop {
                if latest_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                if snapshot.generation > generation
                    || (snapshot.generation == generation
                        && snapshot.session_id.is_some()
                        && snapshot.session_id != Some(session_id))
                {
                    return;
                }
                if snapshot.generation == generation
                    && snapshot.session_id == Some(session_id)
                    && snapshot.state == EngineState::Failed
                {
                    if !matches!(
                        snapshot.last_end_cause,
                        Some(EndCause::StreamRejected | EndCause::DecodeFailure)
                    ) {
                        return;
                    }
                    break;
                }
                if snapshot.generation == generation
                    && snapshot.session_id == Some(session_id)
                    && snapshot.state == EngineState::Stopped
                {
                    return;
                }
                if snapshots.changed().await.is_err() {
                    return;
                }
            }

            let Some(adapter) = catalog.get(&song_key.source) else {
                return;
            };
            let stream = match timeout(
                source_timeout,
                adapter.resolve(&song_key, resolver_locator.as_ref()),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    tracing::warn!(%song_key, %error, "stream retry resolution failed");
                    return;
                }
                Err(_) => {
                    tracing::warn!(%song_key, "stream retry resolution timed out");
                    return;
                }
            };

            if latest_generation.load(Ordering::SeqCst) != generation {
                return;
            }

            let snapshot = engine.subscribe().borrow().clone();
            if snapshot.generation != generation
                || snapshot.session_id != Some(session_id)
                || snapshot.state != EngineState::Failed
                || !matches!(
                    snapshot.last_end_cause,
                    Some(EndCause::StreamRejected | EndCause::DecodeFailure)
                )
            {
                return;
            }

            if let Err(error) = engine
                .command(EngineCommand::RefreshStream {
                    session_id,
                    generation,
                    stream,
                })
                .await
            {
                tracing::warn!(%song_key, %error, "stream retry command failed");
            }
        });
    }
}

fn requested_sources(catalog: &SourceCatalog, requested: Vec<String>) -> Vec<String> {
    if requested.is_empty() {
        return catalog.sources();
    }

    let mut seen = HashSet::new();
    requested
        .into_iter()
        .filter(|source| seen.insert(source.clone()))
        .collect()
}

fn deduplicate(requested: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    requested
        .into_iter()
        .filter(|source| seen.insert(source.clone()))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFailure {
    pub source: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSearchResult {
    pub outcomes: Vec<ProviderSearchOutcome>,
    #[serde(skip)]
    pub songs: Vec<Song>,
    #[serde(skip)]
    pub failures: Vec<SourceFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReceipt {
    pub session_id: Uuid,
    pub generation: u64,
}

impl StartReceipt {
    pub fn session_ref(&self) -> SessionRef {
        SessionRef {
            session_id: self.session_id,
            generation: self.generation,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unknown source: {0}")]
    UnknownSource(String),
    #[error("search failed: no provider completed")]
    SearchFailed {
        outcomes: Vec<ProviderSearchOutcome>,
    },
    #[error("operation failed: {0:?}")]
    Failure(Failure),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::{Notify, watch};
    use url::Url;

    use super::*;
    use crate::catalog::SourceAdapter;
    use crate::domain::{PlaybackSnapshot, StreamSource};

    struct FakeSource {
        source: &'static str,
        songs: Vec<Song>,
        fail_search: bool,
    }

    #[async_trait]
    impl SourceAdapter for FakeSource {
        fn source_code(&self) -> &'static str {
            self.source
        }

        async fn search(&self, _spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            if self.fail_search {
                Err(CatalogError::Transient("offline".to_owned()))
            } else {
                Ok(self.songs.clone())
            }
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.mp3").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    struct RecordingEngine {
        commands: Mutex<Vec<EngineCommand>>,
        snapshot_tx: watch::Sender<PlaybackSnapshot>,
        snapshot: watch::Receiver<PlaybackSnapshot>,
    }

    #[async_trait]
    impl AudioEngine for RecordingEngine {
        async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn subscribe(&self) -> watch::Receiver<PlaybackSnapshot> {
            self.snapshot.clone()
        }
    }

    fn song(source: &str, id: &str) -> Song {
        Song {
            key: SongKey::new(source, id).unwrap(),
            resolver_locator: None,
            title: id.to_owned(),
            artists: vec!["artist".to_owned()],
            album: None,
            duration_ms: Some(180_000),
        }
    }

    fn daemon(adapters: Vec<Arc<dyn SourceAdapter>>) -> (PlayerDaemon, Arc<RecordingEngine>) {
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
        });
        (
            PlayerDaemon::new(
                SourceCatalog::new(adapters),
                engine.clone(),
                Duration::from_secs(1),
            ),
            engine,
        )
    }

    #[test]
    fn snapshot_exposes_a_nonempty_process_runtime_identity() {
        let (daemon, _) = daemon(Vec::new());

        let first = daemon.snapshot().runtime_identity;
        let second = daemon.snapshot().runtime_identity;

        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn search_keeps_successful_sources_and_reports_failed_sources() {
        let (daemon, _) = daemon(vec![
            Arc::new(FakeSource {
                source: "qq",
                songs: vec![song("qq", "q1"), song("qq", "q2")],
                fail_search: false,
            }),
            Arc::new(FakeSource {
                source: "wy",
                songs: Vec::new(),
                fail_search: true,
            }),
        ]);

        let response = daemon
            .search(SearchSpec {
                keyword: "song".to_owned(),
                sources: vec!["qq".to_owned(), "wy".to_owned()],
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(response.songs, vec![song("qq", "q1"), song("qq", "q2")]);
        assert_eq!(response.failures.len(), 1);
        assert_eq!(response.failures[0].source, "wy");
    }

    #[tokio::test]
    async fn play_resolves_media_just_in_time_and_dispatches_generation() {
        let (daemon, engine) = daemon(vec![Arc::new(FakeSource {
            source: "qq",
            songs: Vec::new(),
            fail_search: false,
        })]);

        let receipt = daemon
            .play(
                SongKey::new("qq", "song-1").unwrap(),
                None,
                EndBehavior::NotifyController,
            )
            .await
            .unwrap();

        assert_eq!(receipt.generation, 1);
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EngineCommand::Start { generation: 1, song_key, .. }]
                if song_key == &SongKey::new("qq", "song-1").unwrap()
        ));
    }

    struct RetrySource {
        resolve_count: AtomicUsize,
        seen_locators: Mutex<Vec<Option<ResolverLocator>>>,
    }

    #[async_trait]
    impl SourceAdapter for RetrySource {
        fn source_code(&self) -> &'static str {
            "tx"
        }

        async fn search(&self, _spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            self.seen_locators.lock().unwrap().push(locator.cloned());
            let attempt = self.resolve_count.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(StreamSource {
                url: Url::parse(&format!("https://example.test/audio-{attempt}.mp3")).unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    #[tokio::test]
    async fn failed_remote_stream_is_resolved_again_for_the_same_session() {
        let source = Arc::new(RetrySource {
            resolve_count: AtomicUsize::new(0),
            seen_locators: Mutex::new(Vec::new()),
        });
        let (daemon, engine) = daemon(vec![source.clone()]);
        let song_key = SongKey::new("tx", "song-1").unwrap();
        let resolver_locator = ResolverLocator::new("test-v1:persisted").unwrap();
        let receipt = daemon
            .play(
                song_key.clone(),
                Some(resolver_locator.clone()),
                EndBehavior::NotifyController,
            )
            .await
            .unwrap();

        engine.snapshot_tx.send_replace(PlaybackSnapshot {
            generation: receipt.generation,
            session_id: Some(receipt.session_id),
            state: crate::domain::EngineState::Failed,
            song_key: Some(song_key),
            end_behavior: Some(EndBehavior::NotifyController),
            position_seconds: Some(31.5),
            duration_seconds: Some(240.0),
            last_end_cause: Some(crate::domain::EndCause::DecodeFailure),
            ..PlaybackSnapshot::default()
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if engine.commands.lock().unwrap().len() >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(source.resolve_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            *source.seen_locators.lock().unwrap(),
            vec![Some(resolver_locator.clone()), Some(resolver_locator)]
        );
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [
                EngineCommand::Start { generation, session_id, .. },
                EngineCommand::RefreshStream {
                    generation: retry_generation,
                    session_id: retry_session_id,
                    stream,
                }
            ] if *generation == receipt.generation
                && *retry_generation == receipt.generation
                && *session_id == receipt.session_id
                && *retry_session_id == receipt.session_id
                && stream.url.as_str() == "https://example.test/audio-2.mp3"
        ));
    }

    struct OrderedSource {
        old_started: Notify,
        release_old: Notify,
    }

    #[async_trait]
    impl SourceAdapter for OrderedSource {
        fn source_code(&self) -> &'static str {
            "tx"
        }

        async fn search(&self, _spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            if key.id == "old" {
                self.old_started.notify_one();
                self.release_old.notified().await;
            }
            Ok(StreamSource {
                url: Url::parse(&format!("https://example.test/{}.mp3", key.id)).unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    struct FencedEngine {
        generation: AtomicU64,
        snapshot_tx: watch::Sender<PlaybackSnapshot>,
        snapshot: watch::Receiver<PlaybackSnapshot>,
    }

    #[async_trait]
    impl AudioEngine for FencedEngine {
        async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
            if let EngineCommand::Start {
                session_id,
                generation,
                song_key,
                end_behavior,
                ..
            } = command
            {
                let current = self.generation.load(Ordering::SeqCst);
                if generation < current {
                    return Err(EngineError::Rejected("stale generation".to_owned()));
                }
                self.generation.store(generation, Ordering::SeqCst);
                self.snapshot_tx.send_replace(PlaybackSnapshot {
                    generation,
                    session_id: Some(session_id),
                    state: crate::domain::EngineState::Playing,
                    song_key: Some(song_key),
                    end_behavior: Some(end_behavior),
                    ..PlaybackSnapshot::default()
                });
            }
            Ok(())
        }

        fn subscribe(&self) -> watch::Receiver<PlaybackSnapshot> {
            self.snapshot.clone()
        }
    }

    #[tokio::test]
    async fn slow_older_resolution_cannot_replace_a_newer_session() {
        let source = Arc::new(OrderedSource {
            old_started: Notify::new(),
            release_old: Notify::new(),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(FencedEngine {
            generation: AtomicU64::new(0),
            snapshot_tx,
            snapshot,
        });
        let daemon = PlayerDaemon::new(
            SourceCatalog::new([source.clone() as Arc<dyn SourceAdapter>]),
            engine.clone(),
            Duration::from_secs(1),
        );

        let old_daemon = daemon.clone();
        let old = tokio::spawn(async move {
            old_daemon
                .play(
                    SongKey::new("tx", "old").unwrap(),
                    None,
                    EndBehavior::NotifyController,
                )
                .await
        });
        source.old_started.notified().await;

        let newer = daemon
            .play(
                SongKey::new("tx", "new").unwrap(),
                None,
                EndBehavior::NotifyController,
            )
            .await
            .unwrap();
        source.release_old.notify_one();
        let older_result = old.await.unwrap();

        assert_eq!(newer.generation, 2);
        assert!(matches!(
            older_result,
            Err(DaemonError::Engine(EngineError::Rejected(_)))
        ));
        assert_eq!(engine.snapshot.borrow().generation, 2);
        assert_eq!(
            engine.snapshot.borrow().song_key,
            Some(SongKey::new("tx", "new").unwrap())
        );
    }

    struct NewerFailureSource {
        old_started: Notify,
        release_old: Notify,
    }

    #[async_trait]
    impl SourceAdapter for NewerFailureSource {
        fn source_code(&self) -> &'static str {
            "tx"
        }

        async fn search(&self, _spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            if key.id == "old" {
                self.old_started.notify_one();
                self.release_old.notified().await;
            } else {
                return Err(CatalogError::Transient(
                    "newer resolution failed".to_owned(),
                ));
            }
            Ok(StreamSource {
                url: Url::parse("https://example.test/old.mp3").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    #[tokio::test]
    async fn failed_newer_play_request_supersedes_an_older_pending_resolution() {
        let source = Arc::new(NewerFailureSource {
            old_started: Notify::new(),
            release_old: Notify::new(),
        });
        let (daemon, engine) = daemon(vec![source.clone()]);

        let older_daemon = daemon.clone();
        let older = tokio::spawn(async move {
            older_daemon
                .play(
                    SongKey::new("tx", "old").unwrap(),
                    None,
                    EndBehavior::NotifyController,
                )
                .await
        });
        source.old_started.notified().await;

        let newer = daemon
            .play(
                SongKey::new("tx", "new").unwrap(),
                None,
                EndBehavior::NotifyController,
            )
            .await;
        assert!(matches!(
            newer,
            Err(DaemonError::Catalog(CatalogError::Transient(_)))
        ));

        source.release_old.notify_one();
        let older = older.await.unwrap();

        assert!(matches!(
            older,
            Err(DaemonError::Engine(EngineError::Rejected(message)))
                if message.contains("superseded")
        ));
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    struct DelayedRetrySource {
        old_resolve_count: AtomicUsize,
        retry_started: Notify,
        release_retry: Notify,
    }

    #[async_trait]
    impl SourceAdapter for DelayedRetrySource {
        fn source_code(&self) -> &'static str {
            "tx"
        }

        async fn search(&self, _spec: &SearchSpec) -> Result<Vec<Song>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            if key.id == "old" {
                let attempt = self.old_resolve_count.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt == 2 {
                    self.retry_started.notify_one();
                    self.release_retry.notified().await;
                }
                return Ok(StreamSource {
                    url: Url::parse(&format!("https://example.test/old-{attempt}.mp3")).unwrap(),
                    headers: BTreeMap::new(),
                    expires_at_epoch_ms: None,
                });
            }
            Ok(StreamSource {
                url: Url::parse("https://example.test/new.mp3").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    #[tokio::test]
    async fn newer_play_request_prevents_an_inflight_old_stream_retry() {
        let source = Arc::new(DelayedRetrySource {
            old_resolve_count: AtomicUsize::new(0),
            retry_started: Notify::new(),
            release_retry: Notify::new(),
        });
        let (daemon, engine) = daemon(vec![source.clone()]);
        let old_song = SongKey::new("tx", "old").unwrap();
        let old = daemon
            .play(old_song.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();
        engine.snapshot_tx.send_replace(PlaybackSnapshot {
            generation: old.generation,
            session_id: Some(old.session_id),
            state: EngineState::Failed,
            song_key: Some(old_song),
            end_behavior: Some(EndBehavior::NotifyController),
            last_end_cause: Some(EndCause::DecodeFailure),
            ..PlaybackSnapshot::default()
        });
        source.retry_started.notified().await;

        daemon
            .play(
                SongKey::new("tx", "new").unwrap(),
                None,
                EndBehavior::NotifyController,
            )
            .await
            .unwrap();
        source.release_retry.notify_one();
        tokio::time::sleep(Duration::from_millis(75)).await;

        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [
                EngineCommand::Start { generation: 1, .. },
                EngineCommand::Start { generation: 2, .. }
            ]
        ));
    }
}
