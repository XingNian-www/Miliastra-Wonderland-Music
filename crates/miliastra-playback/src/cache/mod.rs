//! 音频数据缓存：本地代理 + 磁盘缓存 + 流式下载。
//!
//! 播放前的音源 URL 被改写为本机代理地址；代理首次请求时向源站发起
//! 流式下载，边下载边以 chunked 传输喂给解码器，同时把数据写盘；
//! 下载完成后后续播放直接从本地文件服务（支持 Range），断网也能播。

mod download;
mod metadata;
mod proxy;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Mutex, Notify, RwLock, watch};

use serde::Serialize;

use crate::domain::{SongKey, StreamSource};
use crate::model::TrackMetadata;

/// 代理 URL 的路径前缀。
pub(crate) const PROXY_PATH_PREFIX: &str = "/audio/";

/// 默认磁盘缓存上限（20 GiB）。
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// 默认运行时缓存条目上限；与默认播放池容量保持一致。
pub const DEFAULT_MAX_REGISTRY_ENTRIES: usize = 1000;

#[derive(Clone, Debug)]
pub struct AudioCacheConfig {
    pub enabled: bool,
    /// 缓存根目录；为空时使用系统临时目录。
    pub directory: PathBuf,
    /// 统一播放数据库目录；数据库文件固定为 playback.sqlite3。
    /// 为空时使用系统临时目录下的独立数据目录。
    pub metadata_directory: PathBuf,
    /// 磁盘占用上限，超出后按最近使用时间淘汰完整文件。
    pub max_bytes: u64,
    /// 同时进行的源站下载任务上限。
    pub max_concurrent_downloads: usize,
    /// 源站连接/响应超时。
    pub request_timeout: Duration,
    /// 请求尚未下载完成的位置时，等待下载推进的最长时间。
    pub seek_wait_timeout: Duration,
    /// 注册表条目上限（防止过期条目堆积）。
    pub max_registry_entries: usize,
}

impl Default for AudioCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::new(),
            metadata_directory: PathBuf::new(),
            max_bytes: DEFAULT_MAX_CACHE_BYTES,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(10),
            max_registry_entries: DEFAULT_MAX_REGISTRY_ENTRIES,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioCacheStats {
    pub enabled: bool,
    pub directory: String,
    pub complete_entries: usize,
    pub complete_bytes: u64,
    pub max_bytes: u64,
    pub active_downloads: usize,
    pub lyrics_entries: usize,
    pub lyrics_bytes: u64,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioCacheTrackStatus {
    pub source: String,
    pub id: String,
    pub cached: bool,
    pub bytes: Option<u64>,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
    pub last_played_at_ms: Option<u64>,
    pub last_failure_code: Option<String>,
}

/// 磁盘缓存歌曲列表中的单条记录（数据来自元数据索引）。
///
/// 对账登记的孤儿完整文件尚无身份信息，此时 source/id/title/artists/
/// album/durationMs 均为 None，前端展示为「未知缓存」。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedTrackInfo {
    /// 缓存键哈希（source:id 的 md5），也是磁盘文件名与索引主键。
    pub hash: String,
    pub source: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub artists: Option<Vec<String>>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    /// 完整文件字节数（未完整时为 0）。
    pub bytes: u64,
    /// 音频完整文件是否已落盘。
    pub complete: bool,
    /// 首次登记缓存的时间（Unix 毫秒）。
    pub cached_at_ms: u64,
    /// 最近使用时间（Unix 毫秒）。
    pub last_used_at_ms: u64,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
    pub last_played_at_ms: Option<u64>,
    pub last_failure_code: Option<String>,
    pub downloaded_at_ms: Option<u64>,
}

/// 磁盘缓存歌曲分页结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedTrackPage {
    /// 缓存歌曲总条数（与 offset/limit 无关）。
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub tracks: Vec<CachedTrackInfo>,
}

impl CachedTrackPage {
    /// 空页：缓存未启用或查询失败时使用，保留请求的分页参数。
    pub fn empty(offset: usize, limit: usize) -> Self {
        Self {
            total: 0,
            offset,
            limit,
            tracks: Vec::new(),
        }
    }
}

pub(crate) fn cache_file_name(hash: &str, complete: bool) -> String {
    if complete {
        format!("{hash}.complete")
    } else {
        format!("{hash}.part")
    }
}

pub(crate) fn cache_key_hash(key: &SongKey) -> String {
    let digest = md5::compute(format!("{}:{}", key.source, key.id));
    format!("{digest:x}")
}

/// 下载进度句柄：供代理等待数据推进。
#[derive(Clone)]
pub(crate) struct DownloadHandle {
    /// 下载任务身份令牌；状态被替换后旧任务不得再更新新条目。
    pub(crate) token: Arc<()>,
    pub(crate) ready: Arc<Notify>,
    pub(crate) bytes: Arc<AtomicUsize>,
    pub(crate) failed: Arc<AtomicBool>,
    /// 下载任务结束（成功或失败）后置位，避免等待方在最后一块数据后挂到超时。
    pub(crate) finished: Arc<AtomicBool>,
}

#[derive(Clone)]
enum EntryState {
    /// 已注册但尚未开始下载。
    Idle,
    /// 下载进行中；`failed` 置位表示下载失败可重试。
    Downloading(DownloadHandle),
    /// 文件完整可用。
    Complete { bytes: u64 },
}

pub(crate) struct Entry {
    source: StreamSource,
    file: PathBuf,
    state: EntryState,
    last_used: Instant,
}

/// 音频缓存门面：持有本地代理服务器与磁盘缓存状态。
#[derive(Clone)]
pub struct AudioCache {
    inner: Arc<CacheInner>,
}

pub(crate) struct CacheInner {
    pub(crate) config: AudioCacheConfig,
    pub(crate) port: std::sync::atomic::AtomicU16,
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) entries: RwLock<HashMap<String, Entry>>,
    pub(crate) client: reqwest::Client,
    pub(crate) active_downloads: Arc<AtomicUsize>,
    pub(crate) downloaded_bytes: Arc<AtomicUsize>,
    /// 并发下载槽位：限制同时进行的源站下载任务数。
    pub(crate) download_slots: Arc<tokio::sync::Semaphore>,
    /// SQLite 元数据索引（曲目身份/完整状态/歌词正文）。
    metadata_store: Arc<std::sync::Mutex<metadata::MetadataStore>>,
    /// 串行化容量淘汰，避免并发扫描并重复删除同一批文件。
    trim_lock: Mutex<()>,
}

/// 获取元数据存储锁；被 panic 污染时恢复使用，保证缓存功能不受影响。
/// 异步路径不得直接调用（SQLite 会阻塞 runtime），须经 `spawn_blocking`。
fn lock_metadata_store(
    store: &Arc<std::sync::Mutex<metadata::MetadataStore>>,
) -> std::sync::MutexGuard<'_, metadata::MetadataStore> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn inner_concurrency(configured: usize) -> usize {
    if configured == 0 { 1 } else { configured }
}

/// 注册表容量淘汰：超出上限时按最近使用移除非下载条目。
/// 下载中条目持有代理等待方与任务身份，绝不能因容量被移除；
/// 全部条目均在下载时允许暂时超限，下载结束后后续注册再收敛。
fn enforce_registry_limit(entries: &mut HashMap<String, Entry>, max_entries: usize) {
    while entries.len() > max_entries {
        let oldest = entries
            .iter()
            .filter(|(_, entry)| !matches!(entry.state, EntryState::Downloading(_)))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(hash, _)| hash.clone());
        if let Some(oldest) = oldest {
            entries.remove(&oldest);
        } else {
            break;
        }
    }
}

/// 构造本地代理地址的音源（代理 URL + 空请求头）。
fn local_proxy_stream(
    inner: &CacheInner,
    hash: &str,
    expires_at_epoch_ms: Option<u64>,
) -> StreamSource {
    let url = format!(
        "http://127.0.0.1:{}{}{}",
        inner.port.load(Ordering::SeqCst),
        PROXY_PATH_PREFIX,
        hash
    )
    .parse()
    .expect("local proxy url is well-formed");
    StreamSource {
        url,
        headers: Default::default(),
        expires_at_epoch_ms,
    }
}

impl AudioCache {
    /// 启动本地代理服务器（监听 127.0.0.1 随机端口）并加载已有缓存。
    pub async fn spawn(mut config: AudioCacheConfig) -> Result<Self, CacheError> {
        // 空目录解析为系统临时目录，并回写配置：后续文件路径与容量回收都以解析后的目录为准。
        let directory = if config.directory.as_os_str().is_empty() {
            std::env::temp_dir().join("miliastra-audio-cache")
        } else {
            config.directory.clone()
        };
        config.directory = directory.clone();
        // 统一播放数据库目录默认放在系统临时目录下的独立数据目录。
        // 音频容量淘汰只扫描缓存目录，不会触碰数据库。
        let metadata_directory = if config.metadata_directory.as_os_str().is_empty() {
            std::env::temp_dir().join("miliastra-playback-data")
        } else {
            config.metadata_directory.clone()
        };
        config.metadata_directory = metadata_directory.clone();
        let metadata = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)
                .map_err(|error| CacheError::Io(format!("创建音频缓存目录失败: {error}")))?;
            // 打开（或创建）元数据数据库：启动期 open/schema 错误直接失败。
            metadata::MetadataStore::open(&metadata_directory)
                .map_err(|error| CacheError::Metadata(error.to_string()))
        })
        .await
        .map_err(|error| CacheError::Io(format!("初始化音频缓存任务失败: {error}")))??;
        let client = reqwest::Client::builder()
            .connect_timeout(config.request_timeout)
            .build()
            .map_err(|error| CacheError::Io(format!("构建下载客户端失败: {error}")))?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let download_slots = inner_concurrency(config.max_concurrent_downloads);
        let inner = Arc::new(CacheInner {
            config,
            port: std::sync::atomic::AtomicU16::new(0),
            shutdown,
            entries: RwLock::new(HashMap::new()),
            client,
            active_downloads: Arc::new(AtomicUsize::new(0)),
            downloaded_bytes: Arc::new(AtomicUsize::new(0)),
            download_slots: Arc::new(tokio::sync::Semaphore::new(download_slots)),
            metadata_store: Arc::new(std::sync::Mutex::new(metadata)),
            trim_lock: Mutex::new(()),
        });
        let port = proxy::spawn_proxy_server(inner.clone(), shutdown_rx).await?;
        inner.port.store(port, Ordering::SeqCst);
        // 启动时清理上次运行残留的半成品，再做 DB 与磁盘的双向对账；
        // 文件扫描和 SQLite 操作全部放入阻塞线程池。
        let reconcile_inner = inner.clone();
        let complete_bytes = tokio::task::spawn_blocking(move || {
            cleanup_partial_files(&reconcile_inner)?;
            Ok::<_, CacheError>(reconcile_metadata(&reconcile_inner))
        })
        .await
        .map_err(|error| CacheError::Io(format!("音频缓存对账任务失败: {error}")))??;
        inner
            .downloaded_bytes
            .store(complete_bytes as usize, Ordering::SeqCst);
        trim_cache(inner.clone(), inner.config.max_bytes).await;
        Ok(Self { inner })
    }

    /// 把音源改写为本机代理地址并注册缓存条目；曲目元数据（若有）随
    /// 注册一并 upsert 进元数据索引。索引失败只 warning，不影响播放。
    pub async fn rewrite(
        &self,
        key: &SongKey,
        metadata: Option<&TrackMetadata>,
        stream: StreamSource,
    ) -> StreamSource {
        let inner = &self.inner;
        if !inner.config.enabled {
            return stream;
        }
        let hash = cache_key_hash(key);
        // 登记必须先于下载完整状态写入，避免并发时 mark_complete 找不到曲目行；
        // SQLite 在阻塞线程池执行，不占用异步 runtime。
        {
            let store = inner.metadata_store.clone();
            let key = key.clone();
            let metadata = metadata.cloned();
            let _ = tokio::task::spawn_blocking(move || {
                let store = lock_metadata_store(&store);
                if let Err(error) = store.upsert_track(&key, metadata.as_ref()) {
                    log::warn!("音频缓存元数据登记失败: {error}");
                }
            })
            .await;
        }
        {
            let mut entries = inner.entries.write().await;
            let file = inner.config.directory.join(cache_file_name(&hash, false));
            entries
                .entry(hash.clone())
                .and_modify(|entry| {
                    entry.source = stream.clone();
                    entry.last_used = Instant::now();
                })
                .or_insert_with(|| Entry {
                    source: stream.clone(),
                    file,
                    state: EntryState::Idle,
                    last_used: Instant::now(),
                });
            enforce_registry_limit(&mut entries, inner.config.max_registry_entries);
        }
        local_proxy_stream(inner, &hash, stream.expires_at_epoch_ms)
    }

    /// 磁盘已有完整音频文件时直接注册本地代理条目并返回代理音源，
    /// 不访问源站、不要求源 URL 可访问（代理直接服务磁盘文件）。
    /// 未命中（无 `.complete` 文件）返回 None。供播放/预加载在调用
    /// provider resolve 之前优先命中磁盘。
    pub async fn proxy_for_complete(
        &self,
        key: &SongKey,
        metadata: Option<&TrackMetadata>,
    ) -> Option<StreamSource> {
        let inner = &self.inner;
        if !inner.config.enabled {
            return None;
        }
        let hash = cache_key_hash(key);
        // 磁盘无完整文件：交给调用方走网络解析。
        let bytes = complete_file_size(&inner.config.directory, &hash)?;
        // 元数据登记先于条目注册（失败仅 warning，不影响播放）。
        {
            let store = inner.metadata_store.clone();
            let key = key.clone();
            let metadata = metadata.cloned();
            let _ = tokio::task::spawn_blocking(move || {
                let store = lock_metadata_store(&store);
                if let Err(error) = store.upsert_track(&key, metadata.as_ref()) {
                    log::warn!("音频缓存元数据登记失败: {error}");
                }
            })
            .await;
        }
        {
            let mut entries = inner.entries.write().await;
            entries
                .entry(hash.clone())
                .and_modify(|entry| {
                    // 已有条目（Idle/下载中/Complete）保持原状态与源地址：
                    // 代理请求时自行命中磁盘或继续下载，不打断进行中的任务。
                    entry.last_used = Instant::now();
                })
                .or_insert_with(|| Entry {
                    // 占位源地址：完整文件存在时代理直接服务磁盘，不会发起下载。
                    source: StreamSource {
                        url: "http://127.0.0.1:1/unreachable.mp3"
                            .parse()
                            .expect("placeholder url is well-formed"),
                        headers: Default::default(),
                        expires_at_epoch_ms: None,
                    },
                    file: inner.config.directory.join(cache_file_name(&hash, false)),
                    state: EntryState::Complete { bytes },
                    last_used: Instant::now(),
                });
            enforce_registry_limit(&mut entries, inner.config.max_registry_entries);
        }
        Some(local_proxy_stream(inner, &hash, None))
    }

    /// 停止代理服务器（进程退出时调用）。
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown.send_replace(true);
    }

    /// 持久化已进入 Playing 的会话；围栏在 SQL 内原子判重。
    pub async fn record_play(
        &self,
        key: &SongKey,
        session: uuid::Uuid,
        generation: u64,
        requested: bool,
    ) {
        let store = self.inner.metadata_store.clone();
        let hash = cache_key_hash(key);
        let session = session.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            let store = lock_metadata_store(&store);
            if let Err(error) = store.record_play(&hash, &session, generation, requested) {
                log::warn!("播放统计写入失败: {error}");
            }
        })
        .await;
    }

    /// 清零单曲统计；保留音频完整状态、曲目身份、元数据与歌词正文。
    pub async fn reset_statistics(&self, key: &SongKey) -> bool {
        let store = self.inner.metadata_store.clone();
        let hash = cache_key_hash(key);
        tokio::task::spawn_blocking(move || {
            let store = lock_metadata_store(&store);
            match store.reset_statistics(&hash) {
                Ok(changed) => changed,
                Err(error) => {
                    log::warn!("播放统计清零失败: {error}");
                    false
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// 持久化最终播放失败；调用方负责排除可重试中间态与过期会话。
    pub async fn record_failure(
        &self,
        key: &SongKey,
        session: uuid::Uuid,
        generation: u64,
        code: &str,
    ) {
        let store = self.inner.metadata_store.clone();
        let hash = cache_key_hash(key);
        let session = session.to_string();
        let code = code.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            let store = lock_metadata_store(&store);
            if let Err(error) = store.record_failure(&hash, &session, generation, &code) {
                log::warn!("播放失败统计写入失败: {error}");
            }
        })
        .await;
    }

    /// 删除曲目的缓存条目与磁盘文件（播放解码失败后自愈：下次播放重新下载）。
    /// 下载进行中的任务会被标记失败并自行清理残留。
    /// 即使内存注册表无条目，也会删除磁盘 `.part`/`.complete` 文件与
    /// 元数据数据库记录（含歌词正文）；返回是否实际删除了任何内容。
    pub async fn invalidate(&self, key: &SongKey) -> bool {
        let inner = &self.inner;
        if !inner.config.enabled {
            return false;
        }
        let hash = cache_key_hash(key);
        // 1. 移除内存条目；下载中的任务标记失败并唤醒等待方。
        let removed_entry = {
            let mut entries = inner.entries.write().await;
            match entries.remove(&hash) {
                Some(entry) => {
                    if let EntryState::Downloading(handle) = &entry.state {
                        handle.failed.store(true, Ordering::SeqCst);
                        handle.ready.notify_waiters();
                    }
                    true
                }
                None => false,
            }
        };
        // 2. 文件删除与 SQLite 操作均放入阻塞线程池。
        let directory = inner.config.directory.clone();
        let store = inner.metadata_store.clone();
        let blocking_hash = hash.clone();
        let (removed_part, removed_complete, removed_db, removed_lyrics, complete_bytes) =
            tokio::task::spawn_blocking(move || {
                let part_path = directory.join(cache_file_name(&blocking_hash, false));
                let complete_path = directory.join(cache_file_name(&blocking_hash, true));
                let complete_bytes = std::fs::metadata(&complete_path).ok().map(|m| m.len());
                let removed_part = std::fs::remove_file(&part_path).is_ok();
                let removed_complete = std::fs::remove_file(&complete_path).is_ok();
                let store = lock_metadata_store(&store);
                // 缓存失效只清资产状态，保留播放/失败历史。重试自愈不得抹掉统计。
                let removed_db = match store.clear_complete(&blocking_hash) {
                    Ok(changed) => changed,
                    Err(error) => {
                        log::warn!("音频缓存元数据状态清理失败: {error}");
                        false
                    }
                };
                // 歌词正文存于 SQLite，失效时一并删除。
                let removed_lyrics = store.remove_lyrics(&blocking_hash).unwrap_or(false);
                (
                    removed_part,
                    removed_complete,
                    removed_db,
                    removed_lyrics,
                    complete_bytes,
                )
            })
            .await
            .unwrap_or((false, false, false, false, None));
        if removed_complete && let Some(bytes) = complete_bytes {
            saturating_sub_atomic(&inner.downloaded_bytes, bytes as usize);
        }
        removed_entry || removed_part || removed_complete || removed_lyrics || removed_db
    }

    /// 主动下载曲目音源到缓存（播放成功后的后台预热）。
    /// 条目必须先经 `rewrite` 注册；下载失败静默，条目保持 Idle 等待下次代理请求重试。
    pub async fn prefetch(&self, key: &SongKey) -> bool {
        let inner = &self.inner;
        if !inner.config.enabled {
            return false;
        }
        let hash = cache_key_hash(key);
        match download::start_download(inner, &hash).await {
            Some(download::DownloadView::Complete { .. })
            | Some(download::DownloadView::InProgress(_)) => true,
            None => false,
        }
    }

    /// 返回缓存统计与指定曲目的完整缓存状态（数据来自元数据索引；
    /// 歌词条目数与字节数由 DB 聚合，不再扫描磁盘）。
    pub fn stats(&self, keys: &[SongKey]) -> (AudioCacheStats, Vec<AudioCacheTrackStatus>) {
        let inner = &self.inner;
        let directory = &inner.config.directory;
        let store = lock_metadata_store(&inner.metadata_store);
        let (stats, statuses) = match store.stats(keys) {
            Ok(result) => result,
            Err(error) => {
                log::warn!("音频缓存统计查询失败: {error}");
                (metadata::MetadataStats::default(), Vec::new())
            }
        };
        let tracks = statuses
            .into_iter()
            .map(|status| AudioCacheTrackStatus {
                source: status.key.source,
                id: status.key.id,
                cached: status.recorded && status.complete,
                bytes: status.bytes,
                play_count: status.play_count,
                requested_play_count: status.requested_play_count,
                pool_play_count: status.pool_play_count,
                cache_hit_count: status.cache_hit_count,
                failure_count: status.failure_count,
                last_played_at_ms: status.last_played_at_ms,
                last_failure_code: status.last_failure_code,
            })
            .collect();
        (
            AudioCacheStats {
                enabled: inner.config.enabled,
                directory: directory.to_string_lossy().into_owned(),
                complete_entries: stats.complete_tracks,
                complete_bytes: stats.complete_bytes,
                max_bytes: inner.config.max_bytes,
                active_downloads: inner.active_downloads.load(Ordering::SeqCst),
                lyrics_entries: stats.lyrics_tracks,
                lyrics_bytes: stats.lyrics_bytes,
                play_count: stats.play_count,
                requested_play_count: stats.requested_play_count,
                pool_play_count: stats.pool_play_count,
                cache_hit_count: stats.cache_hit_count,
                failure_count: stats.failure_count,
            },
            tracks,
        )
    }

    /// 分页查询磁盘缓存歌曲列表（按最近使用倒序；数据来自元数据索引）。
    /// 缓存未启用或查询失败时返回空页并记录 warning，不中断调用方。
    pub fn cached_tracks(&self, offset: usize, limit: usize) -> CachedTrackPage {
        let inner = &self.inner;
        if !inner.config.enabled {
            return CachedTrackPage::empty(offset, limit);
        }
        let store = lock_metadata_store(&inner.metadata_store);
        match store.list_tracks(offset, limit) {
            Ok((total, records)) => CachedTrackPage {
                total,
                offset,
                limit,
                tracks: records.into_iter().map(cached_track_info).collect(),
            },
            Err(error) => {
                log::warn!("音频缓存歌曲列表查询失败: {error}");
                CachedTrackPage::empty(offset, limit)
            }
        }
    }

    /// 读取曲目的缓存歌词（未命中或无歌词缓存返回 None）。
    /// 歌词正文存于 SQLite，读取在阻塞线程池执行。
    pub async fn get_lyrics(&self, key: &SongKey) -> Option<crate::lyrics::TimedLyrics> {
        let inner = &self.inner;
        if !inner.config.enabled {
            return None;
        }
        let hash = cache_key_hash(key);
        let store = inner.metadata_store.clone();
        let json = tokio::task::spawn_blocking(move || {
            let store = lock_metadata_store(&store);
            match store.get_lyrics_json(&hash) {
                Ok(json) => json,
                Err(error) => {
                    log::warn!("音频缓存歌词读取失败: {error}");
                    None
                }
            }
        })
        .await
        .unwrap_or(None)?;
        serde_json::from_slice(&json).ok()
    }

    /// 写入曲目的歌词缓存（跟随音频缓存的启用状态；正文存 SQLite）。
    pub async fn put_lyrics(&self, key: &SongKey, lyrics: &crate::lyrics::TimedLyrics) -> bool {
        let inner = &self.inner;
        if !inner.config.enabled {
            return false;
        }
        let Ok(json) = serde_json::to_vec(lyrics) else {
            return false;
        };
        let hash = cache_key_hash(key);
        let key = key.clone();
        let store = inner.metadata_store.clone();
        // SQLite 在阻塞线程池执行，不阻塞异步 runtime；失败返回 false。
        tokio::task::spawn_blocking(move || {
            let store = lock_metadata_store(&store);
            if let Err(error) = store.upsert_track(&key, None) {
                log::warn!("音频缓存歌词曲目登记失败: {error}");
                return false;
            }
            match store.upsert_lyrics(&hash, &json) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("音频缓存歌词保存失败: {error}");
                    false
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

/// 把元数据索引记录转换为公开的缓存歌曲信息（秒级时间戳换算为毫秒）。
fn cached_track_info(record: metadata::TrackRecord) -> CachedTrackInfo {
    let metadata = record.metadata.as_ref();
    CachedTrackInfo {
        hash: record.hash,
        source: record.source,
        id: record.id,
        title: metadata.map(|meta| meta.title.clone()),
        artists: metadata.map(|meta| meta.artists.clone()),
        album: metadata.and_then(|meta| meta.album.clone()),
        duration_ms: metadata.and_then(|meta| meta.duration_ms),
        bytes: record.bytes,
        complete: record.complete,
        cached_at_ms: record.created_at_epoch_secs.max(0) as u64 * 1000,
        last_used_at_ms: record.updated_at_epoch_secs.max(0) as u64 * 1000,
        play_count: record.play_count,
        requested_play_count: record.requested_play_count,
        pool_play_count: record.pool_play_count,
        cache_hit_count: record.cache_hit_count,
        failure_count: record.failure_count,
        last_played_at_ms: record.last_played_at_ms,
        last_failure_code: record.last_failure_code,
        downloaded_at_ms: record.downloaded_at_ms,
    }
}

/// 歌词正文存于 SQLite，容量回收仅针对音频文件，无歌词磁盘淘汰。
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("audio cache io error: {0}")]
    Io(String),
    #[error("audio cache metadata error: {0}")]
    Metadata(String),
}

/// 删除上次运行残留的半成品文件。
fn cleanup_partial_files(inner: &CacheInner) -> Result<(), CacheError> {
    let Ok(entries) = std::fs::read_dir(&inner.config.directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".part") {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// 按最近使用时间淘汰完整缓存文件，直到占用不超上限。
/// 阻塞阶段只做文件与 SQLite 操作并返回已删 hash；随后异步取得注册表锁，
/// 把仍为 Complete 的条目转回 Idle，避免状态继续指向已删除文件。
async fn trim_cache(inner: Arc<CacheInner>, max_bytes: u64) {
    let _trim_guard = inner.trim_lock.lock().await;
    let blocking_inner = inner.clone();
    let removed =
        tokio::task::spawn_blocking(move || trim_cache_blocking(&blocking_inner, max_bytes))
            .await
            .unwrap_or_default();
    if removed.is_empty() {
        return;
    }
    let removed: HashSet<String> = removed.into_iter().collect();
    let mut entries = inner.entries.write().await;
    for (hash, entry) in entries.iter_mut() {
        if removed.contains(hash) && matches!(entry.state, EntryState::Complete { .. }) {
            entry.state = EntryState::Idle;
            entry.file = inner.config.directory.join(cache_file_name(hash, false));
        }
    }
}

fn trim_cache_blocking(inner: &CacheInner, max_bytes: u64) -> Vec<String> {
    let mut total = inner.downloaded_bytes.load(Ordering::SeqCst) as u64;
    if total <= max_bytes {
        return Vec::new();
    }
    let mut complete_files: Vec<(PathBuf, u64, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&inner.config.directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".complete") {
                continue;
            }
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            complete_files.push((path, metadata.len(), modified));
        }
    }
    complete_files.sort_by_key(|(_, _, modified)| *modified);
    let mut removed = Vec::new();
    for (path, size, _) in complete_files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            saturating_sub_atomic(&inner.downloaded_bytes, size as usize);
            // 只清除音频完整状态，保留曲目身份、元数据和歌词正文。
            if let Some(hash) = path.file_stem().and_then(|name| name.to_str()) {
                removed.push(hash.to_owned());
                let store = lock_metadata_store(&inner.metadata_store);
                if let Err(error) = store.clear_complete(hash) {
                    log::warn!("音频缓存淘汰后清除完整状态失败: {error}");
                }
            }
        }
    }
    removed
}

fn saturating_sub_atomic(value: &AtomicUsize, amount: usize) {
    let _ = value.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        Some(current.saturating_sub(amount))
    });
}

/// 启动对账：DB 与磁盘双向校准音频完整状态/大小。
///
/// - DB 有完整记录但磁盘无 `.complete` 文件：清完整状态；
/// - 磁盘有 `.complete` 文件但 DB 无记录：登记无身份记录；
/// - 完整大小一律以磁盘文件为准；
///
/// 歌词正文存于 SQLite，无磁盘文件需要对账。
///
/// SQLite 错误只记录 warning，不影响缓存启动；返回磁盘完整音频总字节数，
/// 供调用方初始化下载量计数。
fn reconcile_metadata(inner: &CacheInner) -> u64 {
    let directory = &inner.config.directory;
    let store = lock_metadata_store(&inner.metadata_store);
    // 磁盘完整文件集合：hash → 实际大小。
    let mut disk_complete: HashMap<String, u64> = HashMap::new();
    let mut total_bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(hash) = name.strip_suffix(".complete") else {
                continue;
            };
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if metadata.is_file() {
                let bytes = metadata.len();
                disk_complete.insert(hash.to_owned(), bytes);
                total_bytes = total_bytes.saturating_add(bytes);
            }
        }
    }
    let records = match store.all_tracks() {
        Ok(records) => records,
        Err(error) => {
            log::warn!("音频缓存元数据对账失败（读取曲目记录）: {error}");
            return total_bytes;
        }
    };
    let mut db_hashes: HashSet<&str> = records.iter().map(|record| record.hash.as_str()).collect();
    // 1. DB 记录 vs 磁盘完整文件：文件缺失清完整状态；大小以文件为准刷新。
    for record in &records {
        match disk_complete.get(&record.hash) {
            Some(bytes) => {
                if (!record.complete || record.bytes != *bytes)
                    && let Err(error) = store.mark_complete(&record.hash, *bytes, false)
                {
                    log::warn!("音频缓存对账刷新完整状态失败: {error}");
                }
            }
            None => {
                if record.complete
                    && let Err(error) = store.clear_complete(&record.hash)
                {
                    log::warn!("音频缓存对账清除完整状态失败: {error}");
                }
            }
        }
    }
    // 2. 磁盘孤儿完整文件：DB 无记录则登记（source/id 未知，待 rewrite 补齐）。
    for (hash, bytes) in &disk_complete {
        if !db_hashes.contains(hash.as_str()) {
            match store.insert_unknown_complete(hash, *bytes) {
                Ok(()) => {
                    db_hashes.insert(hash);
                }
                Err(error) => log::warn!("音频缓存对账登记孤儿完整文件失败: {error}"),
            }
        }
    }
    total_bytes
}

/// 完整文件是否已存在于磁盘（供代理命中后直接服务）。
pub(crate) fn complete_file_size(directory: &Path, hash: &str) -> Option<u64> {
    let path = directory.join(cache_file_name(hash, true));
    path.metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_hash_is_stable_and_source_scoped() {
        let key_a = SongKey::new("qqmusic", "song-1").unwrap();
        let key_b = SongKey::new("netease", "song-1").unwrap();
        assert_eq!(cache_key_hash(&key_a), cache_key_hash(&key_a));
        assert_ne!(cache_key_hash(&key_a), cache_key_hash(&key_b));
    }

    #[test]
    fn file_names_reflect_stage() {
        assert_eq!(cache_file_name("abc", false), "abc.part");
        assert_eq!(cache_file_name("abc", true), "abc.complete");
    }

    #[tokio::test]
    async fn idle_entry_reuses_disk_complete_file_without_downloading() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-cache-{}", uuid::Uuid::new_v4()));
        let cache = AudioCache::spawn(AudioCacheConfig {
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            ..Default::default()
        })
        .await
        .unwrap();
        let key = crate::domain::SongKey::new("kugou", "abcdef0123456789").unwrap();
        let hash = cache_key_hash(&key);
        // 注册条目（Idle）。
        cache
            .rewrite(
                &key,
                None,
                crate::domain::StreamSource {
                    url: "http://127.0.0.1:1/unreachable.mp3".parse().unwrap(),
                    headers: std::collections::BTreeMap::new(),
                    expires_at_epoch_ms: None,
                },
            )
            .await;
        // 磁盘预置上次运行遗留的完整文件。
        let complete_path = directory.join(format!("{hash}.complete"));
        std::fs::write(&complete_path, b"fake-audio-bytes").unwrap();

        // 触发下载判断：应直接命中磁盘（源站不可达也不触发下载）。
        assert!(cache.prefetch(&key).await);
        assert!(complete_path.exists());

        let state = cache
            .inner
            .entries
            .read()
            .await
            .get(&hash)
            .map(|entry| entry.state.clone());
        assert!(matches!(state, Some(EntryState::Complete { .. })));
        cache.shutdown().await;
        drop(cache);
        // Windows 上代理任务和 SQLite 连接可能仍在异步收尾，测试只做尽力清理。
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn proxy_for_complete_serves_disk_file_without_source() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-cache-disk-{}", uuid::Uuid::new_v4()));
        let cache = AudioCache::spawn(AudioCacheConfig {
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            ..Default::default()
        })
        .await
        .unwrap();
        // 磁盘预置完整文件（模拟上次运行遗留；源 URL 不存在）。
        let key = SongKey::new("kugou", "disk-only").unwrap();
        let hash = cache_key_hash(&key);
        let body: Vec<u8> = vec![7u8; 2048];
        std::fs::write(directory.join(format!("{hash}.complete")), &body).unwrap();

        // 未命中：无完整文件时返回 None。
        let missing = SongKey::new("kugou", "missing").unwrap();
        assert!(cache.proxy_for_complete(&missing, None).await.is_none());

        // 命中：直接返回本地代理地址。
        let stream = cache
            .proxy_for_complete(&key, None)
            .await
            .expect("磁盘完整文件应命中");
        assert!(
            stream.url.as_str().starts_with("http://127.0.0.1:"),
            "应返回本地代理地址: {}",
            stream.url
        );

        // 代理直接服务磁盘完整文件（不发起任何源站请求）。
        let data = reqwest::get(stream.url)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&data[..], &body[..]);

        // 条目已注册为 Complete。
        let state = cache
            .inner
            .entries
            .read()
            .await
            .get(&hash)
            .map(|entry| entry.state.clone());
        assert!(matches!(state, Some(EntryState::Complete { .. })));

        cache.shutdown().await;
        drop(cache);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn trim_resets_complete_entry_and_preserves_identity_and_lyrics() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-cache-trim-{}", uuid::Uuid::new_v4()));
        let cache = AudioCache::spawn(AudioCacheConfig {
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_bytes: 1024,
            ..Default::default()
        })
        .await
        .unwrap();
        let key = SongKey::new("kugou", "trim-song").unwrap();
        let hash = cache_key_hash(&key);
        cache
            .rewrite(
                &key,
                None,
                StreamSource {
                    url: "http://127.0.0.1:1/unreachable.mp3".parse().unwrap(),
                    headers: Default::default(),
                    expires_at_epoch_ms: None,
                },
            )
            .await;
        let lyrics_json = b"lyrics";
        let complete_path = directory.join(cache_file_name(&hash, true));
        std::fs::write(&complete_path, vec![1u8; 2048]).unwrap();
        {
            let mut entries = cache.inner.entries.write().await;
            entries.get_mut(&hash).unwrap().state = EntryState::Complete { bytes: 2048 };
        }
        cache.inner.downloaded_bytes.store(2048, Ordering::SeqCst);
        {
            let store = lock_metadata_store(&cache.inner.metadata_store);
            store.mark_complete(&hash, 2048, true).unwrap();
            store.upsert_lyrics(&hash, lyrics_json).unwrap();
        }

        trim_cache(cache.inner.clone(), 1024).await;

        assert!(!complete_path.exists());
        assert!(matches!(
            cache.inner.entries.read().await.get(&hash).unwrap().state,
            EntryState::Idle
        ));
        {
            let store = lock_metadata_store(&cache.inner.metadata_store);
            let record = store.track(&hash).unwrap().unwrap();
            assert_eq!(record.source.as_deref(), Some("kugou"));
            assert!(!record.complete);
            // 歌词正文存于 SQLite，容量淘汰不影响。
            assert_eq!(
                store.get_lyrics_json(&hash).unwrap().as_deref(),
                Some(lyrics_json.as_slice())
            );
        }
        cache.shutdown().await;
        drop(cache);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn registry_limit_never_evicts_downloading_entries() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-cache-registry-{}", uuid::Uuid::new_v4()));
        let cache = AudioCache::spawn(AudioCacheConfig {
            directory: directory.clone(),
            metadata_directory: directory.join("index"),
            max_registry_entries: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        let first = SongKey::new("kugou", "downloading").unwrap();
        let first_hash = cache_key_hash(&first);
        let source = StreamSource {
            url: "http://127.0.0.1:1/audio".parse().unwrap(),
            headers: Default::default(),
            expires_at_epoch_ms: None,
        };
        cache.rewrite(&first, None, source.clone()).await;
        {
            let mut entries = cache.inner.entries.write().await;
            entries.get_mut(&first_hash).unwrap().state = EntryState::Downloading(DownloadHandle {
                token: Arc::new(()),
                ready: Arc::new(Notify::new()),
                bytes: Arc::new(AtomicUsize::new(0)),
                failed: Arc::new(AtomicBool::new(false)),
                finished: Arc::new(AtomicBool::new(false)),
            });
        }

        let second = SongKey::new("kugou", "idle").unwrap();
        cache.rewrite(&second, None, source).await;
        let entries = cache.inner.entries.read().await;
        assert!(entries.contains_key(&first_hash));
        assert_eq!(entries.len(), 1, "应优先淘汰非下载条目");
        drop(entries);
        cache.shutdown().await;
        drop(cache);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
