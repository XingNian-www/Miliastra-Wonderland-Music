//! 外部聊天源的传输实现。
//!
//! 本模块只负责"把远端推送变成 [`ChatSourceEvent`]"，不含命令语义、
//! 不碰时间线。新增一种来源（SSE、长轮询、其他游戏）只需再加一个子模块。

pub(crate) mod launcher;
pub(crate) mod ws;

use miliastra_kernel::chat_source::{
    ChatChannel, ChatSourceEvent, ExternalChatMessage, ExternalSender, GameUid,
};

/// 一条 WS 推送的原始 JSON。
///
/// 字段名与 `Miliastra-Wonderland-ChatBridge/docs/API.md` 的 `ChatMessage` 一致。
#[derive(serde::Deserialize)]
struct WireMessage {
    id: u64,
    #[serde(default)]
    ts: Option<u64>,
    channel: String,
    #[serde(rename = "hallId", default)]
    hall_id: Option<u64>,
    #[serde(rename = "hallType", default)]
    hall_type: Option<u32>,
    from: WireSender,
    #[serde(default)]
    to: Option<u64>,
    text: String,
}

#[derive(serde::Deserialize)]
struct WireSender {
    uid: u64,
    #[serde(default)]
    name: Option<String>,
}

/// WS 帧的类型外壳。
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    Hello {
        #[serde(rename = "lastMessageId")]
        last_message_id: u64,
    },
    Message {
        message: WireMessage,
    },
    HallChanged {
        #[serde(rename = "hallId")]
        hall_id: u64,
        #[serde(rename = "hallType", default)]
        hall_type: Option<u32>,
    },
}

/// 解析一帧 JSON，转成内部事件。
///
/// 未知 `type` 返回 `None`：服务端将来加新事件类型时，旧客户端应忽略而不是报错。
pub(crate) fn parse_frame(text: &str) -> anyhow::Result<Option<ChatSourceEvent>> {
    let event: WireEvent = match serde_json::from_str(text) {
        Ok(event) => event,
        Err(error) => {
            // 协议演进时可能出现未知 type，serde 会报 missing variant；
            // 这里降级为"忽略"而不是让整条连接失败。
            // 不打印原始帧：帧体里含玩家昵称、会话 ID 与聊天正文。
            // 只记长度，够判断「是不是整帧都没解析出来」。
            log::debug!(
                "收到无法解析的外部聊天帧，已忽略并继续读取下一帧；帧长 {} 字节: {error}",
                text.len()
            );
            return Ok(None);
        }
    };
    Ok(Some(match event {
        WireEvent::Hello { last_message_id } => ChatSourceEvent::Connected {
            last_sequence: last_message_id,
        },
        WireEvent::Message { message } => ChatSourceEvent::Messages(vec![to_external(message)]),
        WireEvent::HallChanged { hall_id, hall_type } => {
            ChatSourceEvent::HallChanged { hall_id, hall_type }
        }
    }))
}

/// 从 JSON 值解析单条消息（补拉用：`/messages` 返回的是裸 ChatMessage 数组）。
fn parse_wire_message(value: &serde_json::Value) -> anyhow::Result<ExternalChatMessage> {
    let message: WireMessage = serde_json::from_value(value.clone())?;
    Ok(to_external(message))
}

fn to_external(message: WireMessage) -> ExternalChatMessage {
    ExternalChatMessage {
        sequence: message.id,
        sent_at: message.ts,
        channel: ChatChannel::new(message.channel),
        hall_id: message.hall_id,
        sender: ExternalSender::new(
            message.from.name,
            Some(GameUid::from_u32(message.from.uid as u32)),
        ),
        to_uid: message.to.map(|uid| GameUid::from_u32(uid as u32)),
        text: message.text,
    }
}

/// 昵称与备注补查：`from.name` 为 null 时调用 `GET /names?uids=`。
///
/// 文档 §5 明确说昵称到达时序不确定、且服务端**不会补发**，因此这条补查
/// 是拿到发送者的最后手段；查不到就按"未知发送者"处理，不授权任何权限。
///
/// 响应同时带 `remarks`（玩家给该 UID 设置的备注）。备注是用户在游戏里
/// 亲自命名的，比 OCR 猜出来的昵称可靠，身份映射优先用它；没有就返回 None，
/// 由调用方回退到昵称——不编造、不留空。
pub(crate) fn resolve_sender_names(
    base_http_url: &str,
    token: &str,
    uid: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    if uid.trim().is_empty() {
        return Ok((None, None));
    }
    let url = format!(
        "{}/names?uids={}",
        base_http_url.trim_end_matches('/'),
        urlencode(uid.trim())
    );
    // 1s 上限：这条查询由后台补查线程发起，慢一点只让昵称晚到；
    // 不能让它长时间占住线程，更不能回到接收线程上。
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()?;
    let response = client.get(url).header("x-miliastra-token", token).send()?;
    if !response.status().is_success() {
        // 503 game_not_connected 等都属于"暂时查不到"，不是致命错误。
        log::debug!("昵称补查未取到结果（HTTP 状态 {}），本次沿用已有昵称，不影响消息投递", response.status());
        return Ok((None, None));
    }
    let body: serde_json::Value = response.json()?;
    Ok(parse_sender_names(&body, uid.trim()))
}

/// 从 `/names` 的响应体里取出 (昵称, 备注)。
///
/// 抽成纯函数是为了能单测：真机上 /names 的常见组合是
/// `names:{uid:null}` 配 `remarks:{uid:"某备注"}`，两条字段各自可能缺失、
/// 也可能是空串，这些形状必须钉住。
fn parse_sender_names(body: &serde_json::Value, uid: &str) -> (Option<String>, Option<String>) {
    let read = |section: &str| {
        body.get(section)
            .and_then(|map| map.get(uid))
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty())
    };
    (read("names"), read("remarks"))
}

/// 断线补拉：`GET /messages?since=<id>`。
///
/// API 文档 §3.3 提供环形缓冲（最近 2000 条）。重连后先用它补齐断线期间的消息，
/// 再继续走 WS——否则那段时间的命令会永久丢失。
#[derive(Debug)]
pub(crate) struct BacklogBatch {
    pub(crate) messages: Vec<miliastra_kernel::chat_source::ExternalChatMessage>,
    /// 缓冲不够，中间可能有缺口；调用方应重建基线而不是继续叠加。
    pub(crate) truncated: bool,
}

pub(crate) fn fetch_messages_since(
    base_http_url: &str,
    token: &str,
    since: u64,
) -> anyhow::Result<BacklogBatch> {
    let url = format!(
        "{}/messages?since={since}&limit=1000",
        base_http_url.trim_end_matches('/')
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let response = client.get(url).header("x-miliastra-token", token).send()?;
    if !response.status().is_success() {
        anyhow::bail!("补拉失败: status={}", response.status());
    }
    let body: serde_json::Value = response.json()?;
    let messages = body
        .get("messages")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| parse_wire_message(item).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(BacklogBatch {
        messages,
        truncated: body
            .get("truncated")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    })
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {

    /// `/names` 的 remarks 解析：这个字段是本轮新接的，形状必须钉住。
    #[test]
    fn names_response_yields_remark_even_without_nickname() {
        // 真机实测就是这个形状：昵称 null，备注有值。
        let body: serde_json::Value = serde_json::from_str(
            r#"{"names":{"100000001":null},"remarks":{"100000001":"显示名甲"}}"#,
        )
        .unwrap();
        let (name, remark) = parse_sender_names(&body, "100000001");
        assert_eq!(name, None);
        assert_eq!(remark.as_deref(), Some("显示名甲"));
    }

    #[test]
    fn names_response_without_remarks_key_yields_none() {
        // 旧版 ChatBridge 没有 remarks 字段，必须当作「没有备注」而不是报错。
        let body: serde_json::Value = serde_json::from_str(r#"{"names":{"42":"玩家甲"}}"#).unwrap();
        let (name, remark) = parse_sender_names(&body, "42");
        assert_eq!(name.as_deref(), Some("玩家甲"));
        assert_eq!(remark, None);
    }

    #[test]
    fn names_response_ignores_blank_values() {
        // 空串等同于没有：不能把空串写进身份映射把占位键擦掉。
        let body: serde_json::Value =
            serde_json::from_str(r#"{"names":{"7":"   "},"remarks":{"7":""}}"#).unwrap();
        let (name, remark) = parse_sender_names(&body, "7");
        assert_eq!(name, None);
        assert_eq!(remark, None);
    }

    #[test]
    fn names_response_only_returns_the_requested_uid() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"names":{"1":"甲","2":"乙"},"remarks":{"1":"备注甲"}}"#)
                .unwrap();
        let (name, remark) = parse_sender_names(&body, "2");
        assert_eq!(name.as_deref(), Some("乙"));
        assert_eq!(remark, None);
    }

    use super::*;

    #[test]
    fn parses_hall_message_from_documented_shape() {
        let raw = r#"{"type":"message","message":{"id":41,"ts":1792772291,
            "channel":"current_hall","hallId":1159597756390572032,"hallType":16,
            "from":{"uid":900000001,"name":"荧"},"to":null,"text":"@点歌 晴天"}}"#;
        let Some(ChatSourceEvent::Messages(messages)) = parse_frame(raw).unwrap() else {
            panic!("应解析为消息事件");
        };
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.sequence, 41);
        assert_eq!(message.sent_at, Some(1792772291));
        assert!(message.channel.is_current_hall());
        assert_eq!(message.hall_id, Some(1159597756390572032));
        assert_eq!(message.sender.uid.as_ref().unwrap().as_str(), "900000001");
        assert_eq!(message.sender.nickname.as_deref(), Some("荧"));
        assert_eq!(message.to_uid, None);
        assert_eq!(message.text, "@点歌 晴天");
    }

    #[test]
    fn tolerates_missing_nickname_and_private_chat() {
        // 好友私聊：hallId/hallType 为 null，昵称可能尚未解析。
        let raw = "{\"type\":\"message\",\"message\":{\"id\":7,\"ts\":1,\
            \"channel\":\"private\",\"hallId\":null,\"hallType\":null,\
            \"from\":{\"uid\":900000015,\"name\":null},\"to\":900000001,\"text\":\"#手牌\"}}";
        let Some(ChatSourceEvent::Messages(messages)) = parse_frame(raw).unwrap() else {
            panic!("应解析为消息事件");
        };
        assert!(messages[0].channel.is_private());
        assert_eq!(messages[0].sender.nickname, None);
        assert_eq!(messages[0].to_uid.as_ref().unwrap().as_str(), "900000001");
    }

    #[test]
    fn parses_hello_and_hall_changed() {
        let hello = r#"{"type":"hello","version":"0.1.0","lastMessageId":42}"#;
        assert_eq!(
            parse_frame(hello).unwrap().unwrap(),
            ChatSourceEvent::Connected { last_sequence: 42 }
        );
        let changed = r#"{"type":"hall_changed","hallId":7699842546315381217,"hallType":23}"#;
        assert_eq!(
            parse_frame(changed).unwrap().unwrap(),
            ChatSourceEvent::HallChanged {
                hall_id: 7699842546315381217,
                hall_type: Some(23)
            }
        );
    }

    #[test]
    fn backlog_messages_parse_from_wire_shape() {
        // /messages 返回裸 ChatMessage 数组，字段与 WS 的 message 内层一致。
        let raw = serde_json::json!({
            "id": 42, "ts": 1792772291,
            "channel": "current_hall",
            "hallId": 1159597756390572032u64, "hallType": 16,
            "from": {"uid": 900000001u64, "name": "荧"},
            "to": null, "text": "@点歌 晴天"
        });
        let message = parse_wire_message(&raw).expect("应解析成功");
        assert_eq!(message.sequence, 42);
        assert_eq!(message.text, "@点歌 晴天");
        assert!(message.channel.is_current_hall());
    }

    #[test]
    fn urlencode_escapes_non_ascii_and_reserved() {
        assert_eq!(urlencode("900000001"), "900000001");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("甲"), "%E7%94%B2");
    }

    #[test]
    fn unknown_frame_type_is_ignored_not_fatal() {
        // 协议向前兼容：服务端加新事件时旧客户端不该断开。
        assert!(
            parse_frame(r#"{"type":"future_event","x":1}"#)
                .unwrap()
                .is_none()
        );
        assert!(parse_frame("not json").unwrap().is_none());
    }
}
