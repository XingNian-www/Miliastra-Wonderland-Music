//! WebSocket 实现：连接 `chat-api` 的 `/stream`，把推送转成事件。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use miliastra_kernel::chat_source::{ChatSource, ChatSourceEvent};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// 鉴权头名称，与 BOT 自身的 HTTP 服务保持一致。
const TOKEN_HEADER: &str = "x-miliastra-token";

/// 单次读取的超时：只是让调用方能周期性回到上层检查停止信号与健康状态。
///
/// 必须明显短于 `chat_source.lease_timeout_ms`（默认 10s），否则上层在两次
/// 返回之间得不到执行健康检查的机会。注意读超时本身**不**刷新接管租约：
/// WS 活着不代表游戏已接入，租约只由真实消息或 /health 的 gameConnected=true
/// 刷新。服务端 30 秒一次 Ping，3 秒的轮询不会打断它。
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// 读等待的分片长度：决定停止信号最多被拖多久才发现。
///
/// 单次 `READ_TIMEOUT` 是 3 秒，若只在它到期后才检查停止标志，关停要等满一整个
/// 分片——已连接但长时间没有消息正是关闭时的常态。切成 100 毫秒的小段后，
/// 停止信号最多延后一个分片，`join` 保持在亚秒级。
///
/// 分片只影响**检查频率**，不影响超时语义：累计到 `READ_TIMEOUT` 才判定为读超时，
/// 因此「超时」与「连接已关」仍然严格分开（把超时当关闭会导致每 3 秒重连一次）。
const READ_SLICE: Duration = Duration::from_millis(100);

pub(crate) struct WebSocketChatSource {
    name: String,
    url: String,
    token: String,
    runtime: tokio::runtime::Runtime,
    connection: Option<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    /// 上层线程的停止标志。Some 时读等待按 [`READ_SLICE`] 分片，每片之间问一次；
    /// None 时退化为原来的整段 3 秒等待（供不关心关停时序的调用方使用）。
    interrupt: Option<Arc<AtomicBool>>,
}

impl WebSocketChatSource {
    pub(crate) fn new(url: String, token: String) -> Result<Self> {
        // 独立 runtime：本线程是阻塞式的，不与主程序其它 tokio 使用竞争。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("创建外部聊天 WS runtime 失败")?;
        Ok(Self {
            name: "Miliastra-Wonderland-ChatBridge".to_string(),
            url,
            token,
            runtime,
            connection: None,
            interrupt: None,
        })
    }

    /// 挂上停止标志：置位后下一次读等待最多再等一个 [`READ_SLICE`] 就返回。
    ///
    /// 只影响「等待怎么被打断」，不改变任何事件语义——被打断时与读超时一样返回
    /// `Ok(None)`，连接保持不动，由调用方决定退出还是继续。
    pub(crate) fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = Some(flag);
    }

    fn stop_requested(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn connect(&mut self) -> Result<()> {
        let mut request = self
            .url
            .clone()
            .into_client_request()
            .with_context(|| format!("无效的外部聊天 WS 地址: {}", self.url))?;
        let value = HeaderValue::from_str(&self.token).context("访问令牌含非法字符")?;
        request.headers_mut().insert(TOKEN_HEADER, value);
        let (stream, _response) = self
            .runtime
            .block_on(async { tokio_tungstenite::connect_async(request).await })
            .context("连接外部聊天 WS 失败")?;
        self.connection = Some(stream);
        log::info!(
            "外部聊天源已连接，地址「{}」，开始接收大厅与好友消息",
            self.url
        );
        Ok(())
    }
}

impl ChatSource for WebSocketChatSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    fn next_event(&mut self) -> Result<Option<ChatSourceEvent>> {
        if self.connection.is_none() {
            self.connect()?;
        }
        let stream = self.connection.as_mut().expect("刚刚连接");
        // 三态：读到消息 / 读超时（连接仍活）/ 连接已结束。
        // 超时必须与关闭分开——把超时当成关闭会导致每 3 秒重连一次，
        // 每次重连都发 Connected，上层的"连上就切二级"就会反复把手动
        // 切换的一级顶回去，表现为"切了一级又自己变回二级"。
        enum Read {
            Message(Message),
            TimedOut,
            Closed,
        }
        let interrupt = self.interrupt.clone();
        let read: Result<Read> = self.runtime.block_on(async {
            // 分片等待：每片之间检查停止标志，累计到 READ_TIMEOUT 才算读超时。
            let started = tokio::time::Instant::now();
            loop {
                let slice = READ_TIMEOUT.saturating_sub(started.elapsed());
                if slice.is_zero() {
                    return Ok(Read::TimedOut);
                }
                let slice = slice.min(READ_SLICE);
                match tokio::time::timeout(slice, stream.next()).await {
                    // 本片没有消息：先看是否该停止，再决定是否继续等。
                    Err(_) => {
                        if interrupt
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            return Ok(Read::TimedOut);
                        }
                    }
                    // 流结束：对端真的关了。
                    Ok(None) => return Ok(Read::Closed),
                    Ok(Some(Ok(message))) => return Ok(Read::Message(message)),
                    Ok(Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed))) => {
                        return Ok(Read::Closed);
                    }
                    Ok(Some(Err(error))) => return Err(anyhow::anyhow!(error.to_string())),
                }
            }
        });
        match read {
            Ok(Read::Message(Message::Text(text))) => super::parse_frame(text.as_str()),
            Ok(Read::Message(Message::Close(_))) => {
                log::info!("外部聊天源连接被对端关闭，将按重连间隔重新建立");
                self.connection = None;
                Ok(None)
            }
            // 心跳只说明传输层活着，不产生业务事件，也不刷新接管租约
            // （WS 活着不等于游戏已接入；租约由真实消息或 /health 维护）。
            Ok(Read::Message(Message::Ping(_))) | Ok(Read::Message(Message::Pong(_))) => Ok(None),
            Ok(Read::Message(_)) => Ok(None),
            // 读超时：连接保持不动，让上层周期性检查租约与停止信号。
            Ok(Read::TimedOut) => Ok(None),
            Ok(Read::Closed) => {
                self.connection = None;
                Ok(None)
            }
            Err(error) => {
                // 读失败视为连接已断：交给上层重连，不要在这里退避。
                log::warn!(
                    "外部聊天源读取消息失败，本次连接已断开，等待重新连接；断开期间的外部聊天消息不会被接收：{error}"
                );
                self.connection = None;
                Err(error)
            }
        }
    }
}
