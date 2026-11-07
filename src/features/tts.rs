//! fish.audio TTS 客户端与语音编排（task-49 P0）。
//!
//! 设计边界：
//! - 只有 AI 聊天工具 `speak` 会入队，普通模板/命令回复不自动朗读；
//! - fish 请求在独立 `tts-worker` 线程执行，不阻塞 AI 文本回复；
//! - 语音播放器与音乐播放器完全独立；duck 只改 `PlaybackHandle::set_duck_gain`，
//!   不碰 `PlaybackSnapshot.volume`；
//! - 任何网络/解码/设备错误都静默降级，日志不打印 API key 或完整文本。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

use thiserror::Error;

use miliastra_playback::{PlaybackHandle, SpeechAudio, SpeechHealth, SpeechPlayer};

/// fish.audio 默认地址。
pub const DEFAULT_TTS_ENDPOINT: &str = "https://api.fish.audio";
/// 默认模型；S2.1 pro 支持情绪标签。
pub const DEFAULT_TTS_MODEL: &str = "s2.1-pro";

/// `tts` 配置段。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TtsConfig {
    /// 总开关；未配置 api_key 时即使开启也不会发请求。
    #[serde(default)]
    pub enabled: bool,
    /// fish.audio 接口地址；IdleReload。
    #[serde(default = "default_tts_endpoint")]
    pub endpoint: String,
    /// fish.audio API key；Secret + IdleReload，不打印。
    #[serde(default)]
    pub api_key: String,
    /// 可选独立代理；空=沿用 reqwest 环境代理。作用于音色目录与 /v1/tts。
    #[serde(default)]
    pub http_proxy: String,
    /// 模型名，作为 HTTP `model` 头发送。
    #[serde(default = "default_tts_model")]
    pub model: String,
    /// fish voice/reference id。
    #[serde(default)]
    pub reference_id: String,
    /// `reference_id` 的用户友好别名；`reference_id` 为空时使用它。
    #[serde(default)]
    pub voice: String,
    /// 输出音频格式；P0 只支持 wav。
    #[serde(default)]
    pub format: TtsFormat,
    /// 说话期间音乐输出增益（0–100，100=不压低）。
    #[serde(default = "default_tts_duck_volume")]
    pub duck_volume: u8,
    /// duck 淡入/淡出时长（毫秒）。
    #[serde(default = "default_tts_duck_fade_ms")]
    pub duck_fade_ms: u64,
    /// 单条语音文本上限（字符数，超出截断）。
    #[serde(default = "default_tts_max_chars")]
    pub max_chars: usize,
    /// fish 请求超时（毫秒）。
    #[serde(default = "default_tts_timeout_ms")]
    pub timeout_ms: u64,
    /// 队列上限；超过后拒绝新语音。
    #[serde(default = "default_tts_queue_max")]
    pub queue_max: usize,
    /// 触发范围；P0 固定仅 AI 聊天。
    #[serde(default)]
    pub trigger_scope: TtsTriggerScope,
}

/// 输出格式；P0 只请求 wav，SpeechPlayer 直接解码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsFormat {
    #[default]
    Wav,
}

/// 触发范围；P0 固定仅 AI 聊天工具。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsTriggerScope {
    #[default]
    AiChat,
}

fn default_tts_endpoint() -> String {
    DEFAULT_TTS_ENDPOINT.to_string()
}

fn default_tts_model() -> String {
    DEFAULT_TTS_MODEL.to_string()
}

fn default_tts_duck_volume() -> u8 {
    30
}

fn default_tts_duck_fade_ms() -> u64 {
    200
}

fn default_tts_max_chars() -> usize {
    200
}

fn default_tts_timeout_ms() -> u64 {
    8_000
}

fn default_tts_queue_max() -> usize {
    8
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_tts_endpoint(),
            api_key: String::new(),
            http_proxy: String::new(),
            model: default_tts_model(),
            reference_id: String::new(),
            voice: String::new(),
            format: TtsFormat::default(),
            duck_volume: default_tts_duck_volume(),
            duck_fade_ms: default_tts_duck_fade_ms(),
            max_chars: default_tts_max_chars(),
            timeout_ms: default_tts_timeout_ms(),
            queue_max: default_tts_queue_max(),
            trigger_scope: TtsTriggerScope::default(),
        }
    }
}

impl fmt::Debug for TtsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TtsConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .field(
                "http_proxy",
                &if self.http_proxy.trim().is_empty() {
                    ""
                } else {
                    "<set>"
                },
            )
            .field("model", &self.model)
            .field("reference_id", &self.reference_id)
            .field("voice", &self.voice)
            .field("format", &self.format)
            .field("duck_volume", &self.duck_volume)
            .field("duck_fade_ms", &self.duck_fade_ms)
            .field("max_chars", &self.max_chars)
            .field("timeout_ms", &self.timeout_ms)
            .field("queue_max", &self.queue_max)
            .field("trigger_scope", &self.trigger_scope)
            .finish()
    }
}

impl TtsConfig {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        validate_tts_http_proxy(&self.http_proxy)?;
        if self.duck_volume > 100 {
            anyhow::bail!("tts.duck_volume 必须在 0..=100");
        }
        if self.duck_fade_ms > 2_000 {
            anyhow::bail!("tts.duck_fade_ms 必须在 0..=2000");
        }
        if !(1..=2_000).contains(&self.max_chars) {
            anyhow::bail!("tts.max_chars 必须在 1..=2000");
        }
        if !(1_000..=60_000).contains(&self.timeout_ms) {
            anyhow::bail!("tts.timeout_ms 必须在 1000..=60000");
        }
        if !(1..=64).contains(&self.queue_max) {
            anyhow::bail!("tts.queue_max 必须在 1..=64");
        }
        if self.enabled {
            if self.endpoint.trim().is_empty() {
                anyhow::bail!("tts.endpoint 在启用时不能为空");
            }
            if !(self.endpoint.trim().starts_with("http://")
                || self.endpoint.trim().starts_with("https://"))
            {
                anyhow::bail!("tts.endpoint 必须是 http:// 或 https:// 地址");
            }
        }
        Ok(())
    }

    /// 模型名为空时回退默认模型，保证不会发送空 `model` 头。
    pub(crate) fn effective_model(&self) -> &str {
        let model = self.model.trim();
        if model.is_empty() {
            DEFAULT_TTS_MODEL
        } else {
            model
        }
    }

    pub(crate) fn voice_id(&self) -> &str {
        let reference = self.reference_id.trim();
        if reference.is_empty() {
            self.voice.trim()
        } else {
            reference
        }
    }
}

/// 官方文档列出的固定 TTS 推理模型（没有公开的独立列表端点，故内置 fallback）。
pub const TTS_FALLBACK_MODELS: &[&str] =
    &["s1", "s2-pro", "s2.1-pro", "s2.1-pro-free", "drama-3"];

/// 模型/音色列表来源。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsCatalogSource {
    Api,
    #[default]
    Fallback,
}

/// fish voice model（`GET /model` 的 item）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsVoice {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub visibility: String,
}

/// 模型/音色目录。TTS 推理模型没有公开列表端点，`models` 始终是内置 fallback；
/// `source` 表示音色列表是否来自 fish API，`model_source` 始终为 `fallback`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsCatalog {
    pub models: Vec<String>,
    pub voices: Vec<TtsVoice>,
    pub source: TtsCatalogSource,
    #[serde(rename = "modelSource")]
    pub model_source: TtsCatalogSource,
    /// 音色列表只取到第一页时为 true；避免 UI 把不完整列表当真。
    #[serde(default)]
    pub truncated: bool,
}

impl TtsCatalog {
    pub fn fallback() -> Self {
        Self {
            models: TTS_FALLBACK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
            voices: Vec::new(),
            source: TtsCatalogSource::Fallback,
            model_source: TtsCatalogSource::Fallback,
            truncated: false,
        }
    }
}

/// 文本内风格标记语法。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TtsStyleMode {
    /// 未知模型：不注入任何风格标记。
    #[default]
    None,
    /// S1：使用 `(angry)` 一类括号标签。
    Parentheses,
    /// S2：使用 `[laugh]` 一类方括号标签/自然语言。
    Brackets,
    /// Drama 3：使用自然语言方括号导演指令。
    DramaDirections,
}

/// 单个 TTS 推理模型的能力快照。未知模型用安全默认（仅文本）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtsModelCapabilities {
    pub known: bool,
    pub style_mode: TtsStyleMode,
    pub supports_emotion: bool,
    pub supports_style: bool,
    pub supports_speed: bool,
    pub supports_volume_db: bool,
    pub supports_multi_speaker: bool,
    pub supports_phoneme: bool,
    /// Drama 3 的成对词级标签（`<whisper>…</whisper>` 等）。
    pub supports_paired_tags: bool,
    /// Drama 3 的实验性语言切换标签（`<french>…</french>` 等）。
    pub supports_language_tags: bool,
    /// SDK 公共 TTSConfig 的文本清洗参数。
    pub supports_normalize: bool,
    /// SDK 公共 TTSConfig 的分块长度参数。
    pub supports_chunk_length: bool,
    /// SDK 公共 TTSConfig 的延迟模式参数。
    pub supports_latency: bool,
    /// SDK 公共 TTSConfig 的采样参数（temperature/top_p/max_new_tokens）。
    pub supports_sampling: bool,
    /// SDK 公共 TTSConfig 的 reference_id/references 参数。
    pub supports_reference_id: bool,
}

impl Default for TtsModelCapabilities {
    fn default() -> Self {
        Self {
            known: false,
            style_mode: TtsStyleMode::None,
            supports_emotion: false,
            supports_style: false,
            supports_speed: false,
            supports_volume_db: false,
            supports_multi_speaker: false,
            supports_phoneme: false,
            supports_paired_tags: false,
            supports_language_tags: false,
            supports_normalize: false,
            supports_chunk_length: false,
            supports_latency: false,
            supports_sampling: false,
            supports_reference_id: false,
        }
    }
}

impl TtsModelCapabilities {
    pub fn label(&self) -> &'static str {
        match self.style_mode {
            TtsStyleMode::Parentheses => "S1 括号固定情绪标签",
            TtsStyleMode::Brackets => "S2/S2.1 方括号自由自然语言 cue",
            TtsStyleMode::DramaDirections => "Drama 3 方括号导演指示 + 成对标签",
            TtsStyleMode::None => "无风格标签能力",
        }
    }
}

/// 按模型 id 查能力；未知模型返回安全默认，不按名字猜。
pub fn tts_model_capabilities(model: &str) -> TtsModelCapabilities {
    match model.trim().to_ascii_lowercase().as_str() {
        "s1" => TtsModelCapabilities {
            known: true,
            style_mode: TtsStyleMode::Parentheses,
            supports_emotion: true,
            supports_style: false,
            supports_speed: true,
            supports_volume_db: true,
            supports_multi_speaker: false,
            supports_phoneme: false,
            supports_paired_tags: false,
            supports_language_tags: false,
            supports_normalize: true,
            supports_chunk_length: true,
            supports_latency: true,
            supports_sampling: true,
            supports_reference_id: true,
        },
        "s2-pro" | "s2.1-pro" | "s2.1-pro-free" => TtsModelCapabilities {
            known: true,
            style_mode: TtsStyleMode::Brackets,
            supports_emotion: true,
            supports_style: true,
            supports_speed: true,
            supports_volume_db: true,
            supports_multi_speaker: true,
            supports_phoneme: false,
            supports_paired_tags: false,
            supports_language_tags: false,
            supports_normalize: true,
            supports_chunk_length: true,
            supports_latency: true,
            supports_sampling: true,
            supports_reference_id: true,
        },
        "drama-3" => TtsModelCapabilities {
            known: true,
            style_mode: TtsStyleMode::DramaDirections,
            supports_emotion: true,
            supports_style: true,
            supports_speed: true,
            supports_volume_db: true,
            supports_multi_speaker: true,
            supports_phoneme: true,
            supports_paired_tags: true,
            supports_language_tags: true,
            supports_normalize: true,
            supports_chunk_length: true,
            supports_latency: true,
            supports_sampling: true,
            supports_reference_id: true,
        },
        _ => TtsModelCapabilities::default(),
    }
}

/// `speak` 工具的可选语气/风格参数。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TtsSpeakOptions {
    pub emotion: Option<String>,
    pub style: Option<String>,
    pub speed: Option<f32>,
    pub volume_db: Option<f32>,
}

impl TtsSpeakOptions {
    pub fn is_empty(&self) -> bool {
        self.emotion.is_none()
            && self.style.is_none()
            && self.speed.is_none()
            && self.volume_db.is_none()
    }
}

/// 把情绪/风格参数映射成 fish 文本标记；未支持的字段由调用方先拒绝。
pub(crate) fn decorate_speak_text(
    text: &str,
    options: &TtsSpeakOptions,
    capabilities: &TtsModelCapabilities,
) -> String {
    let mut prefix = String::new();
    if let Some(style) = options.style.as_deref().filter(|value| !value.is_empty())
        && capabilities.supports_style
    {
        prefix.push_str(&format!("[{style}] "));
    }
    if let Some(emotion) = options.emotion.as_deref().filter(|value| !value.is_empty())
        && capabilities.supports_emotion
    {
        match capabilities.style_mode {
            TtsStyleMode::Parentheses => prefix.push_str(&format!("({emotion}) ")),
            TtsStyleMode::Brackets | TtsStyleMode::DramaDirections => {
                prefix.push_str(&format!("[{emotion}] "));
            }
            TtsStyleMode::None => {}
        }
    }
    format!("{prefix}{text}")
}

/// 构造 fish `prosody` 字段；不支持/未设置的参数不写入。
pub(crate) fn speak_prosody_body(
    options: &TtsSpeakOptions,
    capabilities: &TtsModelCapabilities,
) -> Option<Value> {
    let mut prosody = serde_json::Map::new();
    if let Some(speed) = options.speed.filter(|_| capabilities.supports_speed) {
        prosody.insert("speed".to_string(), json!(speed));
    }
    if let Some(volume) = options.volume_db.filter(|_| capabilities.supports_volume_db) {
        prosody.insert("volume".to_string(), json!(volume));
    }
    (!prosody.is_empty()).then_some(Value::Object(prosody))
}

/// POST /tts/models 的未保存配置覆盖；`None`=沿用已保存，空串=明确清空。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TtsCatalogOverrides {
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
}

/// 目录请求错误。`Display` 不包含 API key 或文本正文。
#[derive(Clone, Debug, Error)]
pub enum TtsCatalogError {
    #[error("tts catalog config error: {0}")]
    Config(String),
    #[error("tts catalog auth failed: {0}")]
    Auth(String),
    #[error("tts catalog rate limited: {0}")]
    RateLimited(String),
    #[error("tts catalog request timed out: {0}")]
    Timeout(String),
    #[error("tts catalog request failed: {0}")]
    Request(String),
}

/// 校验 TTS 代理。kernel 的 `validate_http_proxy` 只接受 http/https；
/// TTS 额外允许 `socks5://`（reqwest 支持），其余仍复用它。
fn validate_tts_http_proxy(proxy: &str) -> anyhow::Result<()> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return Ok(());
    }
    if proxy.to_ascii_lowercase().starts_with("socks5://") {
        let parsed = url::Url::parse(proxy)
            .map_err(|error| anyhow::anyhow!("tts.http_proxy 配置无效：{error}"))?;
        if parsed.host().is_none() {
            anyhow::bail!("tts.http_proxy 的 socks5 地址缺少主机");
        }
        return Ok(());
    }
    miliastra_kernel::ai::validate_http_proxy(proxy)
        .map_err(|error| anyhow::anyhow!("tts.http_proxy 配置无效：{error}"))
}

/// 统一构建 fish HTTP client：空代理走 reqwest 默认环境代理；非空显式设置代理。
fn build_tts_client(
    config: &TtsConfig,
    timeout: Duration,
) -> std::result::Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(concat!(
            "miliastra-wonderland-music/",
            env!("CARGO_PKG_VERSION")
        ));
    let proxy = config.http_proxy.trim();
    if !proxy.is_empty() {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| format!("tts.http_proxy 配置无效：{error}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| format!("构建 TTS HTTP 客户端失败：{error}"))
}

/// 拉取 fish voice 列表并附上内置 TTS 模型 fallback。
///
/// - key 为空时直接返回 fallback，不发网络请求；
/// - 网络/HTTP 错误返回 `Request`（路由映射 500）；配置错误返回 `Config`（400）；
/// - 日志与错误都不包含 key 或文本正文。
pub fn fetch_catalog(
    config: &TtsConfig,
    overrides: &TtsCatalogOverrides,
) -> Result<TtsCatalog, TtsCatalogError> {
    let endpoint = overrides
        .endpoint
        .as_deref()
        .unwrap_or(&config.endpoint)
        .trim()
        .to_string();
    let api_key = overrides
        .api_key
        .as_deref()
        .unwrap_or(&config.api_key)
        .trim()
        .to_string();
    if endpoint.is_empty() {
        return Err(TtsCatalogError::Config("tts.endpoint 不能为空".to_string()));
    }
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return Err(TtsCatalogError::Config(
            "tts.endpoint 必须是 http:// 或 https:// 地址".to_string(),
        ));
    }
    if api_key.is_empty() {
        return Ok(TtsCatalog::fallback());
    }
    let url = catalog_url(&endpoint);
    let client = build_tts_client(
        config,
        Duration::from_millis(config.timeout_ms.max(1_000).min(15_000)),
    )
    .map_err(TtsCatalogError::Config)?;
    let response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                TtsCatalogError::Timeout("音色列表请求超时".to_string())
            } else {
                TtsCatalogError::Request(error.to_string())
            }
        })?;
    let status = response.status().as_u16();
    if let Some(message) = tts_error_for_status(status) {
        return Err(match status {
            401 | 403 => TtsCatalogError::Auth(message.to_string()),
            429 => TtsCatalogError::RateLimited(message.to_string()),
            408 => TtsCatalogError::Timeout(message.to_string()),
            _ => TtsCatalogError::Request(message.to_string()),
        });
    }
    let body = response
        .text()
        .map_err(|error| TtsCatalogError::Request(error.to_string()))?;
    let voices = parse_voice_catalog(&body)?;
    let truncated = serde_json::from_str::<Value>(&body)
        .ok()
        .map(|value| {
            value.get("has_more").and_then(Value::as_bool).unwrap_or(false)
                || value
                    .get("total")
                    .and_then(Value::as_u64)
                    .is_some_and(|total| total as usize > voices.len())
        })
        .unwrap_or(false);
    Ok(TtsCatalog {
        models: TtsCatalog::fallback().models,
        voices,
        source: TtsCatalogSource::Api,
        model_source: TtsCatalogSource::Fallback,
        truncated,
    })
}

fn catalog_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let base = endpoint.strip_suffix("/v1/tts").unwrap_or(endpoint);
    format!("{base}/model?self=true&page_size=100")
}

/// 解析 fish `/model` 响应；`total` 只做诊断，不要求与 items 长度一致。
pub fn parse_voice_catalog(body: &str) -> Result<Vec<TtsVoice>, TtsCatalogError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| TtsCatalogError::Request("fish 音色列表不是合法 JSON".to_string()))?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| TtsCatalogError::Request("fish 音色列表缺少 items".to_string()))?;
    let mut voices = Vec::new();
    for item in items {
        let id = item
            .get("_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| TtsCatalogError::Request("fish 音色 item 缺少 id".to_string()))?;
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or(id)
            .to_string();
        voices.push(TtsVoice {
            id: id.to_string(),
            title,
            state: item
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            visibility: item
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(voices)
}

/// TTS 错误。`Display` 不包含 API key 或文本正文。
#[derive(Clone, Debug, Error)]
pub enum TtsError {
    #[error("tts disabled or not configured")]
    Disabled,
    #[error("tts queue is full")]
    QueueFull,
    #[error("tts request timed out")]
    Timeout,
    #[error("tts request failed with status {0}")]
    Http(u16),
    #[error("tts config error: {0}")]
    Config(String),
    #[error("tts request failed: {0}")]
    Request(String),
    #[error("tts audio is empty or unsupported")]
    Audio,
    #[error("tts speech player unavailable: {0}")]
    Player(String),
    #[error("tts option is unsupported for this model: {0}")]
    UnsupportedOption(String),
}

/// 把一段文本截到 `max_chars` 个 Unicode 字符；空文本返回空串。
pub(crate) fn truncate_tts_text(text: &str, max_chars: usize) -> String {
    text.trim().chars().take(max_chars).collect()
}

/// 把配置的 endpoint 归一化成 fish 的 `/v1/tts` 地址。
pub(crate) fn tts_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.ends_with("/v1/tts") {
        endpoint.to_string()
    } else {
        format!("{endpoint}/v1/tts")
    }
}

/// fish HTTP 状态码 → 错误；成功由调用方继续读取 body。
pub(crate) fn tts_error_for_status(status: u16) -> Option<TtsError> {
    match status {
        200..=299 => None,
        other => Some(TtsError::Http(other)),
    }
}

/// 生成 duck 增益渐变序列（包含起点，不含终点前的重复；最后一项是目标）。
pub(crate) fn duck_gain_steps(from: u8, to: u8, steps: u32) -> Vec<u8> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|step| {
            let value = i32::from(from)
                + (i32::from(to) - i32::from(from)) * step as i32 / steps as i32;
            value.clamp(0, 100) as u8
        })
        .collect()
}

struct TtsJob {
    text: String,
    config: TtsConfig,
    options: TtsSpeakOptions,
    queued_at: Instant,
}

enum TtsMessage {
    Job(TtsJob),
    Shutdown,
}

/// TTS 编排线程句柄。SpeechPlayer 懒加载；未启用/未配置 key 时不打开音频设备。
pub struct TtsRuntime {
    playback: Option<PlaybackHandle>,
    worker: Mutex<Option<TtsWorker>>,
    pending: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    unavailable: AtomicBool,
    health: Arc<Mutex<Option<SpeechHealth>>>,
}

struct TtsWorker {
    sender: SyncSender<TtsMessage>,
    thread: JoinHandle<()>,
}

impl TtsRuntime {
    pub fn new(playback: PlaybackHandle) -> Self {
        Self {
            playback: Some(playback),
            worker: Mutex::new(None),
            pending: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            unavailable: AtomicBool::new(false),
            health: Arc::new(Mutex::new(None)),
        }
    }

    pub fn disabled() -> Self {
        Self {
            playback: None,
            worker: Mutex::new(None),
            pending: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(true)),
            unavailable: AtomicBool::new(true),
            health: Arc::new(Mutex::new(Some(SpeechHealth::Closed))),
        }
    }

    /// 播放器是否仍可能可用（懒加载前为 true；启动失败后为 false）。
    pub fn available(&self) -> bool {
        if self.unavailable.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            return false;
        }
        let health = self
            .health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        health.as_ref().is_none_or(SpeechHealth::is_healthy)
    }

    fn ensure_worker(&self) -> Result<(), TtsError> {
        let mut guard = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        if self.unavailable.load(Ordering::Acquire) {
            return Err(TtsError::Player("speech player unavailable".to_string()));
        }
        let player = SpeechPlayer::spawn().map_err(|error| {
            self.unavailable.store(true, Ordering::Release);
            TtsError::Player(error.to_string())
        })?;
        let playback = self
            .playback
            .clone()
            .ok_or_else(|| TtsError::Player("playback unavailable".to_string()))?;
        let (sender, receiver) = mpsc::sync_channel(256);
        let worker_pending = Arc::clone(&self.pending);
        let worker_stop = Arc::clone(&self.stop);
        let worker_health = Arc::clone(&self.health);
        let thread = thread::Builder::new()
            .name("tts-worker".to_string())
            .spawn(move || {
                run_tts_worker(
                    receiver,
                    playback,
                    player,
                    worker_pending,
                    worker_stop,
                    worker_health,
                )
            })
            .map_err(|error| {
                self.unavailable.store(true, Ordering::Release);
                TtsError::Player(format!("spawn tts-worker: {error}"))
            })?;
        *guard = Some(TtsWorker { sender, thread });
        Ok(())
    }

    /// 异步入队（无语气/风格参数）；只检查配置和队列，不做网络请求。
    pub fn enqueue(&self, config: &TtsConfig, text: &str) -> Result<(), TtsError> {
        self.enqueue_with_options(config, text, TtsSpeakOptions::default())
    }

    /// 异步入队并可携带当前模型支持的语气/风格参数。
    pub fn enqueue_with_options(
        &self,
        config: &TtsConfig,
        text: &str,
        options: TtsSpeakOptions,
    ) -> Result<(), TtsError> {
        if !config.enabled || config.api_key.trim().is_empty() {
            return Err(TtsError::Disabled);
        }
        if config.trigger_scope != TtsTriggerScope::AiChat {
            return Err(TtsError::Disabled);
        }
        let capabilities = tts_model_capabilities(config.effective_model());
        if options.emotion.is_some() && !capabilities.supports_emotion {
            return Err(TtsError::UnsupportedOption("emotion".to_string()));
        }
        if options.style.is_some() && !capabilities.supports_style {
            return Err(TtsError::UnsupportedOption("style".to_string()));
        }
        if options.speed.is_some() && !capabilities.supports_speed {
            return Err(TtsError::UnsupportedOption("speed".to_string()));
        }
        if options.volume_db.is_some() && !capabilities.supports_volume_db {
            return Err(TtsError::UnsupportedOption("volume_db".to_string()));
        }
        let text = truncate_tts_text(text, config.max_chars);
        if text.is_empty() {
            return Err(TtsError::Audio);
        }
        self.ensure_worker()?;
        let max = config.queue_max.max(1);
        if self.pending.fetch_add(1, Ordering::AcqRel) >= max {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(TtsError::QueueFull);
        }
        let sender = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|worker| worker.sender.clone());
        let Some(sender) = sender else {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(TtsError::Disabled);
        };
        match sender.try_send(TtsMessage::Job(TtsJob {
            text,
            config: config.clone(),
            options,
            queued_at: Instant::now(),
        })) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(TtsError::QueueFull)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(TtsError::Disabled)
            }
        }
    }
}

impl Drop for TtsRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return;
        };
        let _ = worker.sender.try_send(TtsMessage::Shutdown);
        if worker.thread.is_finished() {
            let _ = worker.thread.join();
        } else {
            // 不阻塞退出；worker 会在当前 HTTP/播放结束后看到 stop 并退出。
            log::warn!("TTS worker 未在关停时立即结束，已放弃 join 以避免阻塞退出");
            drop(worker.thread);
        }
    }
}

fn run_tts_worker(
    receiver: mpsc::Receiver<TtsMessage>,
    playback: PlaybackHandle,
    mut player: SpeechPlayer,
    pending: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    health: Arc<Mutex<Option<SpeechHealth>>>,
) {
    while let Ok(message) = receiver.recv() {
        match message {
            TtsMessage::Shutdown => break,
            TtsMessage::Job(job) => {
                pending.fetch_sub(1, Ordering::AcqRel);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if !player.is_alive() {
                    log::warn!("TTS SpeechPlayer 失活，尝试 restart 后再处理队列");
                    if let Err(error) = player.restart() {
                        log::warn!("TTS SpeechPlayer restart 失败：{error}");
                    }
                }
                if let Ok(mut health) = health.lock() {
                    *health = Some(player.health());
                }
                if job.queued_at.elapsed() > tts_job_ttl(&job.config) {
                    log::debug!("TTS 队列条目已过期，跳过");
                    continue;
                }
                let result = synthesize_and_speak(&playback, &mut player, &job, &stop);
                if let Err(error) = &result {
                    log::warn!("TTS 语音跳过（文本回复不受影响）：{error}");
                }
                if let Ok(mut health) = health.lock() {
                    *health = Some(player.health());
                }
            }
        }
    }
    player.shutdown();
}

fn tts_play_wait_limit(audio_duration: Duration) -> Duration {
    (audio_duration + Duration::from_secs(2)).max(Duration::from_secs(3))
}

fn tts_job_ttl(config: &TtsConfig) -> Duration {
    // 队列里的旧回复没有播放价值：按一次请求+预估播放时长给 TTL，至少 30s、最多 120s。
    let playback_estimate_ms = (config.max_chars as u64).saturating_mul(150);
    Duration::from_millis(
        config
            .timeout_ms
            .saturating_mul(2)
            .saturating_add(playback_estimate_ms)
            .clamp(30_000, 120_000),
    )
}

fn synthesize_and_speak(
    playback: &PlaybackHandle,
    player: &mut SpeechPlayer,
    job: &TtsJob,
    stop: &AtomicBool,
) -> Result<(), TtsError> {
    let bytes = synthesize_fish(&job.config, &job.text, &job.options)?;
    // 先解码成功，再 engage duck；解码失败不压音乐。
    let pcm = SpeechAudio::Wav(bytes)
        .decode()
        .map_err(|error| TtsError::Player(error.to_string()))?;
    let frames = pcm.samples.len() / usize::from(pcm.channels.max(1));
    let audio_duration = if pcm.sample_rate == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(frames as f64 / f64::from(pcm.sample_rate))
    };
    // 等待上界按实际音频时长 + 2s 收尾；不再用 30s 硬顶截断长语音。
    // 真正的异常由 health/停止标志提前结束；max_chars 已限制文本上限。
    let wait_limit = tts_play_wait_limit(audio_duration);
    let mut duck = DuckGuard::new(playback.clone());
    duck.engage(job.config.duck_volume, job.config.duck_fade_ms);
    if let Err(error) = player.speak(SpeechAudio::Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples: pcm.samples,
    }) {
        duck.restore();
        return Err(TtsError::Player(error.to_string()));
    }
    // 等待语音线程真正开始，再等它结束；同时监听 health，设备异常立即停等恢复 duck。
    let mut health = player.subscribe_health();
    let started = Instant::now();
    while !player.is_speaking() && started.elapsed() < Duration::from_millis(500) {
        if stop.load(Ordering::Acquire) {
            let _ = player.stop();
            duck.restore();
            return Ok(());
        }
        match health.borrow_and_update().clone() {
            SpeechHealth::Healthy => {}
            SpeechHealth::Degraded { reason } => {
                log::warn!("TTS 播放器降级（{reason}），停止等待并恢复 duck");
                let _ = player.stop();
                let _ = player.restart();
                duck.restore();
                return Ok(());
            }
            SpeechHealth::Closed => {
                log::warn!("TTS 播放器已关闭，停止等待并恢复 duck");
                duck.restore();
                return Ok(());
            }
        }
        if started.elapsed() > wait_limit {
            log::warn!("TTS 等待播放启动超过上界（{wait_limit:?}），停止本段语音");
            let _ = player.stop();
            duck.restore();
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    while player.is_speaking() {
        if stop.load(Ordering::Acquire) {
            let _ = player.stop();
            break;
        }
        match health.borrow_and_update().clone() {
            SpeechHealth::Healthy => {}
            SpeechHealth::Degraded { reason } => {
                log::warn!("TTS 播放器降级（{reason}），停止播放并恢复 duck");
                let _ = player.stop();
                let _ = player.restart();
                break;
            }
            SpeechHealth::Closed => {
                log::warn!("TTS 播放器已关闭，停止播放并恢复 duck");
                break;
            }
        }
        if started.elapsed() > wait_limit {
            log::warn!("TTS 播放超过上界（{wait_limit:?}），停止本段语音");
            let _ = player.stop();
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    duck.restore();
    Ok(())
}

struct DuckGuard {
    playback: PlaybackHandle,
    active: bool,
    gain: u8,
    fade_ms: u64,
}

impl DuckGuard {
    fn new(playback: PlaybackHandle) -> Self {
        Self {
            playback,
            active: false,
            gain: 100,
            fade_ms: 200,
        }
    }

    fn engage(&mut self, target: u8, fade_ms: u64) {
        let target = target.min(100);
        self.gain = target;
        self.fade_ms = fade_ms;
        self.active = fade_duck(&self.playback, 100, target, fade_ms);
        if !self.active {
            log::warn!("TTS duck 压低失败，已尝试直接恢复 100");
            force_duck_full(&self.playback);
        }
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        let restored = fade_duck(&self.playback, self.gain, 100, self.fade_ms);
        if !restored {
            log::warn!("TTS duck 恢复渐变失败，直设/重试恢复 100");
            force_duck_full(&self.playback);
        }
        self.active = false;
        self.gain = 100;
    }
}

impl Drop for DuckGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn force_duck_full(playback: &PlaybackHandle) {
    for attempt in 0..3 {
        match playback.set_duck_gain(100) {
            Ok(()) => return,
            Err(error) => {
                log::warn!("TTS duck 直设 100 失败（第 {} 次）：{error}", attempt + 1);
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    // 立即重试仍失败（例如引擎 Busy）：后台看门狗继续重试，避免永久压低。
    let playback = playback.clone();
    let _ = thread::Builder::new()
        .name("tts-duck-watchdog".to_string())
        .spawn(move || {
            for attempt in 0..50 {
                if playback.set_duck_gain(100).is_ok() {
                    log::warn!("TTS duck 看门狗恢复 100 成功（第 {} 次）", attempt + 1);
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
            log::error!("TTS duck 看门狗 10s 内仍无法恢复 100，请检查播放引擎");
        });
}

fn fade_duck(playback: &PlaybackHandle, from: u8, to: u8, fade_ms: u64) -> bool {
    let steps = if fade_ms == 0 {
        1
    } else {
        (fade_ms / 25).max(1) as u32
    };
    let pause = if steps <= 1 {
        Duration::ZERO
    } else {
        Duration::from_millis(fade_ms / u64::from(steps))
    };
    for gain in duck_gain_steps(from, to, steps) {
        if let Err(error) = playback.set_duck_gain(gain) {
            log::warn!("TTS duck 增益设置失败：{error}");
            // 目标不是 100 时立即尝试直接恢复，避免音乐一直压低。
            if to < 100 {
                force_duck_full(playback);
            }
            return false;
        }
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
    true
}

fn synthesize_fish(
    config: &TtsConfig,
    text: &str,
    options: &TtsSpeakOptions,
) -> Result<Vec<u8>, TtsError> {
    let api_key = config.api_key.trim();
    if api_key.is_empty() {
        return Err(TtsError::Disabled);
    }
    let client = build_tts_client(config, Duration::from_millis(config.timeout_ms))
        .map_err(TtsError::Config)?;
    let capabilities = tts_model_capabilities(config.effective_model());
    let text = decorate_speak_text(text, options, &capabilities);
    let format = match config.format {
        TtsFormat::Wav => "wav",
    };
    let mut body = json!({
        "text": text,
        "format": format,
    });
    if let Some(prosody) = speak_prosody_body(options, &capabilities) {
        body["prosody"] = prosody;
    }
    let voice = config.voice_id();
    if capabilities.supports_reference_id && !voice.is_empty() {
        body["reference_id"] = serde_json::Value::String(voice.to_string());
    }
    let response = client
        .post(tts_url(&config.endpoint))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("model", config.effective_model())
        .json(&body)
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                TtsError::Timeout
            } else {
                TtsError::Request(error.to_string())
            }
        })?;
    if let Some(error) = tts_error_for_status(response.status().as_u16()) {
        return Err(error);
    }
    let bytes = response
        .bytes()
        .map_err(|error| TtsError::Request(error.to_string()))?;
    if bytes.is_empty() {
        return Err(TtsError::Audio);
    }
    Ok(bytes.to_vec())
}

/// 兼容旧调用方/测试的队列状态辅助类型。
#[derive(Default)]
pub(crate) struct TtsQueueState {
    queued: VecDeque<()>,
}

impl TtsQueueState {
    pub(crate) fn len(&self) -> usize {
        self.queued.len()
    }

    pub(crate) fn push(&mut self, max: usize) -> bool {
        if self.queued.len() >= max.max(1) {
            return false;
        }
        self.queued.push_back(());
        true
    }

    pub(crate) fn pop(&mut self) -> bool {
        self.queued.pop_front().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_audio_is_not_truncated_by_a_30s_hard_cap() {
        assert_eq!(
            tts_play_wait_limit(Duration::from_secs(45)),
            Duration::from_secs(47)
        );
        assert_eq!(
            tts_play_wait_limit(Duration::ZERO),
            Duration::from_secs(3)
        );
        assert!(!TtsRuntime::disabled().available());
    }

    #[test]
    fn empty_model_falls_back_and_validate_uses_schema_bounds() {
        let mut config = TtsConfig::default();
        config.model.clear();
        assert_eq!(config.effective_model(), DEFAULT_TTS_MODEL);
        config.duck_fade_ms = 2_001;
        assert!(config.validate().is_err());
        config.duck_fade_ms = 0;
        config.max_chars = 2_001;
        assert!(config.validate().is_err());
        config.max_chars = 200;
        config.timeout_ms = 999;
        assert!(config.validate().is_err());
        config.timeout_ms = 1_000;
        config.queue_max = 65;
        assert!(config.validate().is_err());
        config.queue_max = 64;
        assert!(config.validate().is_ok());
        assert!(!TtsCatalog::fallback().truncated);
    }

    #[test]
    fn tts_http_proxy_defaults_roundtrips_and_builds_clients() {
        let config = TtsConfig::default();
        assert!(config.http_proxy.is_empty());
        let encoded = serde_json::to_string(&config).expect("serialize");
        let decoded: TtsConfig = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, config);
        assert!(build_tts_client(&config, Duration::from_secs(1)).is_ok());

        for proxy in ["http://127.0.0.1:7890", "https://proxy.example:8443", "socks5://127.0.0.1:1080"] {
            let mut config = TtsConfig::default();
            config.http_proxy = proxy.to_string();
            config.validate().expect("合法代理");
            assert!(
                build_tts_client(&config, Duration::from_secs(1)).is_ok(),
                "代理应能构建 client: {proxy}"
            );
        }

        let mut invalid = TtsConfig::default();
        invalid.http_proxy = "not a proxy".to_string();
        let error = invalid.validate().expect_err("无效代理");
        assert!(error.to_string().contains("tts.http_proxy"));
        assert!(build_tts_client(&invalid, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn tts_defaults_match_p0_contract() {
        let config = TtsConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.endpoint, DEFAULT_TTS_ENDPOINT);
        assert_eq!(config.model, DEFAULT_TTS_MODEL);
        assert_eq!(config.duck_volume, 30);
        assert_eq!(config.duck_fade_ms, 200);
        assert_eq!(config.max_chars, 200);
        assert_eq!(config.timeout_ms, 8_000);
        assert_eq!(config.queue_max, 8);
        assert_eq!(config.trigger_scope, TtsTriggerScope::AiChat);
    }

    #[test]
    fn tts_text_truncation_and_url_are_safe() {
        assert_eq!(truncate_tts_text("  你好世界  ", 2), "你好");
        assert_eq!(truncate_tts_text("", 10), "");
        assert_eq!(tts_url("https://api.fish.audio/"), "https://api.fish.audio/v1/tts");
        assert_eq!(
            tts_url("https://gateway.example/v1/tts"),
            "https://gateway.example/v1/tts"
        );
    }

    #[test]
    fn tts_http_status_mapping_covers_errors() {
        assert!(tts_error_for_status(200).is_none());
        for status in [401, 403, 404, 429, 500, 503] {
            assert!(matches!(
                tts_error_for_status(status),
                Some(TtsError::Http(value)) if value == status
            ));
        }
    }

    #[test]
    fn duck_gain_steps_end_at_target_and_are_bounded() {
        let steps = duck_gain_steps(100, 30, 8);
        assert_eq!(steps.last().copied(), Some(30));
        assert!(steps.iter().all(|gain| *gain <= 100));
        assert_eq!(duck_gain_steps(100, 100, 4), vec![100, 100, 100, 100]);
    }

    #[test]
    fn tts_queue_is_bounded() {
        let mut queue = TtsQueueState::default();
        assert!(queue.push(1));
        assert!(!queue.push(1));
        assert!(queue.pop());
        assert!(queue.push(1));
    }

    #[test]
    fn tts_debug_redacts_api_key_and_validation_rejects_bad_ranges() {
        let mut config = TtsConfig::default();
        config.api_key = "secret-fish-key".to_string();
        assert!(!format!("{config:?}").contains("secret-fish-key"));
        config.duck_volume = 101;
        assert!(config.validate().is_err());
        config.duck_volume = 30;
        config.queue_max = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn voice_catalog_parses_id_title_state_and_visibility() {
        let body = r#"{
            "total": 2,
            "items": [
                {"_id": "voice-a", "title": "音色甲", "state": "trained", "visibility": "private"},
                {"id": "voice-b", "title": "", "state": "created", "visibility": "public"}
            ]
        }"#;
        let voices = parse_voice_catalog(body).unwrap();
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].id, "voice-a");
        assert_eq!(voices[0].title, "音色甲");
        assert_eq!(voices[0].state, "trained");
        assert_eq!(voices[1].id, "voice-b");
        assert_eq!(voices[1].title, "voice-b");
        assert!(parse_voice_catalog(r#"{"total":0,"items":[]}"#).unwrap().is_empty());
        assert!(parse_voice_catalog("not json").is_err());
        assert!(parse_voice_catalog(r#"{"total":0}"#).is_err());
        assert!(parse_voice_catalog(r#"{"items":[{"title":"无 id"}]}"#).is_err());
    }

    #[test]
    fn fallback_catalog_marks_models_as_fallback_and_covers_official_models() {
        let catalog = TtsCatalog::fallback();
        assert_eq!(catalog.source, TtsCatalogSource::Fallback);
        assert_eq!(catalog.model_source, TtsCatalogSource::Fallback);
        for model in ["s1", "s2-pro", "s2.1-pro", "s2.1-pro-free", "drama-3"] {
            assert!(catalog.models.iter().any(|item| item == model), "缺少 {model}");
        }
        assert!(catalog.voices.is_empty());
    }

    #[test]
    fn empty_key_returns_fallback_without_network() {
        let mut config = TtsConfig::default();
        config.enabled = true;
        config.api_key.clear();
        let catalog = fetch_catalog(&config, &TtsCatalogOverrides::default()).unwrap();
        assert_eq!(catalog.source, TtsCatalogSource::Fallback);
        assert!(catalog.voices.is_empty());
        assert!(!catalog.models.is_empty());
    }

    #[test]
    fn catalog_errors_do_not_contain_key_or_text() {
        let error = TtsCatalogError::Config("tts.api_key 不能为空".to_string());
        let text = format!("{error:?} {error}");
        assert!(!text.contains("secret-fish-key"));
    }

    #[test]
    fn model_capabilities_cover_each_official_model_and_unknown_fallback() {
        let s1 = tts_model_capabilities("s1");
        assert!(s1.known);
        assert_eq!(s1.style_mode, TtsStyleMode::Parentheses);
        assert!(s1.supports_emotion);
        assert!(!s1.supports_style);
        assert!(s1.supports_normalize && s1.supports_chunk_length);
        assert!(s1.supports_latency && s1.supports_sampling);
        assert!(s1.supports_reference_id);

        for model in ["s2-pro", "s2.1-pro", "s2.1-pro-free"] {
            let caps = tts_model_capabilities(model);
            assert!(caps.known, "{model}");
            assert_eq!(caps.style_mode, TtsStyleMode::Brackets);
            assert!(caps.supports_style);
        }
        let drama = tts_model_capabilities("drama-3");
        assert!(drama.known);
        assert_eq!(drama.style_mode, TtsStyleMode::DramaDirections);
        assert!(drama.supports_phoneme);
        assert!(drama.supports_multi_speaker);
        assert!(drama.supports_paired_tags);
        assert!(drama.supports_language_tags);
        let s2 = tts_model_capabilities("s2.1-pro-free");
        assert!(!s2.supports_paired_tags && !s2.supports_language_tags);

        let unknown = tts_model_capabilities("unknown-model");
        assert!(!unknown.known);
        assert_eq!(unknown.style_mode, TtsStyleMode::None);
        assert!(!unknown.supports_emotion && !unknown.supports_style);
        assert!(!unknown.supports_speed && !unknown.supports_volume_db);
        assert!(!unknown.supports_normalize && !unknown.supports_reference_id);
    }

    #[test]
    fn speak_options_map_to_text_markers_and_prosody() {
        let s1 = tts_model_capabilities("s1");
        let options = TtsSpeakOptions {
            emotion: Some("angry".to_string()),
            style: None,
            speed: Some(1.0),
            volume_db: Some(3.0),
        };
        assert_eq!(decorate_speak_text("你好", &options, &s1), "(angry) 你好");
        assert_eq!(
            speak_prosody_body(&options, &s1),
            Some(json!({"speed": 1.0, "volume": 3.0}))
        );

        let s2 = tts_model_capabilities("s2.1-pro");
        let options = TtsSpeakOptions {
            emotion: Some("laugh".to_string()),
            style: Some("温柔".to_string()),
            speed: None,
            volume_db: None,
        };
        assert_eq!(
            decorate_speak_text("你好", &options, &s2),
            "[温柔] [laugh] 你好"
        );
        assert!(speak_prosody_body(&options, &s2).is_none());
    }

    #[test]
    fn disabled_runtime_never_queues_or_calls_network() {
        let runtime = TtsRuntime::disabled();
        let config = TtsConfig::default();
        assert!(matches!(
            runtime.enqueue(&config, "你好"),
            Err(TtsError::Disabled)
        ));
    }
}
