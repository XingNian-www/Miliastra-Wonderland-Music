//! 统一 AI 连接配置：点歌、歌曲审核、海龟汤共用同一套
//! provider / API Key / endpoint / 代理 / 兼容字段。
//!
//! 功能差异（提示词、是否联网审核、输出上限）留在各自功能段；
//! 模型名可按功能覆盖，留空时沿用 `ai.model`。
//!
//! 调用默认走 OpenAI Responses API。个别网关（Kimi、GLM、部分自建网关）
//! 只实现 Chat Completions，可用 `ai.wire_api` 单独切回。

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequest, CreateChatCompletionRequestArgs, ResponseFormat,
};
use async_openai::types::responses::{
    CreateResponse, CreateResponseArgs, OutputItem, OutputMessageContent, Response,
    ResponseTextParam, TextResponseFormatConfiguration,
};
use miliastra_kernel::ai::{Authentication, Target, validate_http_proxy};

const MIMO_ENDPOINT: &str = "https://api.xiaomimimo.com/v1/chat/completions";
const MIMO_MODEL: &str = "mimo-v2.5";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const OPENAI_MODEL: &str = "gpt-5.6-mini";
pub(crate) const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";
pub(crate) const DEEPSEEK_MODEL: &str = "deepseek-v4-flash";
const CHAT_COMPLETIONS_SUFFIX: &str = "/chat/completions";
const RESPONSES_SUFFIX: &str = "/responses";

/// 线上协议。默认 Responses，只给确实不支持的网关留退路。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    #[default]
    Responses,
    ChatCompletions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiProvider {
    Mimo,
    OpenAi,
    DeepSeek,
    Custom,
}

/// 思考强度。OpenAI 兼容网关把它放在 `reasoning.effort`（Responses）
/// 或 `reasoning_effort`（Chat Completions）里。
///
/// 单独建模而不是让用户手写 extra_body，是为了：
/// - 面板能直接给出下拉选项，不必知道 JSON 结构；
/// - 两个线上协议的字段名差异由这里收敛；
/// - 取值有校验，写错不会静默失效。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// 不显式发送，由服务端默认决定。
    #[default]
    Unset,
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// 调用请求体里的字段值；Unset 返回 None（不发送该字段）。
    pub(crate) fn as_wire_value(self) -> Option<&'static str> {
        match self {
            Self::Unset => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }

    /// 从线上值反查；无法识别时视为不发送。
    fn from_wire_value(value: &str) -> Self {
        match value {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            _ => Self::Unset,
        }
    }
}

/// 一条思考强度适配规则。
///
/// 不同模型接受的 effort 词不同：有的只认 low/medium/high，收到 minimal 会 400。
/// 这里按模型 ID 把源值改写成目标值，或直接拒绝（`deny`）以免发出必失败的请求。
/// 模型匹配支持精确、前缀、后缀——与常见网关的命名习惯一致
/// （如 `o1-preview`、`gpt-5-*`、`*-reasoning`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningRule {
    /// 匹配的源 effort 值（low/medium/high/minimal）。
    pub from: String,
    /// 改写目标值；`deny` 表示该模型不支持此强度。
    pub to: String,
    /// 模型匹配方式：exact / prefix / suffix；留空表示适用所有模型。
    #[serde(default)]
    pub match_type: String,
    /// 与 match_type 配套的模型片段；留空表示适用所有模型。
    #[serde(default)]
    pub model: String,
}

impl ReasoningRule {
    /// 该规则是否适用于这个模型。
    fn applies_to(&self, model: &str) -> bool {
        let needle = self.model.trim();
        if needle.is_empty() {
            return true;
        }
        let model = model.trim();
        match self.match_type.trim().to_ascii_lowercase().as_str() {
            "exact" => model == needle,
            "prefix" | "" => model.starts_with(needle),
            "suffix" => model.ends_with(needle),
            _ => false,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.from.trim().is_empty() || self.to.trim().is_empty() {
            bail!("ai.reasoning_rules 的 from/to 不能为空");
        }
        if !matches!(self.from.trim(), "minimal" | "low" | "medium" | "high") {
            bail!(
                "ai.reasoning_rules.from 只允许 minimal/low/medium/high，当前为：{}",
                self.from
            );
        }
        let to = self.to.trim();
        if to != "deny" && !matches!(to, "minimal" | "low" | "medium" | "high") {
            bail!("ai.reasoning_rules.to 只允许 minimal/low/medium/high/deny，当前为：{to}");
        }
        let kind = self.match_type.trim().to_ascii_lowercase();
        if !kind.is_empty() && !matches!(kind.as_str(), "exact" | "prefix" | "suffix") {
            bail!("ai.reasoning_rules.match_type 只允许 exact/prefix/suffix");
        }
        Ok(())
    }
}

/// 按规则改写本模型的 effort。
///
/// 返回 `Ok(None)` 表示规则要求拒绝（不发送该字段或整体不调），
/// `Ok(Some(effort))` 表示最终要发送的值。第一条命中的规则生效。
fn apply_reasoning_rules(
    model: &str,
    effort: ReasoningEffort,
    rules: &[ReasoningRule],
) -> std::result::Result<Option<&'static str>, String> {
    let Some(current) = effort.as_wire_value() else {
        return Ok(None);
    };
    for rule in rules {
        if rule.from.trim() != current || !rule.applies_to(model) {
            continue;
        }
        let to = rule.to.trim();
        if to == "deny" {
            return Err(format!(
                "模型 {model} 不接受思考强度 {current}，请在配置中改用其他强度"
            ));
        }
        return Ok(Some(match to {
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            other => {
                return Err(format!("未被识别的目标思考强度：{other}"));
            }
        }));
    }
    Ok(Some(current))
}

/// 单次调用的可变参数：模型与思考强度。/// 单次调用的可变参数：模型与思考强度。
///
/// 每个调用点各自构造一个，因此"点歌用高思考、审核用低思考"这种组合不需要
/// 全局开关，也不会互相影响。空字符串模型表示沿用 `ai.model`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AiCallOptions {
    pub(crate) model: String,
    pub(crate) reasoning: ReasoningEffort,
}

impl AiCallOptions {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            reasoning: ReasoningEffort::default(),
        }
    }

    pub(crate) fn with_reasoning(mut self, reasoning: ReasoningEffort) -> Self {
        self.reasoning = reasoning;
        self
    }
}

/// 唯一的 AI 连接配置段。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    /// Chat Completions 地址；Responses 地址由它推导。
    #[serde(default)]
    pub endpoint: String,
    /// 可选的 Responses 地址；留空时把 endpoint 的 /chat/completions 换成 /responses。
    #[serde(default)]
    pub responses_endpoint: String,
    pub model: String,
    #[serde(default)]
    pub http_proxy: String,
    #[serde(default)]
    pub wire_api: WireApi,
    pub extra_body: HashMap<String, Value>,
    /// 思考强度适配规则：按模型改写或拒绝某个 effort 值。
    ///
    /// 不同模型接受的词不同，写错会 400；这里让用户按模型前缀/后缀兜住差异，
    /// 而不必为每个模型单独记一套可用值。
    #[serde(default)]
    pub reasoning_rules: Vec<ReasoningRule>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            api_key: String::new(),
            endpoint: String::new(),
            responses_endpoint: String::new(),
            model: OPENAI_MODEL.to_string(),
            http_proxy: String::new(),
            wire_api: WireApi::default(),
            extra_body: HashMap::new(),
            reasoning_rules: Vec::new(),
        }
    }
}

/// 解析后的单次调用目标：三个功能拿到的都是同一套连接参数。
#[derive(Clone, Debug)]
pub(crate) struct ResolvedAiTarget {
    pub(crate) provider: AiProvider,
    pub(crate) endpoint: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) wire_api: WireApi,
    pub(crate) extra_body: HashMap<String, Value>,
    /// 本次调用的思考强度；Unset 表示不发该字段。
    pub(crate) reasoning: ReasoningEffort,
}

impl ResolvedAiTarget {
    pub(crate) fn authentication(&self) -> Authentication {
        authentication(self.provider)
    }

    /// 按当前线上协议构造 kernel 端点，失败即配置无效。
    pub(crate) fn build_target(&self) -> Result<Target> {
        match self.wire_api {
            WireApi::Responses => Target::responses(&self.endpoint, &self.api_key),
            WireApi::ChatCompletions => {
                Target::chat(&self.endpoint, &self.api_key, self.authentication())
            }
        }
        .context("AI 端点配置无效")
    }
}

impl AiConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_http_proxy(&self.http_proxy).context("ai.http_proxy 配置无效")?;
        if normalize_optional_text(&self.provider, "provider")?.is_empty() {
            bail!("ai.provider 不能为空");
        }
        let provider = parse_provider(&self.provider)?;
        // 未配置密钥表示所有 AI 功能关闭，只校验形状与代理。
        if self.api_key.trim().is_empty() {
            return Ok(());
        }
        let target = self.resolve("")?;
        target.build_target()?;
        for rule in &self.reasoning_rules {
            rule.validate()?;
        }
        validate_extra_body(provider, &self.extra_body)
    }

    pub(crate) fn resolve(&self, feature_model: &str) -> Result<ResolvedAiTarget> {
        self.resolve_with(&AiCallOptions::new(feature_model))
    }

    /// 按调用点选项解析目标：模型与思考强度都在这里生效。
    pub(crate) fn resolve_with(&self, options: &AiCallOptions) -> Result<ResolvedAiTarget> {
        let provider = parse_provider(&self.provider)?;
        let wire_api = self.wire_api;
        let endpoint = match wire_api {
            WireApi::Responses => {
                resolve_responses_endpoint(provider, &self.endpoint, &self.responses_endpoint)?
            }
            WireApi::ChatCompletions => resolve_endpoint(provider, &self.endpoint)?,
        };
        let model = resolve_model(provider, effective_model(self, &options.model))?;
        // 规则按最终模型名匹配：模型可能来自功能覆盖或 ai.model，二者都要适用。
        let reasoning =
            match apply_reasoning_rules(&model, options.reasoning, &self.reasoning_rules) {
                Ok(Some(wire)) => ReasoningEffort::from_wire_value(wire),
                Ok(None) => ReasoningEffort::Unset,
                Err(message) => bail!("{message}"),
            };
        Ok(ResolvedAiTarget {
            provider,
            endpoint,
            api_key: normalize_api_key(&self.api_key)?,
            model,
            wire_api,
            extra_body: self.extra_body.clone(),
            reasoning,
        })
    }
}

pub(crate) fn authentication(provider: AiProvider) -> Authentication {
    if provider == AiProvider::Mimo {
        Authentication::ApiKey
    } else {
        Authentication::Bearer
    }
}

pub(crate) fn parse_provider(value: &str) -> Result<AiProvider> {
    match normalize_optional_text(value, "provider")?
        .to_ascii_lowercase()
        .as_str()
    {
        "mimo" => Ok(AiProvider::Mimo),
        "openai" => Ok(AiProvider::OpenAi),
        "deepseek" => Ok(AiProvider::DeepSeek),
        "custom" => Ok(AiProvider::Custom),
        _ => bail!("ai.provider 只允许 mimo/openai/deepseek/custom"),
    }
}

/// 功能未单独指定模型时沿用 `ai.model`。
pub(crate) fn effective_model<'a>(config: &'a AiConfig, feature_model: &'a str) -> &'a str {
    let feature_model = feature_model.trim();
    if feature_model.is_empty() {
        config.model.as_str()
    } else {
        feature_model
    }
}

pub(crate) fn resolve_endpoint(provider: AiProvider, value: &str) -> Result<String> {
    let text = normalize_optional_text(value, "endpoint")?;
    let endpoint = if text.is_empty() {
        match provider {
            AiProvider::Mimo => MIMO_ENDPOINT,
            AiProvider::OpenAi => OPENAI_ENDPOINT,
            AiProvider::DeepSeek => DEEPSEEK_ENDPOINT,
            AiProvider::Custom => bail!("custom provider 缺少 ai.endpoint"),
        }
        .to_string()
    } else {
        text
    };
    ensure_http_url(&endpoint, "ai.endpoint")?;
    Ok(endpoint)
}

fn resolve_responses_endpoint(
    provider: AiProvider,
    chat_endpoint: &str,
    configured_responses: &str,
) -> Result<String> {
    let configured = normalize_optional_text(configured_responses, "responses_endpoint")?;
    if !configured.is_empty() {
        ensure_http_url(&configured, "ai.responses_endpoint")?;
        return Ok(configured);
    }
    let chat = resolve_endpoint(provider, chat_endpoint)?;
    let Some(base) = chat.strip_suffix(CHAT_COMPLETIONS_SUFFIX) else {
        bail!(
            "ai.endpoint 无法推导 Responses 地址：请填写以 /chat/completions 结尾的地址，或直接填写 ai.responses_endpoint"
        );
    };
    Ok(format!("{base}{RESPONSES_SUFFIX}"))
}

fn ensure_http_url(value: &str, name: &str) -> Result<()> {
    if !value.starts_with("https://") && !value.starts_with("http://") {
        bail!("{name} 必须以 http:// 或 https:// 开头")
    }
    Ok(())
}

pub(crate) fn resolve_model(provider: AiProvider, value: &str) -> Result<String> {
    // 模型名会进入请求体与日志，不接受任何控制字符（含换行/制表）。
    let text = normalize_token(value, "model")?;
    if !text.is_empty() {
        return Ok(text);
    }
    Ok(match provider {
        AiProvider::Mimo => MIMO_MODEL,
        AiProvider::OpenAi => OPENAI_MODEL,
        AiProvider::DeepSeek => DEEPSEEK_MODEL,
        AiProvider::Custom => bail!("custom provider 缺少 ai.model"),
    }
    .to_string())
}

pub(crate) fn validate_extra_body(
    provider: AiProvider,
    extra_body: &HashMap<String, Value>,
) -> Result<()> {
    let optional_string = |key: &str, max_len: usize| -> Result<()> {
        let Some(value) = extra_body.get(key).filter(|value| !value.is_null()) else {
            return Ok(());
        };
        let text = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("ai.extra_body.{key} 必须是字符串"))?;
        if text.chars().count() > max_len {
            bail!("ai.extra_body.{key} 不能超过 {max_len} 个字符");
        }
        Ok(())
    };
    match provider {
        AiProvider::OpenAi => {
            optional_string("safety_identifier", 64)?;
            if let Some(metadata) = extra_body.get("metadata").filter(|value| !value.is_null()) {
                let object = metadata
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("ai.extra_body.metadata 必须是 JSON object"))?;
                if object.len() > 16 {
                    bail!("ai.extra_body.metadata 不能超过 16 项");
                }
                for (key, value) in object {
                    if key.chars().count() > 64 {
                        bail!("ai.extra_body.metadata 的键不能超过 64 个字符");
                    }
                    let text = value.as_str().ok_or_else(|| {
                        anyhow::anyhow!("ai.extra_body.metadata 的值必须是字符串")
                    })?;
                    if text.chars().count() > 512 {
                        bail!("ai.extra_body.metadata 的值不能超过 512 个字符");
                    }
                }
            }
        }
        AiProvider::DeepSeek => {
            optional_string("user_id", 512)?;
            if let Some(user_id) = extra_body.get("user_id").and_then(Value::as_str)
                && !user_id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            {
                bail!("ai.extra_body.user_id 只允许 a-zA-Z0-9-_");
            }
        }
        AiProvider::Mimo | AiProvider::Custom => {}
    }
    Ok(())
}

/// API Key 会写进 Authorization/api-key 请求头，必须只含可打印字符。
fn normalize_api_key(value: &str) -> Result<String> {
    let text = normalize_token(value, "apiKey")?;
    if text.is_empty() {
        bail!("缺少 apiKey 参数")
    }
    Ok(text)
}

/// 单行 token：拒绝所有控制字符（含 \n / \r / \t）。
fn normalize_token(value: &str, name: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("{name} 参数不能包含控制字符")
    }
    Ok(value.trim().to_string())
}

fn normalize_optional_text(value: &str, name: &str) -> Result<String> {
    Ok(assert_no_control_chars(value, name)?.trim().to_string())
}

fn normalize_required(value: &str, name: &str) -> Result<String> {
    let text = assert_no_control_chars(value, name)?.trim().to_string();
    if text.is_empty() {
        bail!("缺少 {name} 参数")
    }
    Ok(text)
}

fn assert_no_control_chars<'a>(value: &'a str, name: &str) -> Result<&'a str> {
    if value
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        bail!("{name} 参数包含控制字符")
    }
    Ok(value)
}

/// 一次非流式 JSON 调用：三个功能共用同一套端点、鉴权与兼容字段。
///
/// 统一走 Responses；若配置为 chat_completions，则退化为 Chat Completions
/// （此时结构化输出只能依赖 json_object，调用方仍需自行校验字段）。
pub(crate) fn request_json(
    openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
    config: &AiConfig,
    options: &AiCallOptions,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    request_with_format(
        openai,
        config,
        options,
        system_prompt,
        user_prompt,
        max_tokens,
        timeout,
        true,
    )
}

/// 同 `request_json`，但可以选择是否强制 JSON 输出。
///
/// `json_mode` 为 false 时必须传——强制 JSON 会要求模型输出一个 JSON 对象，
/// 而「把聊天记录压成一段话」要的是**自然语言摘要**。真机上表现为每次压缩都
/// 400 invalid_request_error，原文因此永远不被压缩、每轮重试一次注定失败的调用。
pub(crate) fn request_plain_text(
    openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
    config: &AiConfig,
    options: &AiCallOptions,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    request_with_format(
        openai,
        config,
        options,
        system_prompt,
        user_prompt,
        max_tokens,
        timeout,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn request_with_format(
    openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
    config: &AiConfig,
    options: &AiCallOptions,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
    json_mode: bool,
) -> anyhow::Result<String> {
    let target = config.resolve_with(options)?;
    let reasoning = target.reasoning;
    match request_json_once(
        openai,
        &target,
        system_prompt,
        user_prompt,
        max_tokens,
        timeout,
        json_mode,
    ) {
        Ok(text) => Ok(text),
        Err(error) => {
            // 兜底降级：某些模型不接受 reasoning 字段，会以 400 拒绝整个请求。
            // 与其让用户去猜该配什么值，不如去掉该字段重试一次——这也是
            // 中转服务普遍采用的兼容策略。只降级一次，避免掩盖真实错误。
            if reasoning == ReasoningEffort::Unset || !looks_like_reasoning_rejection(&error) {
                return Err(error);
            }
            log::warn!(
                "模型 {} 拒绝了思考强度 {}，已去掉该字段重试: {error}",
                target.model,
                reasoning.as_wire_value().unwrap_or("unset")
            );
            let mut without = target.clone();
            without.reasoning = ReasoningEffort::Unset;
            request_json_once(
                openai,
                &without,
                system_prompt,
                user_prompt,
                max_tokens,
                timeout,
                json_mode,
            )
        }
    }
}

/// 判断错误是否指向 reasoning 字段。
///
/// 只认明确提到该字段的错误，避免把"模型不存在""余额不足"这类
/// 与思考强度无关的失败也重试一遍。
fn looks_like_reasoning_rejection(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    const REASONING_HINTS: [&str; 5] = [
        "reasoning",
        "reasoning_effort",
        "thinking",
        "unsupported parameter",
        "invalid parameter",
    ];
    // "invalid parameter" 单独出现太宽泛，必须同时出现 reasoning 相关词。
    REASONING_HINTS[..3].iter().any(|hint| text.contains(hint))
        || (text.contains("parameter") && (text.contains("reasoning") || text.contains("thinking")))
}

fn request_json_once(
    openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
    target: &ResolvedAiTarget,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
    json_mode: bool,
) -> anyhow::Result<String> {
    match target.wire_api {
        WireApi::Responses => {
            let request =
                build_responses_request(target, system_prompt, user_prompt, max_tokens, json_mode)?;
            let response = openai
                .create_response(
                    target.build_target()?,
                    request,
                    &merged_extra_body(target),
                    timeout,
                )?
                .wait()
                .context("AI 请求失败")?;
            responses_output_text(&response)
        }
        WireApi::ChatCompletions => {
            let request =
                build_chat_request(target, system_prompt, user_prompt, max_tokens, json_mode)?;
            let response = openai
                .chat_completion(
                    target.build_target()?,
                    request,
                    &merged_extra_body(target),
                    timeout,
                )?
                .wait()
                .context("AI 请求失败")?;
            // 缓存命中率只能靠服务端回传的 usage 判断，本地估算不可靠。
            // 注意 DeepSeek 把命中数放在 usage 顶层 prompt_cache_hit_tokens，
            // 而 SDK 的 CompletionUsage 没有 flatten，会静默丢弃——
            // 所以这里打印整个 usage 的 JSON，而不是只取 cached_tokens。
            if let Some(usage) = response.usage.as_ref() {
                log::debug!(
                    "AI usage: {}",
                    serde_json::to_string(usage).unwrap_or_else(|_| "<unserializable>".to_string())
                );
            }
            let choice = response
                .choices
                .first()
                .ok_or_else(|| anyhow::anyhow!("AI 响应缺少 choices[0]"))?;
            choice
                .message
                .content
                .as_deref()
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("AI 响应缺少 choices[0].message.content"))
        }
    }
}

/// 把思考强度合进 extra_body。
///
/// 两个线上协议字段名不同：Responses 用嵌套的 reasoning.effort，
/// Chat Completions 用扁平的 reasoning_effort。用户显式写进 extra_body 的
/// 同名字段优先，避免被这里覆盖。
pub(crate) fn merged_extra_body(target: &ResolvedAiTarget) -> HashMap<String, Value> {
    let mut body = target.extra_body.clone();
    let Some(effort) = target.reasoning.as_wire_value() else {
        return body;
    };
    match target.wire_api {
        WireApi::Responses => {
            let entry = body
                .entry("reasoning".to_string())
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(object) = entry.as_object_mut() {
                object
                    .entry("effort".to_string())
                    .or_insert_with(|| Value::String(effort.to_string()));
            }
        }
        WireApi::ChatCompletions => {
            body.entry("reasoning_effort".to_string())
                .or_insert_with(|| Value::String(effort.to_string()));
        }
    }
    body
}

fn build_responses_request(
    target: &ResolvedAiTarget,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    json_mode: bool,
) -> anyhow::Result<CreateResponse> {
    Ok(CreateResponseArgs::default()
        .model(target.model.clone())
        .instructions(system_prompt)
        .input(user_prompt)
        // 只有确实要求 JSON 输出的调用才设 format。
        // 摘要要的是自然语言，强制 JsonObject 会被服务端 400 拒绝。
        .text(ResponseTextParam {
            format: if json_mode {
                TextResponseFormatConfiguration::JsonObject
            } else {
                TextResponseFormatConfiguration::Text
            },
            verbosity: None,
        })
        .temperature(0.1_f32)
        .max_output_tokens(max_tokens)
        .store(false)
        .stream(false)
        .build()?)
}

fn build_chat_request(
    target: &ResolvedAiTarget,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    json_mode: bool,
) -> anyhow::Result<CreateChatCompletionRequest> {
    Ok(CreateChatCompletionRequestArgs::default()
        .model(target.model.clone())
        .messages(vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system_prompt)
                .build()?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(user_prompt)
                .build()?
                .into(),
        ])
        // 同上：摘要要自然语言，不能强制 JSON。
        .response_format(if json_mode {
            ResponseFormat::JsonObject
        } else {
            ResponseFormat::Text
        })
        .temperature(0.1_f32)
        .stream(false)
        .store(false)
        .max_completion_tokens(max_tokens)
        .build()?)
}

/// Responses 的 output_text：跳过 refusal、未完成与缺失内容。
fn responses_output_text(response: &Response) -> anyhow::Result<String> {
    for output in &response.output {
        let OutputItem::Message(message) = output else {
            continue;
        };
        for content in &message.content {
            match content {
                OutputMessageContent::OutputText(text) => {
                    let text = text.text.trim();
                    if !text.is_empty() {
                        return Ok(text.to_string());
                    }
                }
                OutputMessageContent::Refusal(refusal) => {
                    bail!("AI 拒绝回答: {}", refusal.refusal)
                }
            }
        }
    }
    bail!("AI 响应缺少 output_text")
}

/// 拉取模型列表的失败分类。
///
/// 配置形状问题用户按原文能自己改，上游或网络问题不能；混在一起会让路由
/// 只能二选一——之前两者都落到"内部错误"，把可操作的提示吞掉了。
#[derive(Debug)]
pub(crate) enum ModelListError {
    /// provider、endpoint 后缀、apiKey、代理等配置形状不对。
    Config(String),
    /// 请求已发出但失败：网络、上游状态码、响应形状。
    Request(String),
}

impl std::fmt::Display for ModelListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) | Self::Request(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ModelListError {}

/// 从服务端拉取可用模型列表。
///
/// 走 `GET {base}/models`：base 由当前线上协议的调用地址去掉后缀得到，
/// 因此 Responses 与 Chat Completions 两种配置都能用同一个接口。
/// 只读，不改任何配置；失败时把服务端原文带给调用方，便于排查网关差异。
pub(crate) fn fetch_models(config: &AiConfig) -> std::result::Result<Vec<String>, ModelListError> {
    let provider = parse_provider(&config.provider).map_err(config_error)?;
    let base = models_base_url(provider, config).map_err(config_error)?;
    let url = format!("{base}/models");
    ensure_http_url(&url, "models 地址").map_err(config_error)?;
    let api_key = normalize_api_key(&config.api_key).map_err(config_error)?;

    let client = build_model_list_client(&config.http_proxy).map_err(config_error)?;
    let mut request = client.get(&url);
    // 与 kernel 的鉴权方式保持一致：Mimo 用 api-key，其余用 Bearer。
    request = match authentication(provider) {
        Authentication::ApiKey => request.header("api-key", &api_key),
        Authentication::Bearer => request.bearer_auth(&api_key),
    };
    let response = request.send().map_err(|error| {
        ModelListError::Request(format!("请求模型列表失败（检查 endpoint 与代理）：{error}"))
    })?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| ModelListError::Request(format!("读取模型列表响应失败：{error}")))?;
    if !status.is_success() {
        return Err(ModelListError::Request(format!(
            "模型列表请求失败（HTTP {}）：{}",
            status.as_u16(),
            body.chars().take(300).collect::<String>()
        )));
    }
    parse_model_list(&body).map_err(|error| ModelListError::Request(error.to_string()))
}

fn config_error(error: anyhow::Error) -> ModelListError {
    ModelListError::Config(error.to_string())
}

/// 推导 `GET /models` 的 base，必须与真正发请求的端点保持一致。
///
/// Responses 配置允许只填 `ai.responses_endpoint`（此时 `ai.endpoint` 留空也能
/// 正常调用 AI），只按 chat 地址推导会让这类配置在拉列表时报"缺少 ai.endpoint"。
fn models_base_url(provider: AiProvider, config: &AiConfig) -> Result<String> {
    let endpoint = match config.wire_api {
        WireApi::Responses => {
            resolve_responses_endpoint(provider, &config.endpoint, &config.responses_endpoint)?
        }
        WireApi::ChatCompletions => resolve_endpoint(provider, &config.endpoint)?,
    };
    // kernel 的 normalize_endpoint 会先去掉结尾的 /，所以带尾斜杠的配置能正常调用；
    // 这里必须同样处理，否则"AI 能用、拉列表报错"。
    let trimmed = endpoint.trim_end_matches('/');
    let base = trimmed
        .strip_suffix(CHAT_COMPLETIONS_SUFFIX)
        .or_else(|| trimmed.strip_suffix(RESPONSES_SUFFIX))
        .with_context(|| {
            format!(
                "拉取模型列表需要以 /chat/completions 或 /responses 结尾的地址，当前为：{endpoint}"
            )
        })?;
    Ok(base.to_string())
}

/// 兼容两种常见响应形状：
/// - OpenAI：`{"data":[{"id":"gpt-..."},...]}`
/// - 少数网关：`{"models":["a","b"]}` 或裸数组
fn parse_model_list(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body).context("模型列表响应不是合法 JSON")?;
    let mut models = Vec::new();
    let mut push = |id: &str| {
        let id = id.trim();
        if !id.is_empty() && !id.chars().any(char::is_control) {
            models.push(id.to_string());
        }
    };
    match &value {
        Value::Object(object) => {
            if let Some(items) = object.get("data").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        push(id);
                    }
                }
            } else if let Some(items) = object.get("models").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item.as_str() {
                        push(id);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(id) = item.as_str() {
                    push(id);
                }
            }
        }
        _ => {}
    }
    models.sort();
    models.dedup();
    if models.is_empty() {
        bail!("模型列表为空或响应格式无法识别");
    }
    Ok(models)
}

/// 模型列表是一次短请求，单独建 client 以套用代理与 User-Agent。
///
/// reqwest 默认不发 User-Agent，部分网关会因此直接 403。
fn build_model_list_client(proxy: &str) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!(
            "miliastra-wonderland-music/",
            env!("CARGO_PKG_VERSION")
        ));
    let proxy = proxy.trim();
    if !proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("ai.http_proxy 配置无效")?);
    }
    builder.build().context("构建模型列表客户端失败")
}

/// One native tool conversation. History is replayed in full, including Responses
/// reasoning items, so this does not depend on server-side response storage.
pub(crate) struct ToolSession {
    target: ResolvedAiTarget,
    http_proxy: String,
    system: String,
    tools: Vec<Value>,
    names: std::collections::HashSet<String>,
    history: Vec<Value>,
    seen: std::collections::HashSet<String>,
    pending: std::collections::HashSet<String>,
}

#[derive(Debug)]
pub(crate) struct AgentReply {
    pub(crate) text: String,
    pub(crate) calls: Vec<AgentToolCall>,
}

#[derive(Debug)]
pub(crate) struct AgentToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

impl ToolSession {
    pub(crate) fn new(
        config: &AiConfig,
        feature_model: &str,
        system: &str,
        user: &str,
        tools: Vec<Value>,
    ) -> Result<Self> {
        Self::with_history_options(
            config,
            &AiCallOptions::new(feature_model),
            system,
            Vec::new(),
            user,
            tools,
        )
    }

    /// Replay completed text turns before the current user message. Historical
    /// messages cannot supply system instructions or native tool-call metadata.
    pub(crate) fn with_history(
        config: &AiConfig,
        feature_model: &str,
        system: &str,
        history: Vec<Value>,
        user: &str,
        tools: Vec<Value>,
    ) -> Result<Self> {
        Self::with_history_options(
            config,
            &AiCallOptions::new(feature_model),
            system,
            history,
            user,
            tools,
        )
    }

    /// 带调用点选项的构造：模型与思考强度都在这里落到 target 上。
    pub(crate) fn with_history_options(
        config: &AiConfig,
        options: &AiCallOptions,
        system: &str,
        mut history: Vec<Value>,
        user: &str,
        tools: Vec<Value>,
    ) -> Result<Self> {
        if history.len() % 2 != 0 {
            bail!("AI 历史消息必须包含完整的 user/assistant 对话轮次");
        }
        for (index, message) in history.iter().enumerate() {
            let object = message
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("AI 历史消息必须是 JSON object"))?;
            if object.len() != 2 || !object.contains_key("role") || !object.contains_key("content")
            {
                bail!("AI 历史消息只允许 role 和 content 字段");
            }
            let expected_role = if index % 2 == 0 { "user" } else { "assistant" };
            if message["role"].as_str() != Some(expected_role) {
                bail!("AI 历史消息必须按 user/assistant 交替排列");
            }
            if !message["content"].is_string() {
                bail!("AI 历史消息 content 必须是字符串");
            }
        }
        history.push(serde_json::json!({"role":"user", "content":user}));
        let target = config.resolve_with(options)?;
        target.build_target()?;
        validate_http_proxy(&config.http_proxy)?;
        validate_extra_body(target.provider, &target.extra_body)?;
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            if tool["type"] != "function" || !tool["function"].is_object() {
                bail!("AI 工具定义必须是 Chat Completions function 格式");
            }
            let function = &tool["function"];
            let name = tool_string(function, "name")?;
            validate_tool_name(name)?;
            if !names.insert(name.to_string()) {
                bail!("AI 工具名重复: {name}");
            }
            if !function["parameters"].is_object() {
                bail!("AI 工具 parameters 必须是 JSON object");
            }
        }
        Ok(Self {
            target,
            http_proxy: config.http_proxy.clone(),
            system: system.to_string(),
            tools,
            names,
            history,
            seen: Default::default(),
            pending: Default::default(),
        })
    }

    fn request_body(&self, max_tokens: u32, allow_tools: bool) -> Result<Value> {
        if !self.pending.is_empty() {
            bail!("AI 工具调用尚未全部追加结果");
        }
        let enabled = allow_tools && !self.tools.is_empty();
        let tools: Vec<Value> = match self.target.wire_api {
            WireApi::ChatCompletions => self.tools.clone(),
            WireApi::Responses => self
                .tools
                .iter()
                .map(|tool| {
                    let mut function = tool["function"].clone();
                    function["type"] = Value::String("function".into());
                    function
                })
                .collect(),
        };
        let mut body = serde_json::json!({
            "model": self.target.model,
            "stream": false,
            "store": false,
            "tools": tools,
            "tool_choice": if enabled { "auto" } else { "none" },
        });
        match self.target.wire_api {
            WireApi::Responses => {
                body["instructions"] = self.system.clone().into();
                body["max_output_tokens"] = max_tokens.into();
                body["input"] = self.history.clone().into();
                // With store:false, retain the encrypted reasoning content when provided.
                body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
            }
            WireApi::ChatCompletions => {
                let mut messages =
                    vec![serde_json::json!({"role":"system", "content":self.system})];
                messages.extend(self.history.clone());
                body["messages"] = messages.into();
                body["max_completion_tokens"] = max_tokens.into();
            }
        }
        Ok(body)
    }

    fn responses_request(&self, max_tokens: u32, allow_tools: bool) -> Result<CreateResponse> {
        use async_openai::types::responses::{InputItem, InputParam, Item};
        let mut body = self.request_body(max_tokens, allow_tools)?;
        // Explicitly deserialize structured items, avoiding the SDK's untagged
        // item-reference alternative swallowing IDs and dropping item contents.
        body["input"] = serde_json::json!([]);
        let mut request: CreateResponse =
            serde_json::from_value(body).context("AI Responses 工具请求无效")?;
        let items = self
            .history
            .iter()
            .map(|item| {
                if item.get("type").is_some() {
                    serde_json::from_value::<Item>(item.clone()).map(InputItem::Item)
                } else {
                    serde_json::from_value(item.clone())
                }
            })
            .collect::<std::result::Result<Vec<InputItem>, _>>()
            .context("AI Responses 会话历史无效")?;
        request.input = InputParam::Items(items);
        Ok(request)
    }

    pub(crate) fn request(
        &mut self,
        runtime: &miliastra_kernel::ai::OpenAiRuntimeHandle,
        max_tokens: u32,
        timeout: std::time::Duration,
        allow_tools: bool,
    ) -> Result<AgentReply> {
        let runtime = runtime.with_http_proxy(&self.http_proxy)?;
        match self.target.wire_api {
            WireApi::Responses => {
                let request = self.responses_request(max_tokens, allow_tools)?;
                let response = runtime
                    .create_response(
                        self.target.build_target()?,
                        request,
                        &self.target.extra_body,
                        timeout,
                    )?
                    .wait()
                    .context("AI 工具请求失败")?;
                if serde_json::to_value(&response)?["status"] != "completed" {
                    bail!("AI Responses 响应未完成");
                }
                let output = response
                    .output
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                self.accept_output(output, allow_tools)
            }
            WireApi::ChatCompletions => {
                let request: CreateChatCompletionRequest =
                    serde_json::from_value(self.request_body(max_tokens, allow_tools)?)
                        .context("AI Chat Completions 工具请求无效")?;
                let response = runtime
                    .chat_completion(
                        self.target.build_target()?,
                        request,
                        &self.target.extra_body,
                        timeout,
                    )?
                    .wait()
                    .context("AI 工具请求失败")?;
                let choice = response
                    .choices
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("AI 响应缺少 choices[0]"))?;
                if choice.finish_reason == Some(async_openai::types::chat::FinishReason::Length) {
                    bail!("AI 工具响应超出输出上限");
                }
                self.accept_output(vec![serde_json::to_value(&choice.message)?], allow_tools)
            }
        }
    }

    fn accept_output(&mut self, output: Vec<Value>, allow_tools: bool) -> Result<AgentReply> {
        let mut reply = AgentReply {
            text: String::new(),
            calls: Vec::new(),
        };
        for item in &output {
            match self.target.wire_api {
                WireApi::Responses => match item["type"].as_str() {
                    Some("function_call") => {
                        if item["status"].as_str().is_some_and(|s| s != "completed") {
                            bail!("AI 工具调用未完成");
                        }
                        reply.calls.push(AgentToolCall {
                            id: tool_string(item, "call_id")?.to_string(),
                            name: tool_string(item, "name")?.to_string(),
                            arguments: tool_string(item, "arguments")?.to_string(),
                        });
                    }
                    Some("message") => {
                        if let Some(content) = item["content"].as_array() {
                            for part in content {
                                if part["type"] == "refusal" {
                                    bail!("AI 拒绝回答: {}", part["refusal"]);
                                }
                                if part["type"] == "output_text" {
                                    reply.text.push_str(tool_string(part, "text")?);
                                }
                            }
                        }
                    }
                    _ => {}
                },
                WireApi::ChatCompletions => {
                    if let Some(refusal) = item["refusal"].as_str().filter(|s| !s.is_empty()) {
                        bail!("AI 拒绝回答: {refusal}");
                    }
                    if let Some(text) = item["content"].as_str() {
                        reply.text.push_str(text);
                    }
                    if let Some(calls) = item["tool_calls"].as_array() {
                        for call in calls {
                            if call["type"] != "function" {
                                bail!("AI 返回了不支持的工具类型");
                            }
                            reply.calls.push(AgentToolCall {
                                id: tool_string(call, "id")?.to_string(),
                                name: tool_string(&call["function"], "name")?.to_string(),
                                arguments: tool_string(&call["function"], "arguments")?.to_string(),
                            });
                        }
                    }
                }
            }
        }
        if reply.calls.len() > 8 {
            bail!("AI 单轮工具调用不能超过 8 个");
        }
        let mut ids = std::collections::HashSet::new();
        for call in &reply.calls {
            if !allow_tools {
                bail!("AI 在禁用工具时返回工具调用");
            }
            validate_tool_name(&call.name)?;
            if !self.names.contains(&call.name) {
                bail!("AI 返回未声明的工具: {}", call.name);
            }
            if call.id.is_empty()
                || call.id.len() > 512
                || call.id.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                bail!("AI 工具调用 ID 无效");
            }
            if self.seen.contains(&call.id) || !ids.insert(call.id.clone()) {
                bail!("AI 工具调用 ID 重复");
            }
            let arguments: Value =
                serde_json::from_str(&call.arguments).context("AI 工具参数不是有效 JSON")?;
            if !arguments.is_object() {
                bail!("AI 工具参数必须是 JSON object");
            }
        }
        if reply.text.trim().is_empty() && reply.calls.is_empty() {
            bail!("AI 响应缺少文本与工具调用");
        }
        self.seen.extend(ids.iter().cloned());
        self.pending.extend(ids);
        self.history.extend(output);
        Ok(reply)
    }

    pub(crate) fn append_result(&mut self, id: &str, result: &str) -> Result<()> {
        if !self.pending.remove(id) {
            bail!("AI 工具结果没有对应的待处理调用 ID");
        }
        self.history.push(match self.target.wire_api {
            WireApi::Responses => serde_json::json!({
                "type":"function_call_output", "call_id":id, "output":result,
            }),
            WireApi::ChatCompletions => serde_json::json!({
                "role":"tool", "tool_call_id":id, "content":result,
            }),
        });
        Ok(())
    }
}

fn tool_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("AI 工具响应缺少字符串 {field}"))
}

fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("AI 工具名无效");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_definitions() -> Vec<Value> {
        vec![json!({"type":"function", "function":{
            "name":"lookup", "description":"Find a song", "strict":true,
            "parameters":{"type":"object", "properties":{}, "additionalProperties":false}
        }})]
    }

    fn session_body(session: &ToolSession, allow_tools: bool) -> Value {
        match session.target.wire_api {
            WireApi::Responses => {
                serde_json::to_value(session.responses_request(123, allow_tools).unwrap()).unwrap()
            }
            WireApi::ChatCompletions => {
                let request: CreateChatCompletionRequest =
                    serde_json::from_value(session.request_body(123, allow_tools).unwrap())
                        .unwrap();
                serde_json::to_value(request).unwrap()
            }
        }
    }

    #[test]
    fn tool_session_replays_text_history_before_current_user_on_both_wires() {
        let history = vec![
            json!({"role":"user", "content":"First question"}),
            json!({"role":"assistant", "content":"First answer"}),
            json!({"role":"user", "content":"Second question"}),
            json!({"role":"assistant", "content":"Second answer"}),
        ];
        for wire in [WireApi::Responses, WireApi::ChatCompletions] {
            let mut config = config("openai");
            config.wire_api = wire;
            let session = ToolSession::with_history(
                &config,
                "",
                "Persona",
                history.clone(),
                "Current question",
                tool_definitions(),
            )
            .unwrap();
            let body = session_body(&session, true);
            let messages = match wire {
                WireApi::Responses => {
                    assert_eq!(body["instructions"], "Persona");
                    body["input"].as_array().unwrap().clone()
                }
                WireApi::ChatCompletions => {
                    assert_eq!(
                        body["messages"][0],
                        json!({"role":"system","content":"Persona"})
                    );
                    body["messages"].as_array().unwrap()[1..].to_vec()
                }
            };
            assert_eq!(messages.len(), history.len() + 1);
            for (actual, expected) in messages.iter().zip(&history) {
                assert_eq!(actual["role"], expected["role"]);
                assert_eq!(actual["content"], expected["content"]);
            }
            let current = messages.last().unwrap();
            assert_eq!(current["role"], "user");
            assert_eq!(current["content"], "Current question");
            let empty = ToolSession::with_history(
                &config,
                "",
                "Persona",
                vec![],
                "Current question",
                tool_definitions(),
            )
            .unwrap();
            let original = ToolSession::new(
                &config,
                "",
                "Persona",
                "Current question",
                tool_definitions(),
            )
            .unwrap();
            assert_eq!(session_body(&empty, true), session_body(&original, true));
        }
    }

    #[test]
    fn tool_session_rejects_privileged_malformed_and_incomplete_text_history() {
        let user = json!({"role":"user","content":"question"});
        let assistant = json!({"role":"assistant","content":"answer"});
        let mut invalid = vec![
            vec![user.clone()],
            vec![assistant.clone(), user.clone()],
            vec![user.clone(), user.clone()],
            vec![
                json!({"role":"user","content":[{"type":"text","text":"question"}]}),
                assistant.clone(),
            ],
            vec![json!({"role":"user","content":null}), assistant.clone()],
            vec![
                user.clone(),
                json!({"role":"assistant","content":"answer","tool_calls":[]}),
            ],
            vec![
                user.clone(),
                json!({"role":"assistant","content":"answer","type":"message"}),
            ],
            vec![Value::Null, assistant.clone()],
        ];
        for role in ["system", "developer", "tool", "function", "unknown"] {
            invalid.push(vec![
                json!({"role":role,"content":"injected"}),
                assistant.clone(),
            ]);
            invalid.push(vec![
                user.clone(),
                json!({"role":role,"content":"injected"}),
            ]);
        }
        for wire in [WireApi::Responses, WireApi::ChatCompletions] {
            let mut config = config("openai");
            config.wire_api = wire;
            for history in &invalid {
                assert!(
                    ToolSession::with_history(
                        &config,
                        "",
                        "Persona",
                        history.clone(),
                        "Current",
                        vec![]
                    )
                    .is_err(),
                    "accepted {history:?}"
                );
            }
        }
    }

    #[test]
    fn tool_session_serializes_both_configured_wires_and_preserves_target() {
        for wire in [WireApi::Responses, WireApi::ChatCompletions] {
            let mut config = config("mimo");
            config.wire_api = wire;
            config.http_proxy = "http://127.0.0.1:7890".into();
            config
                .extra_body
                .insert("thinking".into(), json!({"type":"disabled"}));
            let session = ToolSession::new(
                &config,
                "feature-model",
                "system",
                "user",
                tool_definitions(),
            )
            .unwrap();
            assert_eq!(session.target.wire_api, wire);
            assert_eq!(session.target.authentication(), Authentication::ApiKey);
            assert_eq!(session.target.api_key, "secret");
            assert_eq!(session.http_proxy, config.http_proxy);
            assert_eq!(session.target.extra_body, config.extra_body);
            let body = session_body(&session, true);
            assert_eq!(body["model"], "feature-model");
            assert_eq!(body["store"], false);
            assert_eq!(body["tool_choice"], "auto");
            assert!(body.get("response_format").is_none());
            assert!(body.get("previous_response_id").is_none());
            match wire {
                WireApi::Responses => {
                    assert!(session.target.endpoint.ends_with("/responses"));
                    assert_eq!(body["tools"][0]["name"], "lookup");
                    assert_eq!(body["tools"][0]["strict"], true);
                    assert_eq!(body["instructions"], "system");
                    assert_eq!(body["input"][0]["content"], "user");
                    assert!(body.get("text").is_none());
                }
                WireApi::ChatCompletions => {
                    assert!(session.target.endpoint.ends_with("/chat/completions"));
                    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
                    assert_eq!(body["messages"][0]["content"], "system");
                }
            }
            assert_eq!(session_body(&session, false)["tool_choice"], "none");
            let empty = ToolSession::new(&config, "", "system", "user", vec![]).unwrap();
            assert_eq!(session_body(&empty, true)["tool_choice"], "none");
        }
    }

    #[test]
    fn tool_session_replays_full_outputs_and_results_across_repeated_rounds() {
        for wire in [WireApi::Responses, WireApi::ChatCompletions] {
            let mut config = config("openai");
            config.wire_api = wire;
            let mut session =
                ToolSession::new(&config, "", "system", "user", tool_definitions()).unwrap();
            for round in 0..2 {
                let id = format!("call_{round}");
                let outputs = match wire {
                    WireApi::Responses => vec![
                        json!({"type":"reasoning", "id":format!("rs_{round}"), "summary":[], "encrypted_content":"encrypted"}),
                        json!({"type":"function_call", "id":format!("fc_{round}"), "call_id":id,
                            "name":"lookup", "arguments":"{}", "status":"completed"}),
                    ],
                    WireApi::ChatCompletions => vec![json!({"role":"assistant", "content":null,
                        "tool_calls":[{"id":id,"type":"function","function":{"name":"lookup","arguments":"{}"}}]})],
                };
                let reply = session.accept_output(outputs.clone(), true).unwrap();
                assert_eq!(reply.calls[0].id, id);
                assert_eq!(reply.calls[0].name, "lookup");
                assert_eq!(reply.calls[0].arguments, "{}");
                assert!(session.request_body(123, true).is_err());
                assert!(session.append_result("unknown", "result").is_err());
                session.append_result(&id, "result").unwrap();
                assert!(session.append_result(&id, "duplicate").is_err());
                let body = session_body(&session, false);
                match wire {
                    WireApi::Responses => {
                        let index = 1 + round * 3;
                        assert_eq!(body["input"][index], outputs[0]);
                        assert_eq!(body["input"][index + 1], outputs[1]);
                        assert_eq!(body["input"][index + 2]["call_id"], id);
                        assert_eq!(body["input"][index + 2]["type"], "function_call_output");
                    }
                    WireApi::ChatCompletions => {
                        let index = 2 + round * 2;
                        assert_eq!(body["messages"][index]["tool_calls"][0]["id"], id);
                        assert_eq!(body["messages"][index + 1]["tool_call_id"], id);
                        assert_eq!(body["messages"][index + 1]["content"], "result");
                    }
                }
            }
            let final_output = match wire {
                WireApi::Responses => {
                    json!({"type":"message","id":"msg_final","role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":"Natural language answer", "annotations":[]}]})
                }
                WireApi::ChatCompletions => {
                    json!({"role":"assistant", "content":"Natural language answer"})
                }
            };
            assert_eq!(
                session
                    .accept_output(vec![final_output], false)
                    .unwrap()
                    .text,
                "Natural language answer"
            );
            session_body(&session, false);
        }
    }

    #[test]
    fn tool_session_rejects_malformed_or_unexpected_calls_without_mutating_history() {
        let mut session =
            ToolSession::new(&config("openai"), "", "s", "u", tool_definitions()).unwrap();
        for (id, name, arguments, allow) in [
            ("", "lookup", "{}", true),
            ("bad id", "lookup", "{}", true),
            ("call", "unknown", "{}", true),
            ("call", "lookup", "not json", true),
            ("call", "lookup", "[]", true),
            ("call", "lookup", "{}", false),
        ] {
            assert!(
                session
                    .accept_output(
                        vec![json!({"type":"function_call", "call_id":id,
                "name":name,"arguments":arguments})],
                        allow
                    )
                    .is_err()
            );
            assert_eq!(session.history.len(), 1);
            assert!(session.pending.is_empty());
        }
        let too_many = (0..9)
            .map(|i| {
                json!({"type":"function_call",
            "call_id":format!("call_{i}"),"name":"lookup","arguments":"{}"})
            })
            .collect();
        assert!(session.accept_output(too_many, true).is_err());
        assert_eq!(session.history.len(), 1);
        assert!(session.pending.is_empty());
        let call =
            json!({"type":"function_call","call_id":"same","name":"lookup","arguments":"{}"});
        assert!(
            session
                .accept_output(vec![call.clone(), call.clone()], true)
                .is_err()
        );
        session.accept_output(vec![call.clone()], true).unwrap();
        session.append_result("same", "done").unwrap();
        assert!(session.accept_output(vec![call], true).is_err());
    }

    fn config(provider: &str) -> AiConfig {
        AiConfig {
            provider: provider.to_string(),
            api_key: "secret".to_string(),
            ..AiConfig::default()
        }
    }

    fn rule(from: &str, to: &str, match_type: &str, model: &str) -> ReasoningRule {
        ReasoningRule {
            from: from.into(),
            to: to.into(),
            match_type: match_type.into(),
            model: model.into(),
        }
    }

    #[test]
    fn reasoning_rules_rewrite_by_model_prefix_and_suffix() {
        let rules = vec![
            rule("minimal", "low", "prefix", "o1"),
            rule("high", "medium", "suffix", "-mini"),
        ];
        // 前缀命中：o1 系列不接受 minimal。
        assert_eq!(
            apply_reasoning_rules("o1-preview", ReasoningEffort::Minimal, &rules).unwrap(),
            Some("low")
        );
        // 后缀命中。
        assert_eq!(
            apply_reasoning_rules("gpt-5-mini", ReasoningEffort::High, &rules).unwrap(),
            Some("medium")
        );
        // 没有规则命中时原样发送。
        assert_eq!(
            apply_reasoning_rules("gpt-5", ReasoningEffort::Minimal, &rules).unwrap(),
            Some("minimal")
        );
    }

    #[test]
    fn reasoning_rules_can_deny_and_are_scoped_to_their_effort() {
        let rules = vec![rule("minimal", "deny", "exact", "strict-model")];
        assert!(
            apply_reasoning_rules("strict-model", ReasoningEffort::Minimal, &rules).is_err(),
            "deny 规则必须拒绝该组合"
        );
        // 同一模型的其它强度不受影响。
        assert_eq!(
            apply_reasoning_rules("strict-model", ReasoningEffort::High, &rules).unwrap(),
            Some("high")
        );
    }

    #[test]
    fn empty_model_selector_matches_every_model() {
        let rules = vec![rule("minimal", "low", "", "")];
        assert_eq!(
            apply_reasoning_rules("任意模型", ReasoningEffort::Minimal, &rules).unwrap(),
            Some("low")
        );
    }

    #[test]
    fn unset_reasoning_never_consults_rules() {
        let rules = vec![rule("minimal", "deny", "", "")];
        assert_eq!(
            apply_reasoning_rules("any", ReasoningEffort::Unset, &rules).unwrap(),
            None
        );
    }

    #[test]
    fn reasoning_rejections_are_detected_only_when_the_field_is_named() {
        // 只对明确提到 reasoning/thinking 的错误降级重试。
        assert!(looks_like_reasoning_rejection(&anyhow::anyhow!(
            "400 Bad Request: Unsupported parameter: reasoning.effort"
        )));
        assert!(looks_like_reasoning_rejection(&anyhow::anyhow!(
            "invalid parameter: reasoning_effort"
        )));
        // 与思考强度无关的失败不能重试，否则会掩盖真实问题。
        assert!(!looks_like_reasoning_rejection(&anyhow::anyhow!(
            "400 model_not_found: gpt-9"
        )));
        assert!(!looks_like_reasoning_rejection(&anyhow::anyhow!(
            "401 invalid api key"
        )));
    }

    #[test]
    fn reasoning_rules_are_validated() {
        let mut connection = config("openai");
        connection.reasoning_rules = vec![rule("minimal", "  ", "", "")];
        assert!(connection.validate().is_err(), "空 to 必须被拒绝");
        connection.reasoning_rules = vec![rule("bogus", "low", "", "")];
        assert!(connection.validate().is_err(), "非法 from 必须被拒绝");
        connection.reasoning_rules = vec![rule("minimal", "low", "middle", "")];
        assert!(connection.validate().is_err(), "非法 match_type 必须被拒绝");
        connection.reasoning_rules = vec![rule("minimal", "deny", "prefix", "o1")];
        connection.validate().expect("合法规则应通过");
    }

    #[test]
    fn reasoning_effort_is_injected_per_wire_api() {
        // Responses 用嵌套的 reasoning.effort，Chat Completions 用扁平字段。
        // 两种协议的字段名差异由 merged_extra_body 收敛，调用点不必关心。
        let responses = config("openai");
        let target = responses
            .resolve_with(&AiCallOptions::new("").with_reasoning(ReasoningEffort::High))
            .expect("responses target");
        let body = merged_extra_body(&target);
        assert_eq!(body["reasoning"]["effort"], json!("high"));
        assert!(!body.contains_key("reasoning_effort"));

        let mut chat = config("openai");
        chat.wire_api = WireApi::ChatCompletions;
        let target = chat
            .resolve_with(&AiCallOptions::new("").with_reasoning(ReasoningEffort::Low))
            .expect("chat target");
        let body = merged_extra_body(&target);
        assert_eq!(body["reasoning_effort"], json!("low"));
        assert!(!body.contains_key("reasoning"));
    }

    #[test]
    fn unset_reasoning_does_not_emit_any_field() {
        let target = config("openai")
            .resolve_with(&AiCallOptions::new(""))
            .expect("target");
        let body = merged_extra_body(&target);
        assert!(!body.contains_key("reasoning"));
        assert!(!body.contains_key("reasoning_effort"));
    }

    #[test]
    fn explicit_extra_body_reasoning_wins_over_the_structured_option() {
        // 用户手写 extra_body 时以他写的为准，避免结构化选项静默覆盖。
        let mut connection = config("openai");
        connection.extra_body =
            HashMap::from([("reasoning".to_string(), json!({"effort": "minimal"}))]);
        let target = connection
            .resolve_with(&AiCallOptions::new("").with_reasoning(ReasoningEffort::High))
            .expect("target");
        assert_eq!(
            merged_extra_body(&target)["reasoning"]["effort"],
            json!("minimal")
        );
    }

    #[test]
    fn model_list_parser_accepts_the_common_shapes() {
        let openai = r#"{"data":[{"id":"gpt-b"},{"id":"gpt-a"}]}"#;
        assert_eq!(parse_model_list(openai).unwrap(), ["gpt-a", "gpt-b"]);
        let alt = r#"{"models":["m2","m1"]}"#;
        assert_eq!(parse_model_list(alt).unwrap(), ["m1", "m2"]);
        let bare = r#"["z","a"]"#;
        assert_eq!(parse_model_list(bare).unwrap(), ["a", "z"]);
        assert!(parse_model_list(r#"{"data":[]}"#).is_err());
        assert!(parse_model_list("not json").is_err());
    }

    #[test]
    fn models_base_follows_the_wire_api_endpoint() {
        // Responses 默认：从推导出的 /responses 地址去掉后缀。
        assert_eq!(
            models_base_url(AiProvider::OpenAi, &config("openai")).unwrap(),
            "https://api.openai.com/v1"
        );

        // 只填 responses_endpoint 也必须能拉列表：这是合法配置，
        // 此时 endpoint 为空，按 chat 地址推导会直接报"缺少 endpoint"。
        let mut responses_only = config("custom");
        responses_only.endpoint = String::new();
        responses_only.responses_endpoint = "https://gateway.example/v1/responses".to_string();
        assert_eq!(
            models_base_url(AiProvider::Custom, &responses_only).unwrap(),
            "https://gateway.example/v1"
        );

        // Chat Completions 配置仍按 chat 地址推导。
        let mut chat = config("custom");
        chat.endpoint = "https://gateway.example/v1/chat/completions".to_string();
        chat.wire_api = WireApi::ChatCompletions;
        assert_eq!(
            models_base_url(AiProvider::Custom, &chat).unwrap(),
            "https://gateway.example/v1"
        );
    }

    #[test]
    fn models_base_rejects_endpoints_without_a_known_suffix() {
        let mut connection = config("custom");
        connection.endpoint = "https://gateway.example/v1/other".to_string();
        connection.responses_endpoint = "https://gateway.example/v1/also-other".to_string();

        let error = models_base_url(AiProvider::Custom, &connection)
            .expect_err("unknown suffix must be rejected");

        assert!(
            error.to_string().contains("/models") || error.to_string().contains("结尾"),
            "{error}"
        );
    }

    #[test]
    fn models_base_trims_a_trailing_slash() {
        let mut connection = config("custom");
        connection.endpoint = "https://gateway.example/v1/chat/completions/".to_string();
        connection.wire_api = WireApi::ChatCompletions;

        // 结尾多一个 / 不该拼出 //models，也不该把后缀匹配搞失败。
        assert_eq!(
            models_base_url(AiProvider::Custom, &connection).unwrap(),
            "https://gateway.example/v1"
        );
    }

    #[test]
    fn responses_endpoint_is_derived_from_the_chat_endpoint() {
        let target = config("openai").resolve("").expect("responses target");

        assert_eq!(target.endpoint, "https://api.openai.com/v1/responses");
        assert_eq!(target.wire_api, WireApi::Responses);
    }

    #[test]
    fn explicit_responses_endpoint_wins_over_derivation() {
        let mut connection = config("custom");
        connection.endpoint = "https://gateway.example/v1/chat/completions".to_string();
        connection.responses_endpoint = "https://gateway.example/v1/responses".to_string();

        let target = connection.resolve("").expect("responses target");

        assert_eq!(target.endpoint, "https://gateway.example/v1/responses");
    }

    #[test]
    fn responses_endpoint_must_be_derivable_or_explicit() {
        let mut connection = config("custom");
        connection.endpoint = "https://gateway.example/v1/other".to_string();

        let error = connection.resolve("").expect_err("no responses suffix");

        assert!(error.to_string().contains("responses_endpoint"), "{error}");
    }

    #[test]
    fn chat_wire_api_keeps_the_configured_chat_endpoint() {
        let mut connection = config("custom");
        connection.endpoint = "https://gateway.example/v1/chat/completions".to_string();
        connection.wire_api = WireApi::ChatCompletions;

        let target = connection.resolve("").expect("chat target");

        assert_eq!(
            target.endpoint,
            "https://gateway.example/v1/chat/completions"
        );
        assert_eq!(target.wire_api, WireApi::ChatCompletions);
    }

    #[test]
    fn feature_model_overrides_the_shared_model() {
        let mut connection = config("openai");
        connection.model = "shared-model".to_string();

        assert_eq!(
            connection.resolve("").expect("shared model").model,
            "shared-model"
        );
        assert_eq!(
            connection
                .resolve("review-model")
                .expect("feature model")
                .model,
            "review-model"
        );
        // 空白覆盖值等于未覆盖。
        assert_eq!(
            connection.resolve("   ").expect("blank override").model,
            "shared-model"
        );
    }

    #[test]
    fn official_providers_resolve_their_default_endpoints_and_models() {
        for (provider, endpoint, model) in [
            ("mimo", MIMO_ENDPOINT, MIMO_MODEL),
            ("openai", OPENAI_ENDPOINT, OPENAI_MODEL),
            ("deepseek", DEEPSEEK_ENDPOINT, DEEPSEEK_MODEL),
        ] {
            let mut connection = config(provider);
            connection.endpoint = String::new();
            connection.model = String::new();

            let target = connection.resolve("").expect("official provider");

            assert_eq!(
                target.endpoint.replace("/chat/completions", "/responses"),
                endpoint.replace("/chat/completions", "/responses"),
                "provider={provider}"
            );
            assert_eq!(target.model, model, "provider={provider}");
        }
    }

    #[test]
    fn unknown_provider_aliases_are_rejected() {
        for provider in ["", "openai-compatible", "openai_compatible", "azure"] {
            let mut connection = config("openai");
            connection.provider = provider.to_string();

            let error = connection.validate().expect_err("unsupported provider");
            assert!(
                error.to_string().contains("ai.provider"),
                "provider={provider} error={error}"
            );
        }
    }

    #[test]
    fn custom_provider_requires_endpoint_and_model() {
        let mut connection = config("custom");
        connection.endpoint = String::new();

        assert!(connection.resolve("").is_err(), "custom without endpoint");

        connection.endpoint = "https://gateway.example/v1/chat/completions".to_string();
        connection.model = String::new();
        assert!(connection.resolve("").is_err(), "custom without model");
    }

    #[test]
    fn mimo_uses_api_key_header_while_other_providers_use_bearer() {
        assert_eq!(authentication(AiProvider::Mimo), Authentication::ApiKey);
        assert_eq!(authentication(AiProvider::OpenAi), Authentication::Bearer);
        assert_eq!(authentication(AiProvider::DeepSeek), Authentication::Bearer);
        assert_eq!(authentication(AiProvider::Custom), Authentication::Bearer);
    }

    #[test]
    fn disabled_connection_only_validates_shape_and_proxy() {
        let mut connection = config("openai");
        connection.api_key = String::new();
        // 未配置密钥时不解析模型/地址，但形状与代理仍然校验。
        assert!(connection.validate().is_ok());

        connection.http_proxy = "socks5://127.0.0.1:1080".to_string();
        assert!(connection.validate().is_err());
    }

    #[test]
    fn enabled_connection_rejects_an_unsupported_http_proxy() {
        let mut connection = config("openai");
        connection.http_proxy = "socks5://127.0.0.1:1080".to_string();

        assert!(connection.validate().is_err());
    }

    #[test]
    fn endpoints_must_be_http_urls() {
        let mut connection = config("custom");
        connection.endpoint = "gateway.example/v1/chat/completions".to_string();

        assert!(connection.resolve("").is_err());

        connection.endpoint = "ftp://gateway.example/v1/chat/completions".to_string();
        assert!(connection.resolve("").is_err());
    }

    #[test]
    fn provider_specific_extra_body_fields_are_validated() {
        let mut connection = config("deepseek");
        connection.extra_body = HashMap::from([("user_id".to_string(), json!("含中文"))]);
        assert!(connection.validate().is_err());

        connection.provider = "openai".to_string();
        connection.extra_body = HashMap::from([("metadata".to_string(), json!(["not-object"]))]);
        assert!(connection.validate().is_err());

        // custom 网关不校验供应商专属字段。
        connection.provider = "custom".to_string();
        connection.endpoint = "https://gateway.example/v1/chat/completions".to_string();
        connection.model = "m".to_string();
        connection.extra_body = HashMap::from([("anything".to_string(), json!(["ok"]))]);
        assert!(connection.validate().is_ok());
    }

    #[test]
    fn extra_body_is_passed_through_unchanged() {
        let mut connection = config("openai");
        connection.extra_body = HashMap::from([("reasoning_effort".to_string(), json!("high"))]);

        let target = connection.resolve("").expect("resolved target");

        assert_eq!(target.extra_body, connection.extra_body);
    }

    #[test]
    fn api_keys_and_models_reject_control_characters() {
        let mut connection = config("openai");
        connection.api_key = "bad\nkey".to_string();
        assert!(connection.resolve("").is_err());

        let mut connection = config("openai");
        connection.model = "bad\tmodel".to_string();
        assert!(connection.resolve("").is_err());
    }

    #[test]
    fn response_output_text_returns_message_and_rejects_refusal_or_empty() {
        let completed: Response = serde_json::from_value(json!({
            "id": "resp-1",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "test",
            "output": [{
                "type": "message",
                "id": "msg-1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "  {\"ok\":true}  ", "annotations": []}]
            }],
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": []
        }))
        .expect("completed response");
        assert_eq!(
            responses_output_text(&completed).expect("output text"),
            "{\"ok\":true}"
        );

        let empty: Response = serde_json::from_value(json!({
            "id": "resp-2",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "test",
            "output": [],
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": []
        }))
        .expect("empty response");
        assert!(responses_output_text(&empty).is_err());
    }

    #[test]
    fn responses_request_uses_json_object_text_format_and_no_tools() {
        let mut connection = config("openai");
        connection.wire_api = WireApi::Responses;
        let target = connection.resolve("").expect("target");
        let request = build_responses_request(&target, "系统", "用户", 512, true).expect("request");
        let body = serde_json::to_value(request).expect("request json");

        assert_eq!(body["text"]["format"]["type"], "json_object");
        assert_eq!(body["max_output_tokens"], 512);
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
        assert!(body.get("tools").is_none());
    }

    /// 回归：摘要这类自然语言输出**不能**强制 JSON 模式。
    ///
    /// 线上表现为每次记忆压缩都 400 invalid_request_error，
    /// 原文因此永远压不掉、每轮重试一次注定失败的调用。
    #[test]
    fn plain_text_mode_does_not_force_json_object() {
        let mut connection = config("openai");
        connection.wire_api = WireApi::ChatCompletions;
        let target = connection.resolve("").expect("target");
        let request = build_chat_request(&target, "系统", "用户", 800, false).expect("request");
        let body = serde_json::to_value(request).expect("request json");
        let format = &body["response_format"]["type"];
        assert_ne!(
            format, "json_object",
            "纯文本模式不能要求 JSON 对象，实际: {format}"
        );
    }

    #[test]
    fn chat_request_uses_typed_messages_and_json_object_format() {
        let mut connection = config("openai");
        connection.wire_api = WireApi::ChatCompletions;
        let target = connection.resolve("").expect("target");
        let request = build_chat_request(&target, "系统", "用户", 1024, true).expect("request");
        let body = serde_json::to_value(request).expect("request json");

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "系统");
        assert_eq!(body["messages"][1]["content"], "用户");
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["max_completion_tokens"], 1024);
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
    }
}
