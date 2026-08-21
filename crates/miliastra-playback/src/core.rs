use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;
use uuid::Uuid;

use crate::catalog::{
    CatalogError, Failure, PlaybackEligibility, ProviderRegistry, ProviderSearchCandidate,
    ProviderSearchOutcome, SourceCatalog,
};
use crate::credentials::ProviderCredential;
use crate::domain::{
    EndBehavior, EndCause, EngineState, PlaybackSnapshot, ResolverLocator, SearchSpec, SessionRef,
    Song, SongKey, StreamSource,
};
use crate::engine::{AudioEngine, EngineCommand, EngineError};
use crate::login::LoginCoordinator;
use crate::lyrics::TimedLyrics;
use crate::model::TrackMetadata;

/// resolve 结果缓存最大条目数；超出后淘汰最旧条目。
const RESOLVE_CACHE_MAX_ENTRIES: usize = 8;
/// 音源 URL 未携带过期时间时的兜底缓存窗口。
const RESOLVE_CACHE_FALLBACK_TTL_MS: u64 = 10 * 60 * 1000;
/// 搜索候选可用性探测的最大并发数。
const PROBE_CONCURRENCY: usize = 5;

fn unix_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 对搜索候选确定可用性：
/// - 已知 VIP 的账号直接放行 VIP 歌曲；已知非 VIP 时额外探测一次，允许刚开通
///   会员的用户立刻点播；
/// - 只有 VIP 无法判定（未登录/未知/查询失败）且标注需要确认时才探测；
/// - 免费可播/无版权/付费/不可播等标注与账号 VIP 无关，直接采用；
/// - 探测确认可播放 → Eligible；平台明确 VIP/无版权 → 对应标注；
/// - 探测确认为不可用（Unavailable）且无明确初始原因 → Ineligible；
/// - 认证/限流/超时等无法确认的场景 → 保留初始标注，不猜测。
async fn probe_candidates(
    adapter: Arc<dyn crate::catalog::SourceAdapter>,
    candidates: Vec<ProviderSearchCandidate>,
    request_timeout: Duration,
) -> Vec<ProviderSearchCandidate> {
    // 免费和未知候选不依赖账号 VIP 状态。仅在候选明确标为 VIP 时才读取
    // 账号缓存，避免每次搜索都额外调用平台账号接口。
    let has_vip_candidate = candidates
        .iter()
        .any(|candidate| candidate.eligibility == PlaybackEligibility::VipRequired);
    let account_vip = if has_vip_candidate {
        match timeout(request_timeout, adapter.account_status()).await {
            Ok(Ok(Some(status))) if status.logged_in && status.vip_known => Some(status.vip),
            _ => None,
        }
    } else {
        None
    };
    let mut probes = Vec::with_capacity(candidates.len());
    for candidate in candidates.into_iter().take(PROBE_CONCURRENCY) {
        let initial = candidate.eligibility;
        let needs_probe = match initial {
            // 明确标注 VIP 的歌曲：只有已确认 VIP 时才能直接放行。缓存的非 VIP
            // 仍需探测一次，避免用户刚开通会员时被一天的旧负缓存挡住。
            PlaybackEligibility::VipRequired => account_vip != Some(true),
            // 元数据未给出判定：探测确认。
            PlaybackEligibility::Unknown => true,
            // 免费可播/无版权/付费/不可播与账号 VIP 无关，直接采用。
            _ => false,
        };
        if !needs_probe {
            let eligibility = match (initial, account_vip) {
                // 账号有 VIP → VIP 歌曲直接放行；无 VIP → 屏蔽。
                (PlaybackEligibility::VipRequired, Some(true)) => PlaybackEligibility::Eligible,
                (PlaybackEligibility::VipRequired, Some(false)) => PlaybackEligibility::VipRequired,
                (other, _) => other,
            };
            probes.push(tokio::spawn(async move { (candidate, eligibility) }));
            continue;
        }
        let adapter = adapter.clone();
        let key = candidate.song.key.clone();
        let locator = candidate.song.resolver_locator.clone();
        probes.push(tokio::spawn(async move {
            let outcome = timeout(
                request_timeout,
                adapter.probe_eligibility(&key, locator.as_ref()),
            )
            .await;
            let eligibility = match outcome {
                // 探测确认可播放（resolve 用账号凭据，VIP 账号能解析 VIP 歌）→ 直接可用。
                Ok(Ok(confirmed)) => match confirmed {
                    PlaybackEligibility::Eligible => PlaybackEligibility::Eligible,
                    other => other,
                },
                Ok(Err(error)) => {
                    // 已缓存为非 VIP 时，VIP 拒绝正好验证了现有结论，不能清空
                    // 一天的乐观缓存后让下一次搜索重新打账号接口。
                    if !matches!(&error, CatalogError::VipRequired(_)) || account_vip != Some(false)
                    {
                        adapter.observe_playback_failure(&error);
                    }
                    match error {
                        CatalogError::VipRequired(_) => PlaybackEligibility::VipRequired,
                        CatalogError::NoCopyright(_) => match initial {
                            // 平台元数据明确标 VIP 的歌曲，探测拿不到完整流多为 VIP 限制。
                            PlaybackEligibility::VipRequired => PlaybackEligibility::VipRequired,
                            _ => PlaybackEligibility::NoCopyright,
                        },
                        CatalogError::Unavailable(_) => match initial {
                            PlaybackEligibility::VipRequired => PlaybackEligibility::VipRequired,
                            PlaybackEligibility::NoCopyright => PlaybackEligibility::NoCopyright,
                            _ => PlaybackEligibility::Ineligible,
                        },
                        _ => initial,
                    }
                }
                Err(_) => initial,
            };
            (candidate, eligibility)
        }));
    }
    let mut confirmed = Vec::with_capacity(probes.len());
    for probe in probes {
        match probe.await {
            Ok((candidate, eligibility)) => confirmed.push(ProviderSearchCandidate {
                song: candidate.song,
                eligibility,
            }),
            Err(error) => log::warn!("搜索候选探测任务失败: {error}"),
        }
    }
    confirmed
}

#[derive(Clone)]
struct ResolveCacheEntry {
    key: SongKey,
    stream: StreamSource,
    resolved_at_epoch_ms: u64,
}

impl ResolveCacheEntry {
    fn is_valid(&self, now_epoch_ms: u64) -> bool {
        if let Some(expires_at) = self.stream.expires_at_epoch_ms {
            // 签名 URL：以平台给出的过期时间为准。
            expires_at > now_epoch_ms
        } else {
            // 无过期字段：按兜底窗口保守缓存，避免 CDN URL 长期失效。
            now_epoch_ms.saturating_sub(self.resolved_at_epoch_ms) < RESOLVE_CACHE_FALLBACK_TTL_MS
        }
    }
}

#[derive(Default)]
struct ResolveCache {
    entries: Vec<ResolveCacheEntry>,
}

impl ResolveCache {
    fn get(&mut self, key: &SongKey, now_epoch_ms: u64) -> Option<StreamSource> {
        self.entries.retain(|entry| entry.is_valid(now_epoch_ms));
        let position = self.entries.iter().position(|entry| entry.key == *key)?;
        // 命中提升到末尾（近似 LRU）。
        let entry = self.entries.remove(position);
        self.entries.push(entry.clone());
        Some(entry.stream)
    }

    fn put(&mut self, key: SongKey, stream: StreamSource, now_epoch_ms: u64) {
        self.entries.retain(|entry| entry.key != key);
        self.entries.push(ResolveCacheEntry {
            key,
            stream,
            resolved_at_epoch_ms: now_epoch_ms,
        });
        while self.entries.len() > RESOLVE_CACHE_MAX_ENTRIES {
            self.entries.remove(0);
        }
    }
}

#[derive(Clone)]
pub struct PlaybackCore {
    catalog: SourceCatalog,
    registry: Option<ProviderRegistry>,
    engine: Arc<dyn AudioEngine>,
    source_timeout: Duration,
    generation: Arc<AtomicU64>,
    runtime_identity: Arc<str>,
    login: LoginCoordinator,
    resolve_cache: Arc<Mutex<ResolveCache>>,
    pub(crate) audio_cache: Option<crate::cache::AudioCache>,
}

impl PlaybackCore {
    #[cfg(test)]
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
            resolve_cache: Arc::new(Mutex::new(ResolveCache::default())),
            audio_cache: None,
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
            resolve_cache: Arc::new(Mutex::new(ResolveCache::default())),
            audio_cache: None,
        }
    }

    /// 启用音频数据缓存（本地代理）：播放音源 URL 将改写为本机地址。
    pub fn with_audio_cache(mut self, cache: crate::cache::AudioCache) -> Self {
        self.audio_cache = Some(cache);
        self
    }

    /// 删除曲目的音频缓存（播放解码失败后自愈：下次播放重新下载）。
    pub async fn invalidate_audio_cache(&self, key: &SongKey) -> bool {
        match &self.audio_cache {
            Some(cache) => cache.invalidate(key).await,
            None => false,
        }
    }

    /// 把音源改写为本地代理地址（若启用了音频缓存）。
    /// 曲目元数据（若有）随 rewrite 一并注册到缓存条目。
    async fn route_stream(
        &self,
        song_key: &SongKey,
        metadata: Option<&TrackMetadata>,
        stream: StreamSource,
    ) -> StreamSource {
        match &self.audio_cache {
            Some(cache) => cache.rewrite(song_key, metadata, stream).await,
            None => stream,
        }
    }

    pub async fn search(&self, spec: SearchSpec) -> Result<CatalogSearchResult, PlaybackCoreError> {
        let keyword = spec.keyword.trim();
        if keyword.is_empty() {
            return Err(PlaybackCoreError::InvalidRequest(
                "keyword must not be empty".to_owned(),
            ));
        }
        if spec.limit == 0 || spec.limit > 50 {
            return Err(PlaybackCoreError::InvalidRequest(
                "limit must be between 1 and 50".to_owned(),
            ));
        }

        let sources = self.requested_sources(spec.sources)?;
        let requests = sources.iter().cloned().map(|source| {
            let request_source = source.clone();
            let catalog = self.catalog.clone();
            let registry = self.registry;
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
                let candidates = timeout(request_timeout, adapter.search(&source_spec))
                    .await
                    .map_err(|_| {
                        CatalogError::TimedOut(request_source.clone())
                            .as_failure(Some(&request_source))
                    })?
                    .map_err(|error| error.as_failure(Some(&request_source)))?;
                let candidates = probe_candidates(adapter, candidates, request_timeout).await;
                Ok(candidates)
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
            return Err(PlaybackCoreError::SearchFailed { outcomes });
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

    #[cfg(test)]
    pub async fn play(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        end_behavior: EndBehavior,
    ) -> Result<StartReceipt, PlaybackCoreError> {
        self.play_inner(
            song_key,
            resolver_locator,
            None,
            end_behavior,
            None,
            true,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn play_cancellable(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        metadata: Option<&TrackMetadata>,
        end_behavior: EndBehavior,
        cancellation: oneshot::Receiver<()>,
        requested: bool,
        seek_seconds: Option<f64>,
    ) -> Result<StartReceipt, PlaybackCoreError> {
        self.play_inner(
            song_key,
            resolver_locator,
            metadata,
            end_behavior,
            Some(cancellation),
            requested,
            seek_seconds,
        )
        .await
    }

    /// 预加载音源：提前完成网络解析并写入缓存，`play` 时可跳过等待。
    ///
    /// 启用音频缓存时同时注册条目并主动下载音频数据：下一首播放时
    /// 直接从磁盘服务，避免起播等待源站。曲目元数据（若有）随
    /// rewrite 一并写入缓存条目。只解析不启动引擎；失败时静默返回
    /// 错误（播放路径会重新解析）。
    pub async fn preload(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        metadata: Option<&TrackMetadata>,
    ) -> Result<(), PlaybackCoreError> {
        if self.cached_stream(&song_key).is_some() {
            tracing::debug!(key = %song_key, "音源已在缓存中，跳过预加载");
            return Ok(());
        }
        // 磁盘缓存优先：完整文件已落盘时直接注册本地代理音源。
        if let Some(stream) = self.disk_complete_stream(&song_key, metadata).await {
            tracing::debug!(key = %song_key, "磁盘缓存命中，跳过预加载解析");
            self.cache_stream(song_key.clone(), stream);
            return Ok(());
        }
        let stream = self
            .resolve_stream(&song_key, resolver_locator.as_ref())
            .await?;
        self.cache_stream(song_key.clone(), stream.clone());
        // 音频数据预下载：播放时直接命中本地文件（下载失败静默，播放时走正常下载）。
        if let Some(cache) = &self.audio_cache {
            cache.rewrite(&song_key, metadata, stream).await;
            if cache.prefetch(&song_key).await {
                tracing::debug!(key = %song_key, "音频数据已开始预下载");
            }
        }
        Ok(())
    }

    async fn resolve_stream(
        &self,
        song_key: &SongKey,
        resolver_locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, PlaybackCoreError> {
        if let Some(registry) = self.registry.as_ref() {
            registry
                .require_enabled(&song_key.source)
                .map_err(PlaybackCoreError::Failure)?;
        }
        let adapter = self
            .catalog
            .get(&song_key.source)
            .ok_or_else(|| PlaybackCoreError::UnknownSource(song_key.source.clone()))?;
        match timeout(
            self.source_timeout,
            adapter.resolve(song_key, resolver_locator),
        )
        .await
        {
            Err(_) => Err(PlaybackCoreError::Catalog(CatalogError::TimedOut(
                song_key.source.clone(),
            ))),
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) => {
                let _ = self
                    .catalog
                    .observe_playback_failure(&song_key.source, &error);
                Err(PlaybackCoreError::Catalog(error))
            }
        }
    }

    fn cached_stream(&self, key: &SongKey) -> Option<StreamSource> {
        let mut cache = self
            .resolve_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key, unix_epoch_ms())
    }

    fn cache_stream(&self, key: SongKey, stream: StreamSource) {
        let mut cache = self
            .resolve_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put(key, stream, unix_epoch_ms());
    }

    /// 磁盘完整缓存命中时注册本地代理条目并返回代理音源；未命中返回 None。
    /// 代理直接服务磁盘文件，源 URL 可不存在。
    async fn disk_complete_stream(
        &self,
        song_key: &SongKey,
        metadata: Option<&TrackMetadata>,
    ) -> Option<StreamSource> {
        match &self.audio_cache {
            Some(cache) => cache.proxy_for_complete(song_key, metadata).await,
            None => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn play_inner(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        metadata: Option<&TrackMetadata>,
        end_behavior: EndBehavior,
        mut cancellation: Option<oneshot::Receiver<()>>,
        requested: bool,
        seek_seconds: Option<f64>,
    ) -> Result<StartReceipt, PlaybackCoreError> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = Uuid::new_v4();
        if let Some(registry) = self.registry.as_ref() {
            let provider = registry
                .require_enabled(&song_key.source)
                .map_err(PlaybackCoreError::Failure)?;
            if provider.as_str() != song_key.source {
                return Err(PlaybackCoreError::Failure(
                    Failure::new("invalid_request", "song key provider is not canonical")
                        .with_provider(song_key.source.clone()),
                ));
            }
        }
        let stream = match self.cached_stream(&song_key) {
            Some(cached) => {
                tracing::debug!(key = %song_key, "播放音源缓存命中，跳过网络解析");
                cached
            }
            None => {
                // 磁盘缓存优先：完整文件已落盘时直接注册本地代理并返回代理音源，
                // 断网或源站不可用也能播放。
                if let Some(stream) = self.disk_complete_stream(&song_key, metadata).await {
                    tracing::debug!(key = %song_key, "磁盘缓存命中，跳过网络解析");
                    self.cache_stream(song_key.clone(), stream.clone());
                    stream
                } else {
                    let adapter = self
                        .catalog
                        .get(&song_key.source)
                        .ok_or_else(|| PlaybackCoreError::UnknownSource(song_key.source.clone()))?;
                    let resolved = {
                        let resolve = timeout(
                            self.source_timeout,
                            adapter.resolve(&song_key, resolver_locator.as_ref()),
                        );
                        tokio::pin!(resolve);
                        match cancellation.as_mut() {
                            Some(cancellation) => {
                                tokio::select! {
                                    biased;
                                    _ = cancellation => return Err(PlaybackCoreError::Cancelled),
                                    resolved = &mut resolve => resolved,
                                }
                            }
                            None => resolve.await,
                        }
                    };
                    let stream = match resolved {
                        Err(_) => {
                            return Err(PlaybackCoreError::Catalog(CatalogError::TimedOut(
                                song_key.source.clone(),
                            )));
                        }
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            let _ = self
                                .catalog
                                .observe_playback_failure(&song_key.source, &error);
                            return Err(PlaybackCoreError::Catalog(error));
                        }
                    };
                    self.cache_stream(song_key.clone(), stream.clone());
                    stream
                }
            }
        };
        let stream = self.route_stream(&song_key, metadata, stream).await;

        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(PlaybackCoreError::Engine(EngineError::Rejected(
                "play request was superseded by a newer request".to_owned(),
            )));
        }

        let session = SessionRef {
            session_id,
            generation,
        };
        let start_result = {
            let start = self.engine.command(EngineCommand::Start {
                session_id,
                generation,
                song_key: song_key.clone(),
                stream,
                end_behavior,
                seek_seconds,
            });
            tokio::pin!(start);
            match cancellation.as_mut() {
                Some(cancellation) => {
                    tokio::select! {
                        biased;
                        _ = cancellation => None,
                        result = &mut start => Some(result),
                    }
                }
                None => Some(start.await),
            }
        };
        let Some(start_result) = start_result else {
            if let Err(error) = self.engine.command(EngineCommand::Stop { session }).await {
                tracing::debug!(%error, generation, %session_id, "cancelled playback cleanup was rejected");
            }
            return Err(PlaybackCoreError::Cancelled);
        };
        start_result?;

        self.spawn_playback_statistics(session_id, generation, song_key.clone(), requested);
        self.spawn_stream_retry(
            session_id,
            generation,
            song_key,
            resolver_locator,
            metadata.cloned(),
        );

        Ok(StartReceipt {
            session_id,
            generation,
        })
    }

    pub async fn lyrics(
        &self,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
    ) -> Result<Option<TimedLyrics>, PlaybackCoreError> {
        // 缓存命中直接返回，避免重复请求平台歌词接口。
        if let Some(cache) = &self.audio_cache
            && let Some(lyrics) = cache.get_lyrics(&song_key).await
        {
            return Ok(Some(lyrics));
        }
        if let Some(registry) = self.registry.as_ref() {
            registry
                .require_enabled(&song_key.source)
                .map_err(PlaybackCoreError::Failure)?;
        }
        let adapter = self
            .catalog
            .get(&song_key.source)
            .ok_or_else(|| PlaybackCoreError::UnknownSource(song_key.source.clone()))?;
        let result = timeout(
            self.source_timeout,
            adapter.lyrics(&song_key, resolver_locator.as_ref()),
        )
        .await
        .map_err(|_| PlaybackCoreError::Catalog(CatalogError::TimedOut(song_key.source.clone())))?
        .map_err(PlaybackCoreError::Catalog);
        // 网络成功且命中歌词：写入缓存（无歌词不缓存）。
        if let Ok(Some(lyrics)) = &result
            && let Some(cache) = &self.audio_cache
        {
            cache.put_lyrics(&song_key, lyrics).await;
        }
        result
    }

    /// 跳转到指定播放位置(秒);仅播放/暂停状态可用。
    pub async fn seek(
        &self,
        session: SessionRef,
        position_seconds: f64,
    ) -> Result<(), PlaybackCoreError> {
        if !position_seconds.is_finite() || position_seconds < 0.0 {
            return Err(PlaybackCoreError::Failure(Failure::new(
                "invalid_seek_position",
                "seek 位置必须是有限的非负数",
            )));
        }
        self.engine
            .command(EngineCommand::Seek {
                session,
                position_seconds,
            })
            .await?;
        Ok(())
    }

    pub async fn pause(&self, session: SessionRef) -> Result<(), PlaybackCoreError> {
        self.engine
            .command(EngineCommand::Pause { session })
            .await?;
        Ok(())
    }

    pub async fn resume(&self, session: SessionRef) -> Result<(), PlaybackCoreError> {
        self.engine
            .command(EngineCommand::Resume { session })
            .await?;
        Ok(())
    }

    pub async fn stop(&self, session: SessionRef) -> Result<(), PlaybackCoreError> {
        self.engine.command(EngineCommand::Stop { session }).await?;
        Ok(())
    }

    pub async fn set_volume(&self, volume: u8) -> Result<(), PlaybackCoreError> {
        if volume > 100 {
            return Err(PlaybackCoreError::InvalidRequest(
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

    pub async fn refresh_kugou_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<ProviderCredential, PlaybackCoreError> {
        let account = self
            .catalog
            .kugou_account()
            .ok_or_else(|| PlaybackCoreError::UnknownSource("kugou".to_owned()))?;
        timeout(self.source_timeout, account.refresh_token(credential))
            .await
            .map_err(|_| PlaybackCoreError::Catalog(CatalogError::TimedOut("kugou".to_owned())))?
            .map_err(PlaybackCoreError::Catalog)
    }

    /// 刷新支持手动刷新的平台凭据（QQ 音乐、哔哩哔哩）。
    /// 返回 `None` 表示当前凭据无需刷新。
    pub async fn refresh_credential(
        &self,
        provider: crate::catalog::ProviderId,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, PlaybackCoreError> {
        let adapter = self
            .catalog
            .refresh_adapter(provider.as_str())
            .ok_or_else(|| PlaybackCoreError::UnknownSource(provider.as_str().to_owned()))?;
        timeout(self.source_timeout, adapter.refresh_credential(credential))
            .await
            .map_err(|_| {
                PlaybackCoreError::Catalog(CatalogError::TimedOut(provider.as_str().to_owned()))
            })?
            .map_err(PlaybackCoreError::Catalog)
    }

    pub async fn kugou_account_status(
        &self,
    ) -> Result<crate::catalog::KugouAccountStatus, PlaybackCoreError> {
        let account = self
            .catalog
            .kugou_account()
            .ok_or_else(|| PlaybackCoreError::UnknownSource("kugou".to_owned()))?;
        timeout(self.source_timeout, account.account_status())
            .await
            .map_err(|_| PlaybackCoreError::Catalog(CatalogError::TimedOut("kugou".to_owned())))?
            .map_err(PlaybackCoreError::Catalog)
    }

    /// 查询平台账号状态。普通查询优先使用乐观缓存。
    pub async fn account_status(
        &self,
        provider: crate::catalog::ProviderId,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, PlaybackCoreError> {
        timeout(
            self.source_timeout,
            self.catalog.account_status(provider.as_str()),
        )
        .await
        .map_err(|_| {
            PlaybackCoreError::Catalog(CatalogError::TimedOut(provider.as_str().to_owned()))
        })?
        .map_err(PlaybackCoreError::Catalog)
    }

    /// 跳过缓存并重新查询平台账号状态。
    pub async fn refresh_account_status(
        &self,
        provider: crate::catalog::ProviderId,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, PlaybackCoreError> {
        timeout(
            self.source_timeout,
            self.catalog.refresh_account_status(provider.as_str()),
        )
        .await
        .map_err(|_| {
            PlaybackCoreError::Catalog(CatalogError::TimedOut(provider.as_str().to_owned()))
        })?
        .map_err(PlaybackCoreError::Catalog)
    }

    pub fn invalidate_account_status(
        &self,
        provider: crate::catalog::ProviderId,
    ) -> Result<(), PlaybackCoreError> {
        self.catalog
            .invalidate_account_status(provider.as_str())
            .map_err(PlaybackCoreError::Catalog)
    }

    pub async fn kugou_claim_vip(
        &self,
    ) -> Result<crate::catalog::KugouListenReport, PlaybackCoreError> {
        let account = self
            .catalog
            .kugou_account()
            .ok_or_else(|| PlaybackCoreError::UnknownSource("kugou".to_owned()))?;
        timeout(self.source_timeout, account.claim_vip())
            .await
            .map_err(|_| PlaybackCoreError::Catalog(CatalogError::TimedOut("kugou".to_owned())))?
            .map_err(PlaybackCoreError::Catalog)
    }

    pub async fn kugou_upgrade_vip(
        &self,
    ) -> Result<crate::catalog::KugouListenReport, PlaybackCoreError> {
        let account = self
            .catalog
            .kugou_account()
            .ok_or_else(|| PlaybackCoreError::UnknownSource("kugou".to_owned()))?;
        timeout(self.source_timeout, account.upgrade_vip())
            .await
            .map_err(|_| PlaybackCoreError::Catalog(CatalogError::TimedOut("kugou".to_owned())))?
            .map_err(PlaybackCoreError::Catalog)
    }

    pub async fn validate_credential(
        &self,
        provider: crate::catalog::ProviderId,
        candidate: &ProviderCredential,
    ) -> Result<(), PlaybackCoreError> {
        if candidate.provider() != provider.as_str() {
            return Err(PlaybackCoreError::Failure(
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
            .ok_or_else(|| PlaybackCoreError::UnknownSource(provider.to_string()))?;
        timeout(self.source_timeout, adapter.validate_credential(candidate))
            .await
            .map_err(|_| {
                PlaybackCoreError::Catalog(CatalogError::TimedOut(provider.to_string()))
            })??;
        Ok(())
    }

    fn requested_sources(&self, requested: Vec<String>) -> Result<Vec<String>, PlaybackCoreError> {
        if let Some(registry) = self.registry.as_ref() {
            for source in &requested {
                if !registry.known(source) {
                    return Err(PlaybackCoreError::Failure(
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

    /// 观察真实引擎状态并写统计。首次 Playing 记一次播放；最终 Failed 才记失败。
    /// 流重试的瞬时 Failed 会继续变化，因此延迟一个短窗口后复查当前会话；SQL 围栏
    /// 再保证重复快照、并发观察者只计一次。换代或 session 不符立即退出。
    fn spawn_playback_statistics(
        &self,
        session_id: Uuid,
        generation: u64,
        song_key: SongKey,
        requested: bool,
    ) {
        let Some(cache) = self.audio_cache.clone() else {
            return;
        };
        let engine = self.engine.clone();
        let latest_generation = self.generation.clone();
        tokio::spawn(async move {
            let mut snapshots = engine.subscribe();
            loop {
                if !generation_is_latest(&latest_generation, generation) {
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
                if snapshot.generation == generation && snapshot.session_id == Some(session_id) {
                    match snapshot.state {
                        EngineState::Playing => {
                            cache
                                .record_play(&song_key, session_id, generation, requested)
                                .await;
                        }
                        EngineState::Failed => {
                            tokio::time::sleep(Duration::from_millis(750)).await;
                            let current = engine.subscribe().borrow().clone();
                            if generation_is_latest(&latest_generation, generation)
                                && current.generation == generation
                                && current.session_id == Some(session_id)
                                && current.state == EngineState::Failed
                            {
                                let code = current
                                    .failure
                                    .as_ref()
                                    .map(|failure| failure.code.as_str())
                                    .unwrap_or_else(|| {
                                        end_cause_failure_code(current.last_end_cause)
                                    });
                                cache
                                    .record_failure(&song_key, session_id, generation, code)
                                    .await;
                                return;
                            }
                        }
                        EngineState::Stopped => return,
                        _ => {}
                    }
                }
                if snapshots.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// 流类失败（StreamRejected/DecodeFailure）后自动重试：
    /// 第一轮重新解析音源并走缓存代理（缓存优先）；
    /// 第一轮刷新后仍失败，第二轮清除缓存并直连源站（跳过缓存）。
    /// 曲目元数据（若有）随两轮 rewrite/写回一并注册到缓存条目。
    fn spawn_stream_retry(
        &self,
        session_id: Uuid,
        generation: u64,
        song_key: SongKey,
        resolver_locator: Option<ResolverLocator>,
        metadata: Option<TrackMetadata>,
    ) {
        let catalog = self.catalog.clone();
        let engine = self.engine.clone();
        let source_timeout = self.source_timeout;
        let latest_generation = self.generation.clone();
        let audio_cache = self.audio_cache.clone();
        tokio::spawn(async move {
            let mut snapshots = engine.subscribe();
            // 第一轮：缓存优先。
            if !wait_for_stream_failure(&mut snapshots, &latest_generation, generation, session_id)
                .await
            {
                return;
            }
            let Some(stream) = resolve_stream_retry(
                &catalog,
                &song_key,
                resolver_locator.as_ref(),
                source_timeout,
            )
            .await
            else {
                return;
            };
            if !generation_is_latest(&latest_generation, generation)
                || !snapshot_is_stream_failure(&snapshots.borrow(), generation, session_id)
            {
                return;
            }
            // 第一轮重试音源同样改写为本地代理地址，保证与初始播放走同一缓存条目。
            let stream = match &audio_cache {
                Some(cache) => cache.rewrite(&song_key, metadata.as_ref(), stream).await,
                None => stream,
            };
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
            // 第二轮：跳过缓存——第一轮刷新后仍失败，清除缓存并直连源站。
            if !wait_for_stream_failure_after_refresh(
                &mut snapshots,
                &latest_generation,
                generation,
                session_id,
            )
            .await
            {
                return;
            }
            if let Some(cache) = &audio_cache {
                cache.invalidate(&song_key).await;
            }
            let Some(stream) = resolve_stream_retry(
                &catalog,
                &song_key,
                resolver_locator.as_ref(),
                source_timeout,
            )
            .await
            else {
                return;
            };
            if !generation_is_latest(&latest_generation, generation)
                || !snapshot_is_stream_failure(&snapshots.borrow(), generation, session_id)
            {
                return;
            }
            // 直连源站刷新流地址。
            if let Err(error) = engine
                .command(EngineCommand::RefreshStream {
                    session_id,
                    generation,
                    stream: stream.clone(),
                })
                .await
            {
                tracing::warn!(%song_key, %error, "stream retry command failed");
            }
            // 直连播放成功后，把本次音源写回缓存（后台预热：下次播放直接命中）。
            if let Some(cache) = audio_cache {
                spawn_cache_prefetch(
                    cache,
                    engine,
                    latest_generation,
                    song_key,
                    stream,
                    metadata,
                    generation,
                    session_id,
                );
            }
        });
    }
}

/// 直连播放成功后把音源写回缓存：观察会话进入 Playing → rewrite 注册 → 主动下载。
/// 播放失败/会话结束/超时则放弃（下次播放走正常缓存路径重新下载）。
#[allow(clippy::too_many_arguments)]
fn spawn_cache_prefetch(
    cache: crate::cache::AudioCache,
    engine: Arc<dyn AudioEngine>,
    latest_generation: Arc<AtomicU64>,
    song_key: SongKey,
    stream: StreamSource,
    metadata: Option<TrackMetadata>,
    generation: u64,
    session_id: Uuid,
) {
    tokio::spawn(async move {
        let mut snapshots = engine.subscribe();
        if !wait_for_playing_after_refresh(
            &mut snapshots,
            &latest_generation,
            generation,
            session_id,
        )
        .await
        {
            return;
        }
        cache.rewrite(&song_key, metadata.as_ref(), stream).await;
        if cache.prefetch(&song_key).await {
            tracing::info!("直连播放成功后已写回音频缓存: {song_key}");
        }
    });
}

/// RefreshStream 生效后等待当前会话进入 Playing（直连播放成功）。
/// 先消费刷新前的旧失败快照，避免把刷新前的状态当作本轮结果。
async fn wait_for_playing_after_refresh(
    snapshots: &mut watch::Receiver<PlaybackSnapshot>,
    latest_generation: &AtomicU64,
    generation: u64,
    session_id: Uuid,
) -> bool {
    if snapshots.changed().await.is_err() {
        return false;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if latest_generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            let snapshot = snapshots.borrow_and_update().clone();
            if snapshot.generation > generation
                || (snapshot.generation == generation
                    && snapshot.session_id.is_some()
                    && snapshot.session_id != Some(session_id))
            {
                return false;
            }
            if snapshot.generation == generation && snapshot.session_id == Some(session_id) {
                match snapshot.state {
                    EngineState::Playing => return true,
                    EngineState::Failed | EngineState::Stopped => return false,
                    _ => {}
                }
            }
            if snapshots.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// 等待当前会话出现流类失败（StreamRejected/DecodeFailure）。
/// 返回 true 表示可开始重试；会话结束/换代/其他失败 → false。
async fn wait_for_stream_failure(
    snapshots: &mut watch::Receiver<PlaybackSnapshot>,
    latest_generation: &AtomicU64,
    generation: u64,
    session_id: Uuid,
) -> bool {
    loop {
        if latest_generation.load(Ordering::SeqCst) != generation {
            return false;
        }
        let snapshot = snapshots.borrow_and_update().clone();
        if snapshot.generation > generation
            || (snapshot.generation == generation
                && snapshot.session_id.is_some()
                && snapshot.session_id != Some(session_id))
        {
            return false;
        }
        if snapshot.generation == generation
            && snapshot.session_id == Some(session_id)
            && snapshot.state == EngineState::Failed
        {
            return matches!(
                snapshot.last_end_cause,
                Some(EndCause::StreamRejected | EndCause::DecodeFailure)
            );
        }
        if snapshot.generation == generation
            && snapshot.session_id == Some(session_id)
            && snapshot.state == EngineState::Stopped
        {
            return false;
        }
        if snapshots.changed().await.is_err() {
            return false;
        }
    }
}

/// RefreshStream 生效后的流失败观察：先消费刷新前的旧失败快照，
/// 等待刷新产生的新状态变化后再判断，避免把刷新前的失败当作本轮结果。
async fn wait_for_stream_failure_after_refresh(
    snapshots: &mut watch::Receiver<PlaybackSnapshot>,
    latest_generation: &AtomicU64,
    generation: u64,
    session_id: Uuid,
) -> bool {
    if snapshots.changed().await.is_err() {
        return false;
    }
    wait_for_stream_failure(snapshots, latest_generation, generation, session_id).await
}

/// 重新解析音源（流重试用）；失败或超时返回 None。
async fn resolve_stream_retry(
    catalog: &SourceCatalog,
    song_key: &SongKey,
    resolver_locator: Option<&ResolverLocator>,
    source_timeout: Duration,
) -> Option<StreamSource> {
    let adapter = catalog.get(&song_key.source)?;
    match timeout(source_timeout, adapter.resolve(song_key, resolver_locator)).await {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(error)) => {
            let _ = catalog.observe_playback_failure(&song_key.source, &error);
            tracing::warn!(%song_key, %error, "stream retry resolution failed");
            None
        }
        Err(_) => {
            tracing::warn!(%song_key, "stream retry resolution timed out");
            None
        }
    }
}

fn generation_is_latest(latest_generation: &AtomicU64, generation: u64) -> bool {
    latest_generation.load(Ordering::SeqCst) == generation
}

fn end_cause_failure_code(cause: Option<EndCause>) -> &'static str {
    match cause {
        Some(EndCause::StreamRejected) => "stream_rejected",
        Some(EndCause::DecodeFailure) => "decode_failure",
        Some(EndCause::RecoveryPositionUnknown) => "recovery_position_unknown",
        Some(EndCause::EngineExited) => "engine_exited",
        _ => "playback_failed",
    }
}

/// 当前快照是否仍是「本会话的流类失败」状态（RefreshStream 前检查用）。
fn snapshot_is_stream_failure(
    snapshot: &PlaybackSnapshot,
    generation: u64,
    session_id: Uuid,
) -> bool {
    snapshot.generation == generation
        && snapshot.session_id == Some(session_id)
        && snapshot.state == EngineState::Failed
        && matches!(
            snapshot.last_end_cause,
            Some(EndCause::StreamRejected | EndCause::DecodeFailure)
        )
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
pub(crate) enum PlaybackCoreError {
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
    #[error("playback operation was cancelled")]
    Cancelled,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{Notify, watch};
    use url::Url;

    use super::*;
    use crate::catalog::{
        PlaybackEligibility, ProviderAccountStatus, ProviderSearchCandidate, SourceAdapter,
    };
    use crate::domain::{PlaybackSnapshot, StreamSource};

    struct FakeSource {
        songs: Vec<Song>,
        fail_search: bool,
    }

    /// 记录 resolve 调用次数的假音源，用于缓存/预加载测试。
    struct CountingSource {
        resolve_calls: Arc<AtomicUsize>,
        expires_at_epoch_ms: Option<u64>,
    }

    #[async_trait]
    impl SourceAdapter for CountingSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            Ok(StreamSource {
                url: Url::parse("https://example.test/audio.mp3").unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: self.expires_at_epoch_ms,
            })
        }
    }

    fn counting_source(
        expires_at_epoch_ms: Option<u64>,
    ) -> (Arc<CountingSource>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource {
            resolve_calls: calls.clone(),
            expires_at_epoch_ms,
        });
        (source, calls)
    }

    #[async_trait]
    impl SourceAdapter for FakeSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            if self.fail_search {
                Err(CatalogError::Transient("offline".to_owned()))
            } else {
                Ok(self
                    .songs
                    .iter()
                    .cloned()
                    .map(|song| ProviderSearchCandidate {
                        song,
                        eligibility: PlaybackEligibility::Unknown,
                    })
                    .collect())
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
        /// RefreshStream 处理后发布流类失败（模拟第一轮刷新后仍失败，触发第二轮直连重试）。
        refresh_fails_after: bool,
    }

    #[async_trait]
    impl AudioEngine for RecordingEngine {
        async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
            if let EngineCommand::RefreshStream {
                session_id,
                generation,
                ..
            } = &command
                && self.refresh_fails_after
            {
                self.snapshot_tx.send_replace(PlaybackSnapshot {
                    generation: *generation,
                    session_id: Some(*session_id),
                    state: EngineState::Failed,
                    last_end_cause: Some(EndCause::DecodeFailure),
                    ..PlaybackSnapshot::default()
                });
            }
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

    fn core(
        adapters: Vec<(String, Arc<dyn SourceAdapter>)>,
    ) -> (PlaybackCore, Arc<RecordingEngine>) {
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: false,
        });
        (
            PlaybackCore::new(
                SourceCatalog::new(adapters),
                engine.clone(),
                Duration::from_secs(1),
            ),
            engine,
        )
    }

    #[test]
    fn snapshot_exposes_a_nonempty_process_runtime_identity() {
        let (core, _) = core(Vec::new());

        let first = core.snapshot().runtime_identity;
        let second = core.snapshot().runtime_identity;

        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    /// 账号 VIP 已知：VIP 标注直接按账号改写，不探测；
    /// 账号 VIP 未知：探测兜底。
    #[tokio::test]
    async fn vip_candidates_use_account_status_before_probing() {
        struct VipSource {
            account: Option<ProviderAccountStatus>,
            probes: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SourceAdapter for VipSource {
            async fn search(
                &self,
                _spec: &SearchSpec,
            ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
                Ok(vec![ProviderSearchCandidate {
                    song: song("vip", "1"),
                    eligibility: PlaybackEligibility::VipRequired,
                }])
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

            async fn probe_eligibility(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<PlaybackEligibility, CatalogError> {
                self.probes.fetch_add(1, Ordering::SeqCst);
                Ok(PlaybackEligibility::Eligible)
            }

            async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
                Ok(self.account.clone())
            }
        }

        fn status(vip: bool, vip_known: bool) -> ProviderAccountStatus {
            ProviderAccountStatus {
                logged_in: true,
                vip_known,
                vip,
                ..ProviderAccountStatus::default()
            }
        }

        // 账号有 VIP：直接放行，探测 0 次。
        let probes = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(VipSource {
            account: Some(status(true, true)),
            probes: probes.clone(),
        });
        let candidates = probe_candidates(
            source,
            vec![ProviderSearchCandidate {
                song: song("vip", "1"),
                eligibility: PlaybackEligibility::VipRequired,
            }],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::Eligible);

        // 账号查询临时失败时，过期缓存中的已知 VIP 结论仍参与播放筛选。
        let probes = Arc::new(AtomicUsize::new(0));
        let mut stale_vip = status(true, true);
        stale_vip.stale = true;
        stale_vip.last_error = Some("provider_transient".to_owned());
        let source = Arc::new(VipSource {
            account: Some(stale_vip),
            probes: probes.clone(),
        });
        let candidates = probe_candidates(
            source,
            vec![ProviderSearchCandidate {
                song: song("vip", "stale"),
                eligibility: PlaybackEligibility::VipRequired,
            }],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::Eligible);

        // 账号缓存为非 VIP 时仍探测一次，避免刚开通会员时被旧负缓存挡住。
        let probes = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(VipSource {
            account: Some(status(false, true)),
            probes: probes.clone(),
        });
        let candidates = probe_candidates(
            source,
            vec![ProviderSearchCandidate {
                song: song("vip", "1"),
                eligibility: PlaybackEligibility::VipRequired,
            }],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::Eligible);

        // 账号 VIP 未知（未登录/查询失败）：探测兜底。
        let probes = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(VipSource {
            account: None,
            probes: probes.clone(),
        });
        let candidates = probe_candidates(
            source,
            vec![ProviderSearchCandidate {
                song: song("vip", "1"),
                eligibility: PlaybackEligibility::VipRequired,
            }],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::Eligible);
    }

    #[tokio::test]
    async fn confirmed_non_vip_probe_keeps_the_optimistic_account_cache() {
        struct ConfirmedNonVipSource {
            invalidations: Arc<AtomicUsize>,
            probes: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SourceAdapter for ConfirmedNonVipSource {
            async fn search(
                &self,
                _spec: &SearchSpec,
            ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
                Ok(Vec::new())
            }

            async fn resolve(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<StreamSource, CatalogError> {
                unreachable!("the explicit probe implementation is used")
            }

            async fn probe_eligibility(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<PlaybackEligibility, CatalogError> {
                self.probes.fetch_add(1, Ordering::SeqCst);
                Err(CatalogError::VipRequired("membership".to_owned()))
            }

            async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
                Ok(Some(ProviderAccountStatus {
                    logged_in: true,
                    vip_known: true,
                    vip: false,
                    ..ProviderAccountStatus::default()
                }))
            }

            fn invalidate_account_status(&self) {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
            }
        }

        let invalidations = Arc::new(AtomicUsize::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let candidates = probe_candidates(
            Arc::new(ConfirmedNonVipSource {
                invalidations: invalidations.clone(),
                probes: probes.clone(),
            }),
            vec![ProviderSearchCandidate {
                song: song("vip", "membership"),
                eligibility: PlaybackEligibility::VipRequired,
            }],
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::VipRequired);
        assert_eq!(invalidations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_vip_candidates_skip_account_status_lookup() {
        struct UnknownSource {
            account_queries: Arc<AtomicUsize>,
            probes: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SourceAdapter for UnknownSource {
            async fn search(
                &self,
                _spec: &SearchSpec,
            ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
                Ok(Vec::new())
            }

            async fn resolve(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<StreamSource, CatalogError> {
                unreachable!("the explicit probe implementation is used")
            }

            async fn probe_eligibility(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<PlaybackEligibility, CatalogError> {
                self.probes.fetch_add(1, Ordering::SeqCst);
                Ok(PlaybackEligibility::Eligible)
            }

            async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
                self.account_queries.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
        }

        let account_queries = Arc::new(AtomicUsize::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let candidates = probe_candidates(
            Arc::new(UnknownSource {
                account_queries: account_queries.clone(),
                probes: probes.clone(),
            }),
            vec![ProviderSearchCandidate {
                song: song("test", "unknown"),
                eligibility: PlaybackEligibility::Unknown,
            }],
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(account_queries.load(Ordering::SeqCst), 0);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert_eq!(candidates[0].eligibility, PlaybackEligibility::Eligible);
    }

    #[tokio::test]
    async fn only_account_related_playback_failures_invalidate_optimistic_status() {
        struct FailingSource {
            error: CatalogError,
            invalidations: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SourceAdapter for FailingSource {
            async fn search(
                &self,
                _spec: &SearchSpec,
            ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
                Ok(Vec::new())
            }

            async fn resolve(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<StreamSource, CatalogError> {
                Err(self.error.clone())
            }

            fn invalidate_account_status(&self) {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
            }
        }

        for (error, expected_invalidations) in [
            (CatalogError::AuthRequired("expired".to_owned()), 1),
            (CatalogError::CredentialRejected("expired".to_owned()), 1),
            (CatalogError::VipRequired("membership".to_owned()), 1),
            (CatalogError::Unavailable("track".to_owned()), 0),
            (
                CatalogError::InvalidResolverLocator("malformed locator".to_owned()),
                0,
            ),
        ] {
            let invalidations = Arc::new(AtomicUsize::new(0));
            let (core, _) = core(vec![(
                "source".to_owned(),
                Arc::new(FailingSource {
                    error,
                    invalidations: invalidations.clone(),
                }),
            )]);

            let result = core
                .resolve_stream(&SongKey::new("source", "track").unwrap(), None)
                .await;
            assert!(matches!(result, Err(PlaybackCoreError::Catalog(_))));
            assert_eq!(invalidations.load(Ordering::SeqCst), expected_invalidations);
        }
    }

    #[tokio::test]
    async fn stream_retry_observes_account_related_resolution_failures() {
        struct RetryFailureSource {
            invalidations: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SourceAdapter for RetryFailureSource {
            async fn search(
                &self,
                _spec: &SearchSpec,
            ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
                Ok(Vec::new())
            }

            async fn resolve(
                &self,
                _key: &SongKey,
                _locator: Option<&ResolverLocator>,
            ) -> Result<StreamSource, CatalogError> {
                Err(CatalogError::CredentialRejected("expired".to_owned()))
            }

            fn invalidate_account_status(&self) {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
            }
        }

        let invalidations = Arc::new(AtomicUsize::new(0));
        let catalog = SourceCatalog::new([(
            "source".to_owned(),
            Arc::new(RetryFailureSource {
                invalidations: invalidations.clone(),
            }) as Arc<dyn SourceAdapter>,
        )]);

        let stream = resolve_stream_retry(
            &catalog,
            &SongKey::new("source", "track").unwrap(),
            None,
            Duration::from_secs(1),
        )
        .await;

        assert!(stream.is_none());
        assert_eq!(invalidations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn search_keeps_successful_sources_and_reports_failed_sources() {
        let (core, _) = core(vec![
            (
                "qq".to_owned(),
                Arc::new(FakeSource {
                    songs: vec![song("qq", "q1"), song("qq", "q2")],
                    fail_search: false,
                }),
            ),
            (
                "wy".to_owned(),
                Arc::new(FakeSource {
                    songs: Vec::new(),
                    fail_search: true,
                }),
            ),
        ]);

        let response = core
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
        let (core, engine) = core(vec![(
            "qq".to_owned(),
            Arc::new(FakeSource {
                songs: Vec::new(),
                fail_search: false,
            }),
        )]);

        let receipt = core
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

    #[tokio::test]
    async fn play_cancellable_forwards_the_restored_seek_position_to_engine_start() {
        let (core, engine) = core(vec![(
            "qq".to_owned(),
            Arc::new(FakeSource {
                songs: Vec::new(),
                fail_search: false,
            }),
        )]);
        // 保持取消发送端存活：Sender 被 drop 会立即触发取消信号。
        let (_cancellation_tx, cancellation) = tokio::sync::oneshot::channel::<()>();

        let receipt = core
            .play_cancellable(
                SongKey::new("qq", "song-1").unwrap(),
                None,
                None,
                EndBehavior::NotifyController,
                cancellation,
                true,
                Some(42.5),
            )
            .await
            .unwrap();

        assert_eq!(receipt.generation, 1);
        // 恢复起始位置必须随 Start 命令到达引擎：新会话从该进度续播。
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EngineCommand::Start { seek_seconds: Some(seek), .. }]
                if (*seek - 42.5).abs() < 1e-9
        ));
    }

    #[tokio::test]
    async fn preload_then_play_uses_cached_resolve() {
        let (source, calls) = counting_source(None);
        let (core, _) = core(vec![("qq".to_owned(), source)]);
        let key = SongKey::new("qq", "song-1").unwrap();

        core.preload(key.clone(), None, None).await.unwrap();
        core.preload(key.clone(), None, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        core.play(key.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 无过期字段的 URL 在兜底窗口内仍命中缓存。
        core.play(key, None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn play_uses_cached_resolve_within_signed_expiry() {
        let expires_at = unix_epoch_ms() + 2000;
        let (source, calls) = counting_source(Some(expires_at));
        let (core, _) = core(vec![("qq".to_owned(), source)]);
        let key = SongKey::new("qq", "song-1").unwrap();

        core.play(key.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 有效期内重复播放：命中缓存。
        core.play(key.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 签名过期后：重新解析。
        tokio::time::sleep(Duration::from_millis(2100)).await;
        core.play(key, None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn resolve_cache_evicts_the_oldest_entry() {
        let (source, calls) = counting_source(None);
        let (core, _) = core(vec![("qq".to_owned(), source)]);

        for index in 0..(RESOLVE_CACHE_MAX_ENTRIES + 2) {
            let key = SongKey::new("qq", format!("song-{index}").as_str()).unwrap();
            core.preload(key, None, None).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), RESOLVE_CACHE_MAX_ENTRIES + 2);

        // 最旧的两首已被淘汰：重新解析。
        for index in 0..2 {
            let key = SongKey::new("qq", format!("song-{index}").as_str()).unwrap();
            core.play(key, None, EndBehavior::NotifyController)
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), RESOLVE_CACHE_MAX_ENTRIES + 4);

        // 最新的仍在缓存中：命中。
        let newest = SongKey::new(
            "qq",
            format!("song-{}", RESOLVE_CACHE_MAX_ENTRIES + 1).as_str(),
        )
        .unwrap();
        core.play(newest, None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), RESOLVE_CACHE_MAX_ENTRIES + 4);
    }

    struct RetrySource {
        resolve_count: AtomicUsize,
        seen_locators: Mutex<Vec<Option<ResolverLocator>>>,
    }

    #[async_trait]
    impl SourceAdapter for RetrySource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
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
        let (core, engine) = core(vec![("tx".to_owned(), source.clone())]);
        let song_key = SongKey::new("tx", "song-1").unwrap();
        let resolver_locator = ResolverLocator::new("test-v1:persisted").unwrap();
        let receipt = core
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

    #[tokio::test]
    async fn stream_retry_second_round_skips_cache_when_first_round_still_fails() {
        // 第一轮刷新（缓存路径）后仍失败 → 第二轮重新解析并直连源站（跳过缓存）。
        let source = Arc::new(RetrySource {
            resolve_count: AtomicUsize::new(0),
            seen_locators: Mutex::new(Vec::new()),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: true,
        });
        let core = PlaybackCore::new(
            SourceCatalog::new([("tx".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        );
        let song_key = SongKey::new("tx", "song-1").unwrap();
        let receipt = core
            .play(song_key.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();

        // 初始播放失败（流类失败）→ 触发两轮重试。
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

        // 等两轮 RefreshStream 全部发出。
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if engine.commands.lock().unwrap().len() >= 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(source.resolve_count.load(Ordering::SeqCst), 3);
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [
                EngineCommand::Start { .. },
                EngineCommand::RefreshStream { stream: first, .. },
                EngineCommand::RefreshStream { stream: second, .. }
            ] if first.url.as_str() == "https://example.test/audio-2.mp3"
                && second.url.as_str() == "https://example.test/audio-3.mp3"
        ));
    }

    struct OrderedSource {
        old_started: Notify,
        release_old: Notify,
    }

    #[async_trait]
    impl SourceAdapter for OrderedSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
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
        let core = PlaybackCore::new(
            SourceCatalog::new([("tx".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        );

        let old_core = core.clone();
        let old = tokio::spawn(async move {
            old_core
                .play(
                    SongKey::new("tx", "old").unwrap(),
                    None,
                    EndBehavior::NotifyController,
                )
                .await
        });
        source.old_started.notified().await;

        let newer = core
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
            Err(PlaybackCoreError::Engine(EngineError::Rejected(_)))
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
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
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
        let (core, engine) = core(vec![("tx".to_owned(), source.clone())]);

        let older_core = core.clone();
        let older = tokio::spawn(async move {
            older_core
                .play(
                    SongKey::new("tx", "old").unwrap(),
                    None,
                    EndBehavior::NotifyController,
                )
                .await
        });
        source.old_started.notified().await;

        let newer = core
            .play(
                SongKey::new("tx", "new").unwrap(),
                None,
                EndBehavior::NotifyController,
            )
            .await;
        assert!(matches!(
            newer,
            Err(PlaybackCoreError::Catalog(CatalogError::Transient(_)))
        ));

        source.release_old.notify_one();
        let older = older.await.unwrap();

        assert!(matches!(
            older,
            Err(PlaybackCoreError::Engine(EngineError::Rejected(message)))
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
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
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
        let (core, engine) = core(vec![("tx".to_owned(), source.clone())]);
        let old_song = SongKey::new("tx", "old").unwrap();
        let old = core
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

        core.play(
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

    struct FixedUrlSource {
        url: String,
    }

    #[async_trait]
    impl SourceAdapter for FixedUrlSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            Ok(StreamSource {
                url: Url::parse(&self.url).unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            })
        }
    }

    /// 预加载启用音频缓存时主动下载音频数据：播放时直接命中本地文件，不再访问源站。
    #[tokio::test]
    async fn preload_with_audio_cache_predownloads_audio_data() {
        // 假源站：计数 + 返回固定音频数据。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let body: Vec<u8> = vec![4u8; 2048];
        tokio::spawn({
            let requests = requests.clone();
            let body = body.clone();
            async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let mut buffer = [0u8; 2048];
                    let _ = stream.read(&mut buffer).await;
                    requests.fetch_add(1, Ordering::SeqCst);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                }
            }
        });

        let directory = std::env::temp_dir().join(format!(
            "miliastra-core-preload-cache-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let cache = crate::cache::AudioCache::spawn(crate::cache::AudioCacheConfig {
            enabled: true,
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_bytes: 64 * 1024 * 1024,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(5),
            max_registry_entries: 16,
        })
        .await
        .expect("缓存启动");

        let source = Arc::new(FixedUrlSource {
            url: format!("http://{addr}/audio.mp3"),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: false,
        });
        let core = PlaybackCore::new(
            SourceCatalog::new([("fake".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        )
        .with_audio_cache(cache.clone());
        let key = SongKey::new("fake", "preload-1").unwrap();

        // 预加载：注册缓存条目并主动下载音频数据。
        core.preload(key.clone(), None, None).await.unwrap();

        // 播放：Start 命令的音源是本地代理地址，数据已由预加载下载完成。
        let _receipt = core
            .play(key, None, EndBehavior::NotifyController)
            .await
            .unwrap();
        let proxy_url = {
            let commands = engine.commands.lock().unwrap();
            let EngineCommand::Start { stream, .. } = &commands[0] else {
                panic!("expected start command");
            };
            assert!(
                stream.url.as_str().starts_with("http://127.0.0.1:"),
                "播放音源应改写为本地代理: {}",
                stream.url
            );
            stream.url.clone()
        };
        let data = reqwest::get(proxy_url)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&data[..], &body[..]);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "预下载已完成，播放不应再访问源站"
        );

        cache.shutdown().await;
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// resolver 恒失败的假音源：验证磁盘缓存优先路径不依赖 provider。
    struct FailingResolveSource {
        resolve_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SourceAdapter for FailingResolveSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
        }

        async fn resolve(
            &self,
            _key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<StreamSource, CatalogError> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            Err(CatalogError::Transient("源站离线".to_owned()))
        }
    }

    /// 重启后进程内 resolve_cache 为空：磁盘已有 `.complete` 文件时，
    /// 直接注册本地代理条目并返回代理音源，不调用 provider resolve；
    /// resolver 失败时仍可播放，代理直接服务磁盘完整文件。
    #[tokio::test]
    async fn disk_complete_cache_plays_when_resolver_fails() {
        let directory = std::env::temp_dir().join(format!(
            "miliastra-core-disk-complete-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let cache = crate::cache::AudioCache::spawn(crate::cache::AudioCacheConfig {
            enabled: true,
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_bytes: 64 * 1024 * 1024,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(5),
            max_registry_entries: 16,
        })
        .await
        .expect("缓存启动");

        // 磁盘预置上次运行遗留的完整缓存文件；源 URL 不存在（无任何可达源站）。
        let key = SongKey::new("fake", "disk-1").unwrap();
        let hash = crate::cache::cache_key_hash(&key);
        let body: Vec<u8> = vec![9u8; 4096];
        std::fs::write(directory.join(format!("{hash}.complete")), &body).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(FailingResolveSource {
            resolve_calls: calls.clone(),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: false,
        });
        let core = PlaybackCore::new(
            SourceCatalog::new([("fake".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        )
        .with_audio_cache(cache.clone());

        // resolver 失败，但磁盘完整文件存在：播放必须成功。
        let _receipt = core
            .play(key.clone(), None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "磁盘命中不得调用 provider resolve"
        );

        // Start 命令的音源是本地代理地址，且代理直接服务磁盘完整文件。
        let proxy_url = {
            let commands = engine.commands.lock().unwrap();
            let EngineCommand::Start { stream, .. } = &commands[0] else {
                panic!("expected start command");
            };
            assert!(
                stream.url.as_str().starts_with("http://127.0.0.1:"),
                "播放音源应改写为本地代理: {}",
                stream.url
            );
            stream.url.clone()
        };
        let data = reqwest::get(proxy_url)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&data[..], &body[..]);

        cache.shutdown().await;
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// 预加载同样磁盘优先：完整文件已落盘时跳过 provider resolve。
    #[tokio::test]
    async fn preload_prefers_disk_complete_when_resolver_fails() {
        let directory = std::env::temp_dir().join(format!(
            "miliastra-core-preload-disk-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let cache = crate::cache::AudioCache::spawn(crate::cache::AudioCacheConfig {
            enabled: true,
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_bytes: 64 * 1024 * 1024,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(5),
            max_registry_entries: 16,
        })
        .await
        .expect("缓存启动");

        let key = SongKey::new("fake", "preload-disk-1").unwrap();
        let hash = crate::cache::cache_key_hash(&key);
        std::fs::write(directory.join(format!("{hash}.complete")), vec![3u8; 1024]).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(FailingResolveSource {
            resolve_calls: calls.clone(),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: false,
        });
        let core = PlaybackCore::new(
            SourceCatalog::new([("fake".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        )
        .with_audio_cache(cache.clone());

        // 预加载命中磁盘：不调用 provider resolve。
        core.preload(key.clone(), None, None).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "预加载磁盘命中不得调用 provider resolve"
        );

        // 预加载后播放同样命中（resolve_cache 已写入代理音源）。
        core.play(key, None, EndBehavior::NotifyController)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        cache.shutdown().await;
        let _ = std::fs::remove_dir_all(&directory);
    }

    struct CountingLyricsSource {
        lyrics_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SourceAdapter for CountingLyricsSource {
        async fn search(
            &self,
            _spec: &SearchSpec,
        ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
            Ok(Vec::new())
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

        async fn lyrics(
            &self,
            _key: &SongKey,
            _locator: Option<&ResolverLocator>,
        ) -> Result<Option<TimedLyrics>, CatalogError> {
            self.lyrics_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::lyrics::TimedLyrics::new(vec![
                crate::lyrics::TimedLyricLine {
                    start_ms: 0,
                    text: "第一句".to_owned(),
                    translation: Some("translation".to_owned()),
                },
            ]))
        }
    }

    /// 歌词首次网络获取后写入缓存：再次请求直接命中，不再访问平台接口。
    #[tokio::test]
    async fn lyrics_are_cached_after_the_first_network_fetch() {
        let directory = std::env::temp_dir().join(format!(
            "miliastra-core-lyrics-cache-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let cache = crate::cache::AudioCache::spawn(crate::cache::AudioCacheConfig {
            enabled: true,
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_bytes: 64 * 1024 * 1024,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(5),
            max_registry_entries: 16,
        })
        .await
        .expect("缓存启动");

        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingLyricsSource {
            lyrics_calls: calls.clone(),
        });
        let (snapshot_tx, snapshot) = watch::channel(PlaybackSnapshot::default());
        let engine = Arc::new(RecordingEngine {
            commands: Mutex::new(Vec::new()),
            snapshot_tx,
            snapshot,
            refresh_fails_after: false,
        });
        let core = PlaybackCore::new(
            SourceCatalog::new([("fake".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        )
        .with_audio_cache(cache.clone());
        let key = SongKey::new("fake", "lyrics-1").unwrap();

        let first = core.lyrics(key.clone(), None).await.unwrap().unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 第二次：命中缓存，不再访问平台。
        let second = core.lyrics(key.clone(), None).await.unwrap().unwrap();
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 缓存文件已落盘：新 core（同一缓存目录）也能命中。
        let core2 = PlaybackCore::new(
            SourceCatalog::new([("fake".to_owned(), source.clone() as Arc<dyn SourceAdapter>)]),
            engine.clone(),
            Duration::from_secs(1),
        )
        .with_audio_cache(cache.clone());
        let third = core2.lyrics(key, None).await.unwrap().unwrap();
        assert_eq!(third, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.shutdown().await;
        let _ = std::fs::remove_dir_all(&directory);
    }
}
