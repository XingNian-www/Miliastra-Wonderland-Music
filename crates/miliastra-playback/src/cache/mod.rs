//! 音频数据缓存：本地代理 + 磁盘缓存 + 流式下载。
//!
//! 播放前的音源 URL 被改写为本机代理地址；代理首次请求时向源站发起
//! 流式下载，边下载边以 chunked 传输喂给解码器，同时把数据写盘；
//! 下载完成后后续播放直接从本地文件服务（支持 Range），断网也能播。

mod download;
mod proxy;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Notify, RwLock, watch};

use crate::domain::{SongKey, StreamSource};

/// 代理 URL 的路径前缀。
pub(crate) const PROXY_PATH_PREFIX: &str = "/audio/";

/// 默认磁盘缓存上限（20 GiB）。
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AudioCacheConfig {
    pub enabled: bool,
    /// 缓存根目录；为空时使用系统临时目录。
    pub directory: PathBuf,
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
            max_bytes: DEFAULT_MAX_CACHE_BYTES,
            max_concurrent_downloads: 2,
            request_timeout: Duration::from_secs(15),
            seek_wait_timeout: Duration::from_secs(10),
            max_registry_entries: 16,
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
}

fn inner_concurrency(configured: usize) -> usize {
    if configured == 0 { 1 } else { configured }
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
        std::fs::create_dir_all(&directory)
            .map_err(|error| CacheError::Io(format!("创建音频缓存目录失败: {error}")))?;
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
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
        });
        let port = proxy::spawn_proxy_server(inner.clone(), shutdown_rx).await?;
        inner.port.store(port, Ordering::SeqCst);
        // 启动时清理上次运行残留的半成品，并回收超限磁盘。
        cleanup_partial_files(&inner)?;
        trim_cache(inner.clone(), inner.config.max_bytes).await;
        Ok(Self { inner })
    }

    /// 把音源改写为本机代理地址并注册缓存条目。
    pub async fn rewrite(&self, key: &SongKey, stream: StreamSource) -> StreamSource {
        let inner = &self.inner;
        if !inner.config.enabled {
            return stream;
        }
        let hash = cache_key_hash(key);
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
            while entries.len() > inner.config.max_registry_entries {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(hash, _)| hash.clone());
                if let Some(oldest) = oldest {
                    entries.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let local_url = format!(
            "http://127.0.0.1:{}{}{}",
            inner.port.load(Ordering::SeqCst),
            PROXY_PATH_PREFIX,
            hash
        )
        .parse()
        .expect("local proxy url is well-formed");
        StreamSource {
            url: local_url,
            headers: Default::default(),
            expires_at_epoch_ms: stream.expires_at_epoch_ms,
        }
    }

    /// 停止代理服务器（进程退出时调用）。
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown.send_replace(true);
    }

    /// 删除曲目的缓存条目与磁盘文件（播放解码失败后自愈：下次播放重新下载）。
    /// 下载进行中的任务会被标记失败并自行清理残留。
    pub async fn invalidate(&self, key: &SongKey) -> bool {
        let inner = &self.inner;
        if !inner.config.enabled {
            return false;
        }
        let hash = cache_key_hash(key);
        let mut entries = inner.entries.write().await;
        let Some(entry) = entries.remove(&hash) else {
            return false;
        };
        if let EntryState::Downloading(handle) = &entry.state {
            handle.failed.store(true, Ordering::SeqCst);
            handle.ready.notify_waiters();
        }
        let _ = std::fs::remove_file(&entry.file);
        let _ = std::fs::remove_file(entry.file.with_extension("complete"));
        true
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

    /// 读取曲目的缓存歌词（未命中或无歌词缓存返回 None）。
    pub async fn get_lyrics(&self, key: &SongKey) -> Option<crate::lyrics::TimedLyrics> {
        let inner = &self.inner;
        if !inner.config.enabled {
            return None;
        }
        let path = lyrics_file(&inner.config.directory, key);
        let bytes = tokio::fs::read(&path).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// 写入曲目的歌词缓存（跟随音频缓存的启用状态、目录与容量上限）。
    pub async fn put_lyrics(&self, key: &SongKey, lyrics: &crate::lyrics::TimedLyrics) -> bool {
        let inner = &self.inner;
        if !inner.config.enabled {
            return false;
        }
        let Ok(bytes) = serde_json::to_vec(lyrics) else {
            return false;
        };
        let directory = inner.config.directory.join("lyrics");
        if std::fs::create_dir_all(&directory).is_err() {
            return false;
        }
        let path = lyrics_file(&inner.config.directory, key);
        if tokio::fs::write(&path, bytes).await.is_err() {
            return false;
        }
        trim_lyrics_directory(
            &directory,
            inner.config.max_bytes,
            inner.config.max_registry_entries,
        );
        true
    }
}

/// 歌词缓存目录内的文件路径。
fn lyrics_file(directory: &Path, key: &SongKey) -> PathBuf {
    directory
        .join("lyrics")
        .join(format!("{}.json", cache_key_hash(key)))
}

/// 歌词缓存上限与音频缓存保持一致：
/// 字节上限 `max_bytes`、文件数量上限 `max_registry_entries`；
/// 任一超限时按修改时间删除最旧文件。
fn trim_lyrics_directory(directory: &Path, max_bytes: u64, max_entries: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return None;
            }
            let Ok(metadata) = path.metadata() else {
                return None;
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            Some((path, metadata.len(), modified))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    if files.len() <= max_entries && total <= max_bytes {
        return;
    }
    files.sort_by_key(|(_, _, modified)| *modified);
    let mut remaining = files.len();
    for (path, size, _) in files {
        let over_entries = remaining > max_entries;
        let over_bytes = total > max_bytes;
        if !over_entries && !over_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            remaining -= 1;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("audio cache io error: {0}")]
    Io(String),
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
async fn trim_cache(inner: Arc<CacheInner>, max_bytes: u64) {
    let mut total = inner.downloaded_bytes.load(Ordering::SeqCst) as u64;
    if total <= max_bytes {
        return;
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
    for (path, size, _) in complete_files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            inner
                .downloaded_bytes
                .fetch_sub(size as usize, Ordering::SeqCst);
        }
    }
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
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
