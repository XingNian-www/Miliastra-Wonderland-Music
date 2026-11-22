//! 本地 HTTP 代理服务器：把缓存内容以 HTTP 流喂给解码器。
//!
//! - 完整文件：标准 `200`/`206` + Content-Length，支持 Range。
//! - 下载中：先确认下载能产出首块数据，再回 `200` + chunked 边下边播；
//!   等待窗口内拿不到数据（槽位饱和/源站失败）时回 `503` + `Retry-After`，
//!   绝不先写 `200` 再让客户端等到超时断流。非零 Range 返回 `416`。
//! - 仅监听 127.0.0.1，只处理 GET `/audio/{hash}`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::cache::{
    CacheError, CacheInner, DownloadHandle, EntryState, PROXY_PATH_PREFIX,
    download::{DownloadPriority, DownloadView, start_download, wait_for_bytes},
};

/// 单次响应读文件/写 socket 的块大小。
const IO_CHUNK_SIZE: usize = 64 * 1024;
/// 请求头总量上限（防滥用）。
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
/// 下载无进展时关闭连接的最长等待。
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 读取请求头的超时:慢速/挂起的本地连接不应无限占用任务。
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 响应写入超时：慢/停读客户端不应无限挂住连接任务。
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// 点播下载失效后重新申请槽位的最多次数（防止失败源站导致忙重试）。
const MAX_ON_DEMAND_REACQUIRE: u8 = 2;

pub(crate) async fn spawn_proxy_server(
    inner: Arc<CacheInner>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<u16, CacheError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| CacheError::Io(format!("绑定本地代理端口失败: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| CacheError::Io(format!("读取本地代理端口失败: {error}")))?
        .port();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        continue;
                    };
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(inner, stream).await;
                    });
                }
            }
        }
    });
    Ok(port)
}

struct RequestHead {
    method: String,
    path: String,
    range_start: Option<u64>,
    range_end: Option<u64>,
}

async fn read_request_head(stream: &mut TcpStream) -> std::io::Result<Option<RequestHead>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    let header_end;
    loop {
        let read = tokio::time::timeout(HEADER_READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "读取代理请求头超时")
            })??;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_HEADER_BYTES {
            return Ok(None);
        }
        if let Some(index) = find_header_end(&buffer) {
            header_end = index;
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut range_start = None;
    let mut range_end = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("range:")
            && let Some(rest) = value.trim().strip_prefix("bytes=")
            && let Some(dash) = rest.find('-')
        {
            let start = rest[..dash].trim();
            let end = rest[dash + 1..].trim();
            if !start.is_empty() {
                range_start = start.parse::<u64>().ok();
            }
            if !end.is_empty() {
                range_end = end.parse::<u64>().ok();
            }
        }
    }
    Ok(Some(RequestHead {
        method,
        path,
        range_start,
        range_end,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

async fn handle_connection(inner: Arc<CacheInner>, mut stream: TcpStream) -> std::io::Result<()> {
    let Some(head) = read_request_head(&mut stream).await? else {
        return Ok(());
    };
    if head.method != "GET" {
        write_simple(&mut stream, "405 Method Not Allowed", "").await?;
        return Ok(());
    }
    let Some(hash) = path_hash(&head.path) else {
        write_simple(&mut stream, "404 Not Found", "").await?;
        return Ok(());
    };
    let Some(view) = start_download(&inner, &hash, DownloadPriority::OnDemand).await else {
        write_simple(&mut stream, "404 Not Found", "").await?;
        return Ok(());
    };
    match view {
        DownloadView::Complete { bytes } => {
            let store = inner.metadata_store.clone();
            let hit_hash = hash.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let store = super::lock_metadata_store(&store);
                if let Err(error) = store.record_cache_hit(&hit_hash) {
                    log::warn!("音频缓存命中统计写入失败: {error}");
                }
            })
            .await;
            serve_complete(
                &inner,
                &mut stream,
                &hash,
                bytes,
                head.range_start,
                head.range_end,
            )
            .await?;
        }
        DownloadView::InProgress(handle) => {
            serve_streaming(
                &inner,
                &mut stream,
                &hash,
                handle,
                head.range_start,
                head.range_end,
            )
            .await?;
        }
    }
    Ok(())
}

/// 从 `/audio/{hash}` 路径中提取 hash。
fn path_hash(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path.strip_prefix(PROXY_PATH_PREFIX)?;
    if rest.is_empty() || !rest.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-') {
        return None;
    }
    Some(rest.to_owned())
}

async fn write_simple(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// 下载尚未产出数据（槽位饱和/源站失败）时在写出响应头之前返回 503。
///
/// `Retry-After` 让客户端/上层可以把这当作可重试的暂时不可用，而不是
/// 「已开始的 200 流被截断」——后者会被解码器判成 `UnexpectedEof`。
async fn write_unavailable(stream: &mut TcpStream) -> std::io::Result<()> {
    let body = "audio cache download is not ready";
    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nRetry-After: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// Range 不满足时返回 416 并携带 Content-Range 头。
async fn write_range_not_satisfiable(
    stream: &mut TcpStream,
    total: Option<u64>,
) -> std::io::Result<()> {
    let range = match total {
        Some(total) => format!("bytes */{total}"),
        None => "bytes */*".to_owned(),
    };
    let response = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain\r\nContent-Range: {range}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await
}

/// 服务完整文件（支持 Range）。
async fn serve_complete(
    inner: &CacheInner,
    stream: &mut TcpStream,
    hash: &str,
    total: u64,
    range_start: Option<u64>,
    range_end: Option<u64>,
) -> std::io::Result<()> {
    let path = inner
        .config
        .directory
        .join(crate::cache::cache_file_name(hash, true));
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            write_simple(stream, "404 Not Found", "").await?;
            return Ok(());
        }
    };
    match range_start {
        Some(start) if start >= total => {
            write_range_not_satisfiable(stream, Some(total)).await?;
        }
        Some(start) => {
            // Range 结束位置：显式 end 或文件末尾；end 小于 start 时按 416 处理。
            let end = match range_end {
                Some(end) if end < start => {
                    write_range_not_satisfiable(stream, Some(total)).await?;
                    return Ok(());
                }
                Some(end) => end.min(total - 1),
                None => total - 1,
            };
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let length = end - start + 1;
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n"
            );
            write_all_with_timeout(stream, head.as_bytes()).await?;
            copy_file_to_stream(&mut file, stream, length).await?;
        }
        None => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
            );
            write_all_with_timeout(stream, head.as_bytes()).await?;
            copy_file_to_stream(&mut file, stream, total).await?;
        }
    }
    Ok(())
}

async fn write_all_with_timeout(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "写入代理响应超时，客户端停止读取",
            )
        })?
}

async fn copy_file_to_stream(
    file: &mut tokio::fs::File,
    stream: &mut TcpStream,
    mut remaining: u64,
) -> std::io::Result<()> {
    let mut buffer = vec![0u8; IO_CHUNK_SIZE];
    while remaining > 0 {
        let to_read = remaining.min(IO_CHUNK_SIZE as u64) as usize;
        let read = file.read(&mut buffer[..to_read]).await?;
        if read == 0 {
            break;
        }
        write_all_with_timeout(stream, &buffer[..read]).await?;
        remaining -= read as u64;
    }
    Ok(())
}

/// 响应头已按 chunked 发出后，从完整文件的 `start` 偏移以 chunked 续传剩余数据。
/// 供「下载刚完成、.part 已改名」的竞态窗口使用，避免发出第二个响应头。
async fn serve_complete_chunked(
    inner: &CacheInner,
    stream: &mut TcpStream,
    hash: &str,
    start: u64,
) -> std::io::Result<()> {
    let Some(total) = crate::cache::complete_file_size(&inner.config.directory, hash) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "缓存下载未完成，不能结束代理响应",
        ));
    };
    let mut complete = match tokio::fs::File::open(
        inner
            .config
            .directory
            .join(crate::cache::cache_file_name(hash, true)),
    )
    .await
    {
        Ok(file) => file,
        Err(error) => return Err(error),
    };
    complete.seek(std::io::SeekFrom::Start(start)).await?;
    let mut remaining = total.saturating_sub(start);
    let mut buffer = vec![0u8; IO_CHUNK_SIZE];
    while remaining > 0 {
        let to_read = remaining.min(IO_CHUNK_SIZE as u64) as usize;
        let read = complete.read(&mut buffer[..to_read]).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "完整缓存文件意外结束，代理响应不完整",
            ));
        }
        write_all_with_timeout(stream, format!("{:x}\r\n", read).as_bytes()).await?;
        write_all_with_timeout(stream, &buffer[..read]).await?;
        write_all_with_timeout(stream, b"\r\n").await?;
        remaining -= read as u64;
    }
    write_chunk_end(stream).await?;
    Ok(())
}

/// 首块数据就绪情况。
enum StreamReadiness {
    /// 已有字节可读（可能仍在下载）。
    Ready(DownloadHandle),
    /// 下载已整体完成，直接从完整文件服务。
    Complete(u64),
    /// 源站失败：不写响应头，由调用方以连接错误表达（绝不伪装成干净的 chunked EOF）。
    SourceFailed,
    /// 等待窗口内没有产出数据（槽位饱和/排队）：写出响应头之前返回 503。
    Unavailable,
}

/// 在写出 `200` 之前等待下载产出首块数据。
///
/// 代理一旦发出 `200 + chunked` 就只能靠闭连接表达失败，客户端会看到
/// `UnexpectedEof`。因此这里先把「能不能开始流」判定完：
/// - 已有字节 → 立即开始；
/// - 下载完成 → 按完整文件服务（带 Content-Length）；
/// - 下载被放弃（预取让位）/失败 → 以点播优先级重新申请槽位；
/// - 超过 `seek_wait_timeout` 仍无数据 → 交给调用方返回 503。
async fn await_stream_ready(
    inner: &Arc<CacheInner>,
    hash: &str,
    mut handle: DownloadHandle,
) -> StreamReadiness {
    let deadline = Instant::now() + (inner.limits)().seek_wait_timeout;
    let mut reacquired = 0_u8;
    loop {
        if handle.bytes.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            return StreamReadiness::Ready(handle);
        }
        if handle.failed.load(std::sync::atomic::Ordering::SeqCst) {
            // 源站确实失败：保持既有语义——不写响应头，让客户端看到明确的连接错误。
            return StreamReadiness::SourceFailed;
        }
        if handle.finished.load(std::sync::atomic::Ordering::SeqCst) {
            // 下载在产出数据前被放弃（预取让位/信号量关闭）：条目已回到 Idle，
            // 按点播优先级重新申请，保证点播不会等待一个不会启动的下载任务。
            reacquired = reacquired.saturating_add(1);
            if reacquired > MAX_ON_DEMAND_REACQUIRE {
                return StreamReadiness::Unavailable;
            }
            match start_download(inner, hash, DownloadPriority::OnDemand).await {
                Some(DownloadView::Complete { bytes }) => return StreamReadiness::Complete(bytes),
                Some(DownloadView::InProgress(next)) => {
                    handle = next;
                    continue;
                }
                None => return StreamReadiness::SourceFailed,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return StreamReadiness::Unavailable;
        }
        // 有进展/失败/结束都会唤醒；超时后回到循环顶部复查状态。
        let _ = wait_for_bytes(&handle, 1, remaining).await;
        if Instant::now() >= deadline
            && handle.bytes.load(std::sync::atomic::Ordering::SeqCst) == 0
            && !handle.failed.load(std::sync::atomic::Ordering::SeqCst)
            && !handle.finished.load(std::sync::atomic::Ordering::SeqCst)
        {
            // 排队等不到槽位（不是源站失败）：明确 503，而不是先 200 再断流。
            return StreamReadiness::Unavailable;
        }
    }
}

/// 边下边播：chunked 传输。`range_start` 未下载到位时等待下载推进。
async fn serve_streaming(
    inner: &Arc<CacheInner>,
    stream: &mut TcpStream,
    hash: &str,
    handle: DownloadHandle,
    range_start: Option<u64>,
    _range_end: Option<u64>,
) -> std::io::Result<()> {
    let start = range_start.unwrap_or(0);
    // 未完成流无法给出总长和合法 Content-Range；绝不能返回 200 却从非零偏移发送。
    if start > 0 {
        write_range_not_satisfiable(stream, None).await?;
        return Ok(());
    }
    // 先判定能否开始流，再写响应头：槽位饱和时明确 503，而不是 200 后断流。
    let handle = match await_stream_ready(inner, hash, handle).await {
        StreamReadiness::Ready(handle) => handle,
        StreamReadiness::Complete(bytes) => {
            return serve_complete(inner, stream, hash, bytes, None, None).await;
        }
        StreamReadiness::Unavailable => {
            log::debug!("缓存下载在等待窗口内没有产出数据，代理返回 503: {hash}");
            return write_unavailable(stream).await;
        }
        StreamReadiness::SourceFailed => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "缓存下载在产出数据前失败，代理响应不完整",
            ));
        }
    };
    let part_path = inner
        .config
        .directory
        .join(crate::cache::cache_file_name(hash, false));
    let mut file = match tokio::fs::File::open(&part_path).await {
        Ok(file) => file,
        Err(_) => {
            // 首块数据之后下载刚好完成并改名：尚未写响应头，按完整文件服务。
            let Some(total) = crate::cache::complete_file_size(&inner.config.directory, hash)
            else {
                return write_unavailable(stream).await;
            };
            return serve_complete(inner, stream, hash, total, None, None).await;
        }
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    write_all_with_timeout(stream, head.as_bytes()).await?;

    let mut next_read_offset = 0_usize;
    let mut buffer = vec![0u8; IO_CHUNK_SIZE];
    loop {
        // 读当前已下载的部分。
        let available = handle.bytes.load(std::sync::atomic::Ordering::SeqCst);
        if available > next_read_offset {
            let to_read = (available - next_read_offset).min(IO_CHUNK_SIZE);
            let read = file.read(&mut buffer[..to_read]).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "缓存临时文件意外结束，代理响应不完整",
                ));
            }
            write_all_with_timeout(stream, format!("{:x}\r\n", read).as_bytes()).await?;
            write_all_with_timeout(stream, &buffer[..read]).await?;
            write_all_with_timeout(stream, b"\r\n").await?;
            next_read_offset += read;
            continue;
        }
        // 等待下载推进或完成。
        let progressed = wait_for_bytes(&handle, next_read_offset + 1, STALL_TIMEOUT).await;
        if !progressed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "缓存下载停滞或失败，代理响应不完整",
            ));
        }
        // 检查是否已变完整：改读完整文件。
        if handle.failed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "缓存下载失败，代理响应不完整",
            ));
        }
        if !matches!(
            inner
                .entries
                .read()
                .await
                .get(hash)
                .map(|entry| &entry.state),
            Some(EntryState::Downloading(_))
        ) {
            // 下载已完成：从完整文件续传剩余部分。
            return serve_complete_chunked(inner, stream, hash, next_read_offset as u64).await;
        }
    }
}

async fn write_chunk_end(stream: &mut TcpStream) -> std::io::Result<()> {
    write_all_with_timeout(stream, b"0\r\n\r\n").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hash_from_proxy_paths() {
        assert_eq!(path_hash("/audio/abc123").as_deref(), Some("abc123"));
        assert_eq!(path_hash("/audio/abc123?x=1").as_deref(), Some("abc123"));
        assert_eq!(path_hash("/audio/"), None);
        assert_eq!(path_hash("/other/abc"), None);
        assert_eq!(path_hash("/audio/ab/c"), None);
    }

    #[test]
    fn finds_header_end_marker() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(
            find_header_end(b"GET / HTTP/1.1\r\nA: b\r\n\r\nx"),
            Some(24)
        );
        assert_eq!(find_header_end(b"no marker"), None);
    }
}
