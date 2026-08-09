//! 源站流式下载：把音频数据写入 `.part` 文件，完成时改名 `.complete`。
//!
//! 下载进度通过 `Notify` 广播，代理服务器据此把已下载部分喂给解码器。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::AsyncWriteExt;

use crate::cache::{
    CacheInner, DownloadHandle, EntryState, complete_file_size, trim_cache,
};

/// 开始下载；幂等：已下载中/已完成则直接返回现有句柄。
pub(crate) async fn start_download(inner: &Arc<CacheInner>, hash: &str) -> Option<DownloadView> {
    loop {
        let mut entries = inner.entries.write().await;
        let entry = entries.get_mut(hash)?;
        match &entry.state {
            EntryState::Complete { bytes } => {
                entry.last_used = std::time::Instant::now();
                return Some(DownloadView::Complete { bytes: *bytes });
            }
            EntryState::Downloading(handle) => {
                entry.last_used = std::time::Instant::now();
                return Some(DownloadView::InProgress(handle.clone()));
            }
            EntryState::Idle => {
                // 先占位为 Downloading，再释放锁启动任务，防止并发重复下载。
                let handle = DownloadHandle {
                    ready: Arc::new(tokio::sync::Notify::new()),
                    bytes: Arc::new(AtomicUsize::new(0)),
                    failed: Arc::new(AtomicBool::new(false)),
                    finished: Arc::new(AtomicBool::new(false)),
                };
                entry.state = EntryState::Downloading(handle.clone());
                let source = entry.source.clone();
                let file = entry.file.clone();
                let hash = hash.to_owned();
                drop(entries);
                let inner = inner.clone();
                tokio::spawn(async move {
                    run_download(inner.clone(), hash, source, file).await;
                });
                return Some(DownloadView::InProgress(handle));
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum DownloadView {
    InProgress(DownloadHandle),
    Complete { bytes: u64 },
}

/// 等待下载推进到 `at_least` 字节。
///
/// 返回 true 表示「有进展或下载已结束」（调用方需复查注册表状态）；
/// 返回 false 表示已失败或超时。
/// 先创建 `notified()` future 再检查条件，避免「检查后、await 前」错过最后一次
/// notify 导致挂到超时（lost wakeup）。
pub(crate) async fn wait_for_bytes(
    handle: &DownloadHandle,
    at_least: usize,
    timeout: std::time::Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            let notified = handle.ready.notified();
            if handle.failed.load(Ordering::SeqCst) {
                return false;
            }
            if handle.bytes.load(Ordering::SeqCst) >= at_least
                || handle.finished.load(Ordering::SeqCst)
            {
                return true;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or(false)
}

async fn run_download(
    inner: Arc<CacheInner>,
    hash: String,
    source: crate::domain::StreamSource,
    part_file: PathBuf,
) {
    // 并发下载槽位：占满时排队等待，保证配置的并发上限生效。
    // 信号量只在进程退出时关闭，acquire 失败视为放弃本次下载。
    let _permit = match inner.download_slots.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return,
    };
    inner.active_downloads.fetch_add(1, Ordering::SeqCst);
    let result = download_to_file(&inner, &hash, &source, &part_file).await;
    inner.active_downloads.fetch_sub(1, Ordering::SeqCst);

    // 更新注册表状态（先取句柄，避免借用冲突）。
    let max_bytes = inner.config.max_bytes;
    let (notify, handle, completed) = {
        let mut entries = inner.entries.write().await;
        // 以下两个提前返回实际不可达：下载中条目 last_used 最新不会被淘汰，
        // 幂等占位保证本任务执行期间状态恒为 Downloading；此处清理残留文件兜底。
        let Some(entry) = entries.get_mut(&hash) else {
            let _ = std::fs::remove_file(&part_file);
            return;
        };
        let EntryState::Downloading(handle) = &entry.state else {
            return;
        };
        let handle = handle.clone();
        match result {
            Ok(()) => {
                let size = complete_file_size(&inner.config.directory, &hash).unwrap_or(0);
                entry.state = EntryState::Complete { bytes: size };
                entry.last_used = std::time::Instant::now();
                inner
                    .downloaded_bytes
                    .fetch_add(size as usize, Ordering::SeqCst);
                (handle.ready.clone(), handle, true)
            }
            Err(error) => {
                log::warn!("音频缓存下载失败，等待下次请求重试: {error}");
                let _ = std::fs::remove_file(&part_file);
                entry.state = EntryState::Idle;
                handle.failed.store(true, Ordering::SeqCst);
                (handle.ready.clone(), handle, false)
            }
        }
    };
    // 标记下载结束并唤醒所有等待方（最后一块数据后不再有推进通知）。
    handle.finished.store(true, Ordering::SeqCst);
    notify.notify_waiters();
    // 完成一个完整文件后做一次容量回收。
    if completed {
        trim_cache(inner.clone(), max_bytes).await;
    }
}

async fn download_to_file(
    inner: &Arc<CacheInner>,
    hash: &str,
    source: &crate::domain::StreamSource,
    part_file: &std::path::Path,
) -> Result<(), String> {
    let mut request = inner
        .client
        .get(source.url.clone())
        .timeout(inner.config.request_timeout);
    for (name, value) in &source.headers {
        request = request.header(name, value);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| format!("源站请求失败: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("源站返回 HTTP {}", response.status()));
    }
    let mut file = tokio::fs::File::create(part_file)
        .await
        .map_err(|error| format!("创建缓存文件失败: {error}"))?;
    let mut written = 0usize;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("读取源站响应流失败: {error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("写入缓存文件失败: {error}"))?;
        written += chunk.len();
        let mut entries = inner.entries.write().await;
        if let Some(entry) = entries.get_mut(hash)
            && let EntryState::Downloading(handle) = &entry.state
        {
            handle.bytes.store(written, Ordering::SeqCst);
            handle.ready.notify_waiters();
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("刷新缓存文件失败: {error}"))?;
    drop(file);
    // 半成品改名为完整文件。
    let complete_path = part_file.with_extension("complete");
    std::fs::rename(part_file, &complete_path)
        .map_err(|error| format!("缓存文件落盘失败: {error}"))?;
    Ok(())
}
