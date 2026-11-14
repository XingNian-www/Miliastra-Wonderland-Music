//! AI 实际请求追踪：最近 N 次发往 provider 的报文，供 Web 面板核对。
//!
//! 为什么需要它：缓存命中率、提示词顺序、工具 schema 这些结论，光看日志
//! 里的「请求形状摘要」无法复核；用户要能看到**这一次到底发了什么**。
//! 内核在真正发出请求时回调 [`miliastra_kernel::ai::WireExchange`]，
//! 这里只做三件事：取最近 N 条、抽缓存/用量指标、脱敏。
//!
//! 边界：
//! - **只存内存**（环形，进程重启即清空），不写日志、不落库、不导出文件。
//! - **不参与业务**：记录失败绝不影响请求本身（observer 内不返回错误）。
//! - 正文里的 `data:image/...;base64,...` 与密钥字段一律替换成占位串；
//!   API Key 只走 HTTP 头，本来就不在请求体里，这里防的是 extras/自定义字段。
//! - 展示的是 provider 原始返回的 `usage`，包括 OpenAI 的
//!   `prompt_tokens_details.cached_tokens` 与 DeepSeek 顶层的
//!   `prompt_cache_hit_tokens`（typed SDK 结构体会丢掉后者）。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::{Map, Value, json};

use miliastra_kernel::ai::WireExchange;

/// 内存里保留的最近请求条数。面板只读最近若干条，不做历史归档。
pub(crate) const CAPACITY: usize = 24;

/// 单条请求的完整快照（含系统提示词、消息、工具与原始 usage）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiTraceEntry {
    pub(crate) id: u64,
    /// 请求开始时间（Unix 毫秒）。
    pub(crate) started_at_ms: u64,
    /// 固定的 UTC+8 可读时间，展示用。
    pub(crate) started_at: String,
    pub(crate) wire_api: String,
    pub(crate) model: String,
    pub(crate) reasoning: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) ok: bool,
    pub(crate) error: Option<String>,
    /// Chat Completions 的 system 消息 / Responses 的 instructions。
    pub(crate) system: Option<String>,
    /// 不含 system 的消息（Chat Completions）或 input items（Responses）。
    /// 工具调用与工具结果原样保留。
    pub(crate) messages: Value,
    /// 本次实际声明的工具 schema；没有工具时是空数组。
    pub(crate) tools: Value,
    /// 请求里除 model/system/messages/tools 外的参数（temperature、tool_choice、
    /// stream、store、max_tokens、prompt_cache_key 等）。
    pub(crate) parameters: Value,
    /// provider 原始 usage 对象（已脱敏）。
    pub(crate) usage: Option<Value>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    /// 缓存命中 token：OpenAI cached_tokens / DeepSeek prompt_cache_hit_tokens。
    pub(crate) cached_tokens: Option<u64>,
    /// `cached_tokens / prompt_tokens`，无数据时为 None。
    pub(crate) cache_hit_ratio: Option<f64>,
    /// system + tools 的指纹：相同指纹代表可复用的稳定前缀完全一致。
    pub(crate) prefix_fingerprint: String,
    /// 稳定前缀（system + tools）的字节数；动态历史/当前问题不计入。
    pub(crate) stable_prefix_bytes: usize,
    /// 请求体 JSON 的字节数（脱敏后、去掉图片数据后）。
    pub(crate) request_bytes: usize,
    /// 消息条数（不含 system）。
    pub(crate) message_count: usize,
    /// 最后一条 user 消息（本轮问题 + 代码追加的尾部）的估算 token 数。
    ///
    /// 与 `stable_prefix_bytes` 一起看：前者衡量可复用的稳定前缀，
    /// 后者衡量每轮真正变化、且被放在最后的动态尾部。用与 ai_chat 相同的
    /// 粗略口径（ASCII≈0.3、其他≈1）估算，不调用 provider 的 tokenizer。
    pub(crate) tail_tokens: usize,
}

struct Store {
    next_id: u64,
    entries: VecDeque<AiTraceEntry>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            next_id: 1,
            entries: VecDeque::with_capacity(CAPACITY),
        })
    })
}

/// 安装内核报文观察者；重复调用无副作用。
///
/// 只在进程启动时调用一次。内核只接受第一次注册，测试里重复安装会返回 false
/// 而不是覆盖已有观察者。
pub(crate) fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return;
    }
    let observer: miliastra_kernel::ai::WireObserver = std::sync::Arc::new(record);
    if miliastra_kernel::ai::set_wire_observer(observer) {
        let _ = INSTALLED.set(());
    }
}

/// 内核回调入口：把一次报文往返做脱敏、抽指标后放进环形缓冲。
pub(crate) fn record(exchange: WireExchange) {
    // 脱敏/序列化/指标提取都在锁外完成：内核是在 Tokio worker 线程同步调用
    // 观察者的，持锁做全量 JSON 处理会拖慢并发的下一次模型请求。
    let mut entry = build_entry(0, exchange);
    let Ok(mut store) = store().lock() else {
        // 面板记录绝不 panic 请求线程；锁中毒时直接丢弃这一条。
        return;
    };
    entry.id = store.next_id;
    store.next_id = store.next_id.saturating_add(1);
    store.entries.push_back(entry);
    while store.entries.len() > CAPACITY {
        store.entries.pop_front();
    }
}

fn build_entry(id: u64, exchange: WireExchange) -> AiTraceEntry {
    let WireExchange {
        wire_api,
        mut request,
        mut response,
        error,
        started_at_ms,
        elapsed,
    } = exchange;
    redact_value(&mut request);
    if let Some(response) = response.as_mut() {
        redact_value(response);
    }

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (system, messages) = split_prompt(wire_api, &request);
    let message_count = messages.as_array().map_or(0, |items| items.len());
    let tools = request.get("tools").cloned().unwrap_or_else(|| json!([]));
    let stable_prefix_bytes = system
        .as_ref()
        .map_or(0, |system| system.len())
        .saturating_add(serde_json::to_vec(&tools).map_or(0, |bytes| bytes.len()));
    let parameters = remaining_parameters(&request);
    let reasoning = read_reasoning(&request);
    let usage = response
        .as_ref()
        .and_then(|response| response.get("usage"))
        .filter(|usage| usage.is_object())
        .cloned();
    let metrics = usage.as_ref().map(usage_metrics).unwrap_or_default();

    AiTraceEntry {
        id,
        started_at_ms,
        started_at: format_utc8_ms(started_at_ms),
        wire_api: wire_api.to_string(),
        model,
        reasoning,
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ok: error.is_none() && response.is_some(),
        error,
        system,
        messages,
        tools,
        parameters,
        usage,
        prompt_tokens: metrics.prompt_tokens,
        completion_tokens: metrics.completion_tokens,
        cached_tokens: metrics.cached_tokens,
        cache_hit_ratio: cache_hit_ratio(metrics.cached_tokens, metrics.prompt_tokens),
        prefix_fingerprint: prefix_fingerprint(wire_api, &request),
        stable_prefix_bytes,
        request_bytes: serde_json::to_vec(&request).map_or(0, |bytes| bytes.len()),
        message_count,
        tail_tokens: tail_tokens(wire_api, &request),
    }
}

/// 最后一条 user 消息的估算 token 数。
///
/// 工具轮次里最后一条可能是 tool 结果，但真正逐轮变化、承载问题与尾部
/// 上下文的是 user 消息，所以从后往前找 role=user。Responses 的 input
/// 与 Chat Completions 的 messages 结构不同，但都带 role，取法一致。
fn tail_tokens(wire_api: &str, request: &Value) -> usize {
    let items = if wire_api == "Responses" {
        request.get("input")
    } else {
        request.get("messages")
    };
    let Some(items) = items.and_then(Value::as_array) else {
        return 0;
    };
    let Some(tail) = items
        .iter()
        .rev()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return 0;
    };
    estimate_text_tokens(tail.get("content")) as usize
}

/// 兼容纯文本与多模态 content（数组里的 `{"type":"text","text":...}`）。
fn estimate_text_tokens(content: Option<&Value>) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    collect_chars(content, &mut ascii, &mut other);
    (ascii as f64 * 0.3 + other as f64).ceil() as u64
}

fn collect_chars(content: Option<&Value>, ascii: &mut u64, other: &mut u64) {
    match content {
        Some(Value::String(text)) => {
            for ch in text.chars() {
                if ch.is_ascii() {
                    *ascii += 1;
                } else {
                    *other += 1;
                }
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                collect_chars(Some(item), ascii, other);
            }
        }
        Some(Value::Object(object)) => {
            // 多模态 part 的正文在 `text`；Responses 的 message 则直接放在
            // content 数组里。先看 text，避免把同一段内容按两个键算两遍。
            if let Some(text) = object.get("text") {
                collect_chars(Some(text), ascii, other);
            } else if let Some(inner) = object.get("content") {
                collect_chars(Some(inner), ascii, other);
            }
        }
        _ => {}
    }
}

/// 拆出 system 与其余消息。
///
/// Chat Completions 的 system 是 messages[0]；Responses 的 system 是
/// `instructions`，消息在 `input`。两种协议在面板上用同一套结构展示。
fn split_prompt(wire_api: &str, request: &Value) -> (Option<String>, Value) {
    if wire_api == "Responses" {
        let system = request
            .get("instructions")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let messages = request.get("input").cloned().unwrap_or_else(|| json!([]));
        return (system, messages);
    }
    let mut system = String::new();
    let mut messages = Vec::new();
    if let Some(items) = request.get("messages").and_then(Value::as_array) {
        for item in items {
            if item.get("role").and_then(Value::as_str) == Some("system") {
                if let Some(content) = item.get("content").and_then(Value::as_str) {
                    system.push_str(content);
                }
            } else {
                messages.push(item.clone());
            }
        }
    }
    let system = (!system.is_empty()).then_some(system);
    (system, Value::Array(messages))
}

/// 请求体里除核心字段外的所有参数，原样展示（temperature、tool_choice…）。
fn remaining_parameters(request: &Value) -> Value {
    let Some(object) = request.as_object() else {
        return json!({});
    };
    let mut parameters = Map::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "model" | "messages" | "input" | "instructions" | "tools"
        ) {
            continue;
        }
        parameters.insert(key.clone(), value.clone());
    }
    Value::Object(parameters)
}

fn read_reasoning(request: &Value) -> Option<String> {
    request
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

#[derive(Default)]
struct UsageMetrics {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
}

/// 从 provider 原始 usage 中取指标。
///
/// 同时覆盖三种常见形状：
/// - OpenAI Chat：`prompt_tokens` / `prompt_tokens_details.cached_tokens`
/// - OpenAI Responses：`input_tokens` / `input_tokens_details.cached_tokens`
/// - DeepSeek：顶层 `prompt_cache_hit_tokens`（miss 数原样留在 usage 里）
fn usage_metrics(usage: &Value) -> UsageMetrics {
    let prompt_tokens = number(usage, "prompt_tokens").or_else(|| number(usage, "input_tokens"));
    let completion_tokens =
        number(usage, "completion_tokens").or_else(|| number(usage, "output_tokens"));
    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|details| number(details, "cached_tokens"))
        .or_else(|| {
            usage
                .get("input_tokens_details")
                .and_then(|details| number(details, "cached_tokens"))
        })
        .or_else(|| number(usage, "prompt_cache_hit_tokens"))
        .or_else(|| number(usage, "cache_read_input_tokens"));
    UsageMetrics {
        prompt_tokens,
        completion_tokens,
        cached_tokens,
    }
}

fn number(object: &Value, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64).or_else(|| {
        object
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|value| u64::try_from(value).ok())
    })
}

fn cache_hit_ratio(cached: Option<u64>, prompt: Option<u64>) -> Option<f64> {
    // 两个字段都必须是 provider 真实返回的：只有 prompt 没有 cache 字段时
    // 显示 0% 会让人误以为「缓存全没命中」，实际是「服务端没报缓存数据」。
    let (cached, prompt) = cached.zip(prompt)?;
    (prompt > 0).then(|| cached as f64 / prompt as f64)
}

/// system + tools 的稳定指纹（FNV-1a 64）。
///
/// 不用 `DefaultHasher`：它不保证跨版本稳定，而这里要放进面板给人比对
/// 「这两次请求的稳定前缀是不是同一份」。
fn prefix_fingerprint(wire_api: &str, request: &Value) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    // system 在两种协议里的位置不同，但都参与指纹。
    if let Some(system) = split_prompt(wire_api, request).0 {
        feed(system.as_bytes());
    }
    feed(b"\0");
    if let Some(tools) = request.get("tools") {
        feed(tools.to_string().as_bytes());
    }
    format!("{hash:016x}")
}

fn format_utc8_ms(unix_ms: u64) -> String {
    let (year, month, day, hour, minute, second) =
        miliastra_kernel::date_time::utc8_datetime((unix_ms / 1000) as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// 密钥字段名（小写精确匹配；`prompt_tokens` 这类用量字段不能误伤）。
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "access_token",
    "refresh_token",
    "secret",
    "password",
    "x-api-key",
    "x-miliastra-token",
    "token",
];

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    // prompt_cache_key 是缓存路由键，不是密钥，必须原样展示给面板。
    if key == "prompt_cache_key" {
        return false;
    }
    SECRET_KEYS.contains(&key.as_str())
        || key == "key"
        || key.ends_with("_key")
        || key.ends_with("-key")
        || key.ends_with("_secret")
        // `prompt_tokens` / `completion_tokens` / `max_tokens` 是用量字段。
        || (key.contains("token") && !key.contains("tokens"))
}

/// 递归脱敏：密钥字段、Bearer/sk- 字面量、data URL 图片正文。
pub(crate) fn redact_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if is_secret_key(key) {
                    *value = json!("[redacted]");
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        Value::String(text) => {
            if let Some(redacted) = redact_string(text) {
                *text = redacted;
            }
        }
        _ => {}
    }
}

fn redact_string(text: &str) -> Option<String> {
    // 兼容整串就是 data URL，以及 data URL 夹在普通文本里的情况。
    if let Some((meta, _payload)) = text.split_once(";base64,")
        && let Some(start) = meta.find("data:")
    {
        let before = &meta[..start];
        let head = &meta[start..];
        return Some(format!(
            "{before}{head};base64,[已省略图片数据 {} 字节]",
            text.len().saturating_sub(meta.len() + 8)
        ));
    }
    let trimmed = text.trim();
    let token = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed);
    if (trimmed.starts_with("sk-") || trimmed.starts_with("Bearer "))
        && token.len() > 16
        && !token.contains(char::is_whitespace)
    {
        return Some("[redacted]".to_string());
    }
    None
}

/// 面板列表：只给摘要，正文按条读取，避免一次传输整个历史。
pub(crate) fn list_json() -> String {
    let Ok(store) = store().lock() else {
        return json!({"entries": [], "error": "AI 请求记录不可用"}).to_string();
    };
    let entries = store.entries.iter().rev().map(summary).collect::<Vec<_>>();
    // 汇总只统计「provider 同时给了 prompt 与 cached」的条目，避免把没有
    // 缓存字段的请求算成 0% 命中。
    let (mut prompt_tokens, mut cached_tokens, mut measured) = (0u64, 0u64, 0usize);
    for entry in &store.entries {
        if let (Some(prompt), Some(cached)) = (entry.prompt_tokens, entry.cached_tokens) {
            prompt_tokens = prompt_tokens.saturating_add(prompt);
            cached_tokens = cached_tokens.saturating_add(cached);
            measured += 1;
        }
    }
    let mut fingerprints = std::collections::BTreeSet::new();
    for entry in &store.entries {
        fingerprints.insert(entry.prefix_fingerprint.clone());
    }
    json!({
        "capacity": CAPACITY,
        "count": store.entries.len(),
        "entries": entries,
        "stats": {
            "requests": store.entries.len(),
            "promptTokens": prompt_tokens,
            "cachedTokens": cached_tokens,
            // 只按真正上报缓存字段的请求汇总；measured=0 时不显示 0%。
            "cacheHitRatio": (measured > 0).then(|| {
                cache_hit_ratio(Some(cached_tokens), Some(prompt_tokens)).unwrap_or(0.0)
            }),
            "cacheMeasuredRequests": measured,
            "distinctPrefixes": fingerprints.len(),
        },
    })
    .to_string()
}

fn summary(entry: &AiTraceEntry) -> Value {
    json!({
        "id": entry.id,
        "startedAt": entry.started_at,
        "startedAtMs": entry.started_at_ms,
        "wireApi": entry.wire_api,
        "model": entry.model,
        "reasoning": entry.reasoning,
        "ok": entry.ok,
        "durationMs": entry.duration_ms,
        "promptTokens": entry.prompt_tokens,
        "completionTokens": entry.completion_tokens,
        "cachedTokens": entry.cached_tokens,
        "cacheHitRatio": entry.cache_hit_ratio,
        "prefixFingerprint": entry.prefix_fingerprint,
        "stablePrefixBytes": entry.stable_prefix_bytes,
        "requestBytes": entry.request_bytes,
        "messageCount": entry.message_count,
        "tailTokens": entry.tail_tokens,
        "error": entry.error,
    })
}

/// 单条完整请求；按 id 取，找不到返回 None（面板显示「已滚出」）。
pub(crate) fn entry_json(id: u64) -> Option<String> {
    // 先在锁内克隆，再在锁外序列化：单条报文可能有几十 KB，不阻塞下一次
    // AI 请求写入。
    let entry = {
        let store = store().lock().ok()?;
        store.entries.iter().find(|entry| entry.id == id).cloned()?
    };
    serde_json::to_string(&entry).ok()
}

/// 测试辅助：清空全局环形缓冲并重置 id。
#[cfg(test)]
pub(crate) fn clear_for_tests() {
    if let Ok(mut store) = store().lock() {
        store.entries.clear();
        store.next_id = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 全局环形缓冲是进程级单例；并行测试必须串行访问它。
    fn trace_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn exchange(request: Value, response: Option<Value>) -> WireExchange {
        WireExchange {
            wire_api: "ChatCompletions",
            request,
            response,
            error: None,
            started_at_ms: 1_793_026_800_000,
            elapsed: Duration::from_millis(123),
        }
    }

    #[test]
    fn redacts_secret_fields_and_image_payloads() {
        let mut value = json!({
            "api_key": "sk-live-should-not-show",
            "extra": {"token": "abc", "prompt_tokens": 12, "max_tokens": 64},
            "secret_key": "abc",
            "x-goog-api-key": "abc",
            "prompt_cache_key": "miliastra-keep-me",
            "image_url": {"url": "data:image/jpeg;base64,AAAA"},
            "pasted": "图片：data:image/png;base64,BBBB 结束",
            "nested": ["Bearer abcdefghijklmnopqrstuvwxyz"],
        });
        redact_value(&mut value);
        assert_eq!(value["api_key"], "[redacted]");
        assert_eq!(value["extra"]["token"], "[redacted]");
        assert_eq!(value["secret_key"], "[redacted]", "secret_key 也是密钥字段");
        assert_eq!(value["x-goog-api-key"], "[redacted]");
        assert_eq!(
            value["extra"]["prompt_tokens"], 12,
            "用量字段不能被当成密钥"
        );
        assert_eq!(value["extra"]["max_tokens"], 64);
        assert_eq!(
            value["prompt_cache_key"], "miliastra-keep-me",
            "缓存路由键不是密钥，面板要能看到"
        );
        assert!(
            value["image_url"]["url"]
                .as_str()
                .unwrap()
                .contains("已省略图片数据")
        );
        assert_eq!(value["nested"][0], "[redacted]");
        assert!(
            value["pasted"].as_str().unwrap().contains("已省略图片数据"),
            "夹在文本里的 data URL 也要裁剪"
        );
        assert_eq!(
            cache_hit_ratio(None, Some(10)),
            None,
            "服务端没给缓存字段时不能算成 0%"
        );
        assert_eq!(cache_hit_ratio(Some(0), Some(10)), Some(0.0));
    }

    #[test]
    fn usage_metrics_cover_openai_responses_and_deepseek() {
        let openai = usage_metrics(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 60}
        }));
        assert_eq!(openai.prompt_tokens, Some(100));
        assert_eq!(openai.cached_tokens, Some(60));

        let responses = usage_metrics(&json!({
            "input_tokens": 80,
            "output_tokens": 10,
            "input_tokens_details": {"cached_tokens": 40}
        }));
        assert_eq!(responses.prompt_tokens, Some(80));
        assert_eq!(responses.completion_tokens, Some(10));
        assert_eq!(responses.cached_tokens, Some(40));

        // DeepSeek 的命中数在顶层，typed SDK 会丢掉；这里必须能读到。
        let deepseek = usage_metrics(&json!({
            "prompt_tokens": 50,
            "prompt_cache_hit_tokens": 30,
            "prompt_cache_miss_tokens": 20
        }));
        assert_eq!(deepseek.cached_tokens, Some(30));
        assert_eq!(
            cache_hit_ratio(deepseek.cached_tokens, deepseek.prompt_tokens),
            Some(0.6)
        );
    }

    #[test]
    fn splits_system_and_messages_for_both_wires() {
        let chat = json!({
            "messages": [
                {"role": "system", "content": "系统"},
                {"role": "user", "content": "问题"},
                {"role": "tool", "tool_call_id": "1", "content": "结果"}
            ]
        });
        let (system, messages) = split_prompt("ChatCompletions", &chat);
        assert_eq!(system.as_deref(), Some("系统"));
        assert_eq!(messages.as_array().unwrap().len(), 2);

        let responses = json!({
            "instructions": "系统",
            "input": [{"role": "user", "content": "问题"}]
        });
        let (system, messages) = split_prompt("Responses", &responses);
        assert_eq!(system.as_deref(), Some("系统"));
        assert_eq!(messages.as_array().unwrap().len(), 1);
    }

    #[test]
    fn prefix_fingerprint_ignores_history_but_tracks_system_and_tools() {
        let base = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "S"},
                {"role": "user", "content": "第一问"}
            ],
            "tools": [{"type": "function", "function": {"name": "a"}}]
        });
        let mut later = base.clone();
        later["messages"] = json!([
            {"role": "system", "content": "S"},
            {"role": "user", "content": "第一问"},
            {"role": "assistant", "content": "答"},
            {"role": "user", "content": "第二问"}
        ]);
        assert_eq!(
            prefix_fingerprint("ChatCompletions", &base),
            prefix_fingerprint("ChatCompletions", &later),
            "历史增长不应改变稳定前缀指纹"
        );
        let mut changed_tools = base.clone();
        changed_tools["tools"] = json!([]);
        assert_ne!(
            prefix_fingerprint("ChatCompletions", &base),
            prefix_fingerprint("ChatCompletions", &changed_tools)
        );
        let mut changed_system = base.clone();
        changed_system["messages"][0]["content"] = json!("S2");
        assert_ne!(
            prefix_fingerprint("ChatCompletions", &base),
            prefix_fingerprint("ChatCompletions", &changed_system)
        );
    }

    #[test]
    fn ring_keeps_newest_entries_and_details_are_fetchable() {
        let _guard = trace_test_lock();
        clear_for_tests();
        for index in 0..(CAPACITY + 3) {
            record(exchange(
                json!({
                    "model": "m",
                    "messages": [{"role": "system", "content": "S"}],
                    "tools": []
                }),
                Some(json!({"usage": {"prompt_tokens": index as u64}})),
            ));
        }
        let list: Value = serde_json::from_str(&list_json()).expect("list JSON");
        assert_eq!(list["count"], CAPACITY);
        let ids = list["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids.first().copied(), Some((CAPACITY + 3) as u64));
        assert_eq!(ids.last().copied(), Some(4));
        assert!(entry_json(ids[0]).is_some());
        assert!(entry_json(1).is_none(), "被挤出的条目不再可读");
        clear_for_tests();
    }

    /// 端到端形状：真实 Chat Completions 请求 + DeepSeek 风格 usage，
    /// 面板要能一次拿到 system、messages（含工具消息）、tools 与缓存字段。
    #[test]
    fn detail_exposes_the_exact_wire_request_and_cache_fields() {
        let _guard = trace_test_lock();
        clear_for_tests();
        record(exchange(
            json!({
                "model": "deepseek-chat",
                "stream": false,
                "store": false,
                "tool_choice": "auto",
                "max_completion_tokens": 256,
                "tools": [{
                    "type": "function",
                    "function": {"name": "look", "description": "看画面",
                                 "parameters": {"type": "object"}}
                }],
                "messages": [
                    {"role": "system", "content": "稳定人设"},
                    {"role": "user", "content": "[甲]:第一问"},
                    {"role": "assistant", "content": null,
                     "tool_calls": [{"id": "c1", "type": "function",
                                     "function": {"name": "look", "arguments": "{}"}}]},
                    {"role": "tool", "tool_call_id": "c1", "content": "{\"status\":\"looked\"}"},
                    {"role": "user", "content": "当前时间：2026-11-13 23:00"}
                ]
            }),
            Some(json!({
                "id": "x",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 1200, "completion_tokens": 30,
                          "total_tokens": 1230, "prompt_cache_hit_tokens": 900,
                          "prompt_cache_miss_tokens": 300}
            })),
        ));
        let list: Value = serde_json::from_str(&list_json()).expect("list JSON");
        let id = list["entries"][0]["id"].as_u64().expect("id");
        let entry: Value =
            serde_json::from_str(&entry_json(id).expect("detail")).expect("detail JSON");
        assert_eq!(entry["system"], "稳定人设");
        assert_eq!(entry["model"], "deepseek-chat");
        assert_eq!(entry["tools"].as_array().unwrap().len(), 1);
        let messages = entry["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4, "system 不计入 messages");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "{\"status\":\"looked\"}");
        assert_eq!(entry["promptTokens"], 1200);
        assert_eq!(entry["cachedTokens"], 900);
        assert_eq!(entry["cacheHitRatio"], 0.75);
        assert_eq!(
            entry["parameters"]["tool_choice"], "auto",
            "请求参数（含 tool_choice）必须可见"
        );
        assert!(entry["parameters"].get("messages").is_none());
        assert!(entry["stablePrefixBytes"].as_u64().unwrap() > 0);
        // 尾部取最后一条 user 消息（当前问题），不是 system，也不是工具结果。
        assert!(
            entry["tailTokens"].as_u64().unwrap() > 0,
            "尾部 token 必须能算出来"
        );
        assert_eq!(list["entries"][0]["tailTokens"], entry["tailTokens"]);
        assert_eq!(entry["usage"]["prompt_cache_miss_tokens"], 300);
        clear_for_tests();
    }

    /// 尾部指标只统计最后一条 user 消息：工具轮次的 tool 结果不算，
    /// 历史更早的 user 消息也不算（那部分属于可缓存前缀）。
    #[test]
    fn tail_tokens_track_only_the_last_user_message() {
        let request = json!({
            "messages": [
                {"role": "system", "content": "S"},
                {"role": "user", "content": "很早以前的问题"},
                {"role": "assistant", "content": "答"},
                {"role": "user", "content": [{"type": "text", "text": "新问题"}]},
                {"role": "assistant", "content": null,
                 "tool_calls": [{"id": "c1", "type": "function",
                                 "function": {"name": "look", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "{\"status\":\"looked\"}"}
            ]
        });
        assert_eq!(tail_tokens("ChatCompletions", &request), 3);

        let responses = json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "旧问题"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "答"}]},
                {"role": "user", "content": [{"type": "input_text", "text": "你好"}]}
            ]
        });
        assert_eq!(tail_tokens("Responses", &responses), 2);
    }

    #[test]
    fn record_is_failure_safe_and_marks_errors() {
        let _guard = trace_test_lock();
        clear_for_tests();
        record(WireExchange {
            wire_api: "Responses",
            request: json!({"model": "m", "instructions": "S", "input": []}),
            response: None,
            error: Some("OpenAI Chat Completions 请求超时".into()),
            started_at_ms: 0,
            elapsed: Duration::from_millis(5),
        });
        let list: Value = serde_json::from_str(&list_json()).expect("list JSON");
        assert_eq!(list["entries"][0]["ok"], false);
        assert!(
            list["entries"][0]["error"]
                .as_str()
                .unwrap()
                .contains("超时")
        );
        clear_for_tests();
    }
}
