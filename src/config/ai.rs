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
        validate_extra_body(provider, &self.extra_body)
    }

    pub(crate) fn resolve(&self, feature_model: &str) -> Result<ResolvedAiTarget> {
        let provider = parse_provider(&self.provider)?;
        let wire_api = self.wire_api;
        let endpoint = match wire_api {
            WireApi::Responses => {
                resolve_responses_endpoint(provider, &self.endpoint, &self.responses_endpoint)?
            }
            WireApi::ChatCompletions => resolve_endpoint(provider, &self.endpoint)?,
        };
        Ok(ResolvedAiTarget {
            provider,
            endpoint,
            api_key: normalize_api_key(&self.api_key)?,
            model: resolve_model(provider, effective_model(self, feature_model))?,
            wire_api,
            extra_body: self.extra_body.clone(),
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
    feature_model: &str,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    let target = config.resolve(feature_model)?;
    match target.wire_api {
        WireApi::Responses => {
            let request = build_responses_request(&target, system_prompt, user_prompt, max_tokens)?;
            let response = openai
                .create_response(target.build_target()?, request, &target.extra_body, timeout)?
                .wait()
                .context("AI 请求失败")?;
            responses_output_text(&response)
        }
        WireApi::ChatCompletions => {
            let request = build_chat_request(&target, system_prompt, user_prompt, max_tokens)?;
            let response = openai
                .chat_completion(target.build_target()?, request, &target.extra_body, timeout)?
                .wait()
                .context("AI 请求失败")?;
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

fn build_responses_request(
    target: &ResolvedAiTarget,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
) -> anyhow::Result<CreateResponse> {
    Ok(CreateResponseArgs::default()
        .model(target.model.clone())
        .instructions(system_prompt)
        .input(user_prompt)
        .text(ResponseTextParam {
            format: TextResponseFormatConfiguration::JsonObject,
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
        .response_format(ResponseFormat::JsonObject)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(provider: &str) -> AiConfig {
        AiConfig {
            provider: provider.to_string(),
            api_key: "secret".to_string(),
            ..AiConfig::default()
        }
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
        let request = build_responses_request(&target, "系统", "用户", 512).expect("request");
        let body = serde_json::to_value(request).expect("request json");

        assert_eq!(body["text"]["format"]["type"], "json_object");
        assert_eq!(body["max_output_tokens"], 512);
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn chat_request_uses_typed_messages_and_json_object_format() {
        let mut connection = config("openai");
        connection.wire_api = WireApi::ChatCompletions;
        let target = connection.resolve("").expect("target");
        let request = build_chat_request(&target, "系统", "用户", 1024).expect("request");
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
