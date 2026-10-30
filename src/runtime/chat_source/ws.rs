//! WebSocket 实现：连接 `chat-api` 的 `/stream`，把推送转成事件。

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

/// 单次读取的超时：只是让调用方能周期性回到上层检查租约与停止信号。
///
/// 必须明显短于 `chat_source.lease_timeout_ms`（默认 10s）：读超时返回
/// `Ok(None)` 而连接仍在，上层据此刷新租约；若这里设得比租约还长，
/// 上层在两次返回之间根本得不到机会，租约也只能靠消息到达来维持。
/// 服务端 30 秒一次 Ping，3 秒的轮询不会打断它。
const READ_TIMEOUT: Duration = Duration::from_secs(3);

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
        })
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
        log::info!("外部聊天源已连接，地址「{}」，开始接收大厅与好友消息", self.url);
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
        let read: Result<Read> = self.runtime.block_on(async {
            match tokio::time::timeout(READ_TIMEOUT, stream.next()).await {
                // 流结束：对端真的关了。
                Err(_) => Ok(Read::TimedOut),
                Ok(None) => Ok(Read::Closed),
                Ok(Some(Ok(message))) => Ok(Read::Message(message)),
                Ok(Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed))) => {
                    Ok(Read::Closed)
                }
                Ok(Some(Err(error))) => Err(anyhow::anyhow!(error.to_string())),
            }
        });
        match read {
            Ok(Read::Message(Message::Text(text))) => super::parse_frame(text.as_str()),
            Ok(Read::Message(Message::Close(_))) => {
                log::info!("外部聊天源连接被对端关闭，将按重连间隔重新建立");
                self.connection = None;
                Ok(None)
            }
            // 心跳只说明连接活着，不产生业务事件；调用方按"连接仍在"刷新租约。
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
                    "外部聊天源读取消息失败，本次连接已断开，等待重新连接；断开期间的外部聊天消息不会被接收: {error}"
                );
                self.connection = None;
                Err(error)
            }
        }
    }
}
