//! WebSocket 实现：连接 `chat-api` 的 `/stream`，把推送转成事件。

use std::time::Duration;

use anyhow::{Context, Result};
use miliastra_kernel::chat_source::{ChatSource, ChatSourceEvent};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// 鉴权头名称，与 BOT 自身的 HTTP 服务保持一致。
const TOKEN_HEADER: &str = "x-miliastra-token";

/// 单次读取的超时：服务端 30 秒一次 Ping，这里留足余量。
///
/// 超时不是错误——它只是让调用方能周期性检查"是否该换成 OCR 了"。
const READ_TIMEOUT: Duration = Duration::from_secs(45);

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
        request
            .headers_mut()
            .insert(TOKEN_HEADER, value);
        let (stream, _response) = self
            .runtime
            .block_on(async { tokio_tungstenite::connect_async(request).await })
            .context("连接外部聊天 WS 失败")?;
        self.connection = Some(stream);
        log::info!("外部聊天 WS 已连接: {}", self.url);
        Ok(())
    }
}

/// 读取一条完整的 WS 消息。连接正常关闭返回 `None`，异常转为错误。
async fn read_message<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<Option<Message>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Error as WsError;
    match stream.next().await {
        None => Ok(None),
        Some(Ok(message)) => Ok(Some(message)),
        Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => Ok(None),
        Some(Err(error)) => Err(anyhow::anyhow!(error.to_string())),
    }
}

impl ChatSource for WebSocketChatSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn next_event(&mut self) -> Result<Option<ChatSourceEvent>> {
        if self.connection.is_none() {
            self.connect()?;
        }
        let stream = self.connection.as_mut().expect("刚刚连接");
        let read: Result<Option<Message>> = self.runtime.block_on(async {
            match tokio::time::timeout(READ_TIMEOUT, read_message(stream)).await {
                Ok(Ok(message)) => Ok(message),
                Ok(Err(error)) => Err(error),
                // 超时不是错误：返回 None，让上层有机会检查是否该回落 OCR。
                Err(_) => Ok(None),
            }
        });
        match read {
            Ok(Some(Message::Text(text))) => super::parse_frame(text.as_str()),
            Ok(Some(Message::Close(_))) => {
                log::info!("外部聊天 WS 被对端关闭");
                self.connection = None;
                Ok(None)
            }
            Ok(Some(Message::Ping(_))) | Ok(Some(Message::Pong(_))) => Ok(None),
            Ok(Some(_)) => Ok(None),
            Ok(None) => {
                self.connection = None;
                Ok(None)
            }
            Err(error) => {
                // 读失败视为连接已断：交给上层重连，不要在这里退避。
                log::warn!("外部聊天 WS 读取出错: {error}");
                self.connection = None;
                Err(error)
            }
        }
    }
}
