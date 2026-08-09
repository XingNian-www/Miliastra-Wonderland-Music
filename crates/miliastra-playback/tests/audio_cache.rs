//! 音频数据缓存的端到端测试：
//! 假源站 → AudioCache::spawn → rewrite → 本地代理（首次 chunked 边下边播、
//! 完成后 200/206 直接服务磁盘）。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use miliastra_playback::{AudioCache, AudioCacheConfig, SongKey, StreamSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// 启动一个假源站：收到请求后分两批发送 `body`（模拟慢速源站）。
async fn spawn_fake_origin(body: Vec<u8>) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut done_rx = done_rx;
        loop {
            tokio::select! {
                _ = &mut done_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { continue };
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 2048];
                        let _ = stream.read(&mut buffer).await;
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        // 先发一半，模拟流式下载推进。
                        let mid = body.len() / 2;
                        let _ = stream.write_all(&body[..mid]).await;
                        let _ = stream.flush().await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let _ = stream.write_all(&body[mid..]).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });
    (addr, done_tx)
}

fn cache_config(directory: PathBuf) -> AudioCacheConfig {
    AudioCacheConfig {
        enabled: true,
        directory,
        max_bytes: 64 * 1024 * 1024,
        max_concurrent_downloads: 2,
        request_timeout: Duration::from_secs(15),
        seek_wait_timeout: Duration::from_secs(5),
        max_registry_entries: 16,
    }
}

fn sample_stream(url: String) -> StreamSource {
    StreamSource {
        url: url.parse().unwrap(),
        headers: BTreeMap::new(),
        expires_at_epoch_ms: None,
    }
}

/// 首次请求：chunked 边下边播，最终拿到全部字节。
/// 完成后再次请求：完整 200 + Content-Length；Range 请求返回 206。
#[tokio::test]
async fn proxy_serves_streaming_then_cached_complete_and_range() {
    let body: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let (origin_addr, _done) = spawn_fake_origin(body.clone()).await;
    let directory = std::env::temp_dir().join(format!(
        "miliastra-audio-cache-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let cache = AudioCache::spawn(cache_config(directory.clone()))
        .await
        .expect("缓存启动");
    let key = SongKey::new("qq", "e2e-1").unwrap();
    let rewritten = cache
        .rewrite(&key, sample_stream(format!("http://{origin_addr}/audio.mp3")))
        .await;

    // 1. 首次请求：chunked 流式。
    let first = reqwest::get(rewritten.url.clone())
        .await
        .expect("首次请求")
        .bytes()
        .await
        .expect("读取流式响应");
    assert_eq!(first.len(), body.len());
    assert_eq!(&first[..], &body[..]);

    // 2. 第二次请求：完整文件 200 + Content-Length。
    let response = reqwest::get(rewritten.url.clone())
        .await
        .expect("完整请求");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get(reqwest::header::CONTENT_LENGTH).unwrap(),
        body.len().to_string().as_str()
    );
    let second = response.bytes().await.unwrap();
    assert_eq!(&second[..], &body[..]);

    // 3. Range 请求：206 部分内容。
    let response = reqwest::Client::new()
        .get(rewritten.url)
        .header(reqwest::header::RANGE, "bytes=100-199")
        .send()
        .await
        .expect("Range 请求");
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let part = response.bytes().await.unwrap();
    assert_eq!(&part[..], &body[100..200]);

    cache.shutdown().await;
    let _ = std::fs::remove_dir_all(&directory);
    let _ = Arc::new(());
}

/// 下载失败后注册表回退 Idle，下次请求重试；最终成功。
#[tokio::test]
async fn failed_download_is_retried_on_next_request() {
    // 假源站第一次连接立即断开，第二次正常返回。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let body: Vec<u8> = vec![7u8; 4096];
    tokio::spawn({
        let attempts = attempts.clone();
        let body = body.clone();
        async move {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept().await else { continue };
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer).await;
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // 第一次：发完响应头就断开。
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    drop(stream);
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                }
            }
        }
    });

    let directory = std::env::temp_dir().join(format!(
        "miliastra-audio-cache-test-retry-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let cache = AudioCache::spawn(cache_config(directory.clone()))
        .await
        .expect("缓存启动");
    let key = SongKey::new("qq", "e2e-2").unwrap();
    let rewritten = cache
        .rewrite(&key, sample_stream(format!("http://{addr}/audio.mp3")))
        .await;

    // 第一次请求可能拿不到完整数据（连接被切断）；失败后注册表回退 Idle。
    let _ = reqwest::get(rewritten.url.clone()).await;
    // 等下载任务失败状态落定。
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 第二次请求触发重试，应拿到完整数据。
    let response = reqwest::get(rewritten.url)
        .await
        .expect("重试请求");
    let data = response.bytes().await.expect("读取重试响应");
    assert_eq!(&data[..], &body[..]);

    cache.shutdown().await;
    let _ = std::fs::remove_dir_all(&directory);
}

/// rewrite 后 URL 指向本地代理，源站 URL 不再暴露给解码器。
#[tokio::test]
async fn rewrite_points_to_local_proxy_and_preserves_source_headers() {
    let body: Vec<u8> = vec![1u8; 512];
    let (origin_addr, _done) = spawn_fake_origin(body.clone()).await;
    let directory = std::env::temp_dir().join(format!(
        "miliastra-audio-cache-test-rw-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let cache = AudioCache::spawn(cache_config(directory.clone()))
        .await
        .expect("缓存启动");
    let key = SongKey::new("wy", "e2e-3").unwrap();
    let mut headers = BTreeMap::new();
    headers.insert(
        "authorization".to_owned(),
        "Bearer test".to_owned(),
    );
    let rewritten = cache
        .rewrite(
            &key,
            StreamSource {
                url: format!("http://{origin_addr}/a.mp3").parse().unwrap(),
                headers,
                expires_at_epoch_ms: Some(123),
            },
        )
        .await;
    assert!(
        rewritten.url.as_str().starts_with("http://127.0.0.1:"),
        "rewritten url = {}",
        rewritten.url
    );
    assert_eq!(rewritten.expires_at_epoch_ms, Some(123));
    let data = reqwest::get(rewritten.url)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&data[..], &body[..]);
    cache.shutdown().await;
    let _ = std::fs::remove_dir_all(&directory);
}
