use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_openai::types::responses::{
    CreateResponse, CreateResponseArgs, ResponseFormatJsonSchema, Tool, ToolChoiceOptions,
    ToolChoiceParam, WebSearchTool,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::runtime::clock::Delay;
use crate::runtime::openai::{OpenAiRuntimeHandle, Target, validate_http_proxy};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SongReviewConfig {
    pub enabled: bool,
    pub max_allowed_level: u8,
    pub failure_policy: SongReviewFailurePolicy,
    pub retry_count: u32,
    pub retry_delay_ms: u64,
    pub reply_reason_max_chars: usize,
    pub policy_prompt: String,
    pub custom_prompt: String,
    pub provider: SongReviewProviderConfig,
}

impl Default for SongReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_allowed_level: 4,
            failure_policy: SongReviewFailurePolicy::Reject,
            retry_count: 2,
            retry_delay_ms: 500,
            reply_reason_max_chars: 40,
            policy_prompt: default_song_review_policy_prompt(),
            custom_prompt: String::new(),
            provider: SongReviewProviderConfig::default(),
        }
    }
}

impl SongReviewConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_http_proxy(&self.provider.http_proxy)
            .context("song_review.provider.http_proxy 配置无效")?;
        if !self.enabled {
            return Ok(());
        }
        if !(1..=10).contains(&self.max_allowed_level) {
            bail!("song_review.max_allowed_level 必须在 1 到 10 之间");
        }
        if self.reply_reason_max_chars == 0 {
            bail!("song_review.reply_reason_max_chars 必须大于 0");
        }
        if self.provider.endpoint.trim().is_empty() {
            bail!("song_review.provider.endpoint 未配置");
        }
        if self.provider.api_key.trim().is_empty() {
            bail!("song_review.provider.api_key 未配置");
        }
        if self.provider.model.trim().is_empty() {
            bail!("song_review.provider.model 未配置");
        }
        Target::responses(&self.provider.endpoint, &self.provider.api_key)
            .context("song_review.provider 配置无效")?;
        Ok(())
    }
}

fn default_song_review_policy_prompt() -> String {
    [
        "审核目标：只通过整体听感偏舒缓、柔和、轻松、安静、治愈、抒情、慢节奏或中低强度的歌曲。",
        "拒绝明显炸场、吵闹、压迫感强、节奏过快、情绪过激、强烈电子噪音、重金属、硬核、鬼畜、洗脑循环、尖锐喊叫、强烈攻击性或明显破坏房间氛围的歌曲。",
        "请尽量使用联网搜索得到的曲风、歌词摘要、歌曲介绍和公开听感描述判断。",
        "如果信息不足，请保守判断；不确定时应给较高强度等级，而不是因为歌曲热门、用户喜欢或歌手知名就放宽标准。",
    ]
    .join("\n")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SongReviewFailurePolicy {
    Reject,
    Allow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SongReviewProviderConfig {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub http_proxy: String,
    pub extra_body: HashMap<String, Value>,
}

#[derive(Clone)]
pub(crate) struct SongReviewClient {
    config: SongReviewConfig,
    request_timeout: Duration,
    openai: OpenAiRuntimeHandle,
    retry_delay: Arc<dyn Delay>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SongReviewCandidate {
    pub source: String,
    pub title: String,
    pub artist: String,
    pub uri: String,
    pub message_type: String,
    pub username: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SongReviewDecision {
    pub allowed: bool,
    pub level: Option<u8>,
    pub threshold: u8,
    pub reason: String,
    pub tags: Vec<String>,
    pub attempts: u32,
    pub failed_open: bool,
}

#[derive(Clone, Debug)]
struct SongReviewResult {
    level: u8,
    reason: String,
    tags: Vec<String>,
}

impl SongReviewClient {
    pub(crate) fn new(
        config: &SongReviewConfig,
        request_timeout: Duration,
        openai: OpenAiRuntimeHandle,
        retry_delay: Arc<dyn Delay>,
    ) -> Self {
        Self {
            config: config.clone(),
            request_timeout,
            openai,
            retry_delay,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub(crate) fn reply_reason_max_chars(&self) -> usize {
        self.config.reply_reason_max_chars
    }

    pub(crate) fn review(&self, candidate: &SongReviewCandidate) -> SongReviewDecision {
        let threshold = self.threshold();
        if !self.enabled() {
            return SongReviewDecision {
                allowed: true,
                level: None,
                threshold,
                reason: "候选歌曲审核未启用".to_string(),
                tags: Vec::new(),
                attempts: 0,
                failed_open: false,
            };
        }

        let mut last_error = None;
        let max_attempts = self.config.retry_count.saturating_add(1);
        for attempt in 1..=max_attempts {
            match self.review_once(candidate) {
                Ok(result) => {
                    let allowed = result.level <= threshold;
                    return SongReviewDecision {
                        allowed,
                        level: Some(result.level),
                        threshold,
                        reason: result.reason,
                        tags: result.tags,
                        attempts: attempt,
                        failed_open: false,
                    };
                }
                Err(error) => {
                    log::warn!(
                        "候选歌曲审核请求失败 attempt={}/{}: {error:#}",
                        attempt,
                        max_attempts
                    );
                    last_error = Some(error.to_string());
                    if attempt < max_attempts {
                        self.retry_delay
                            .wait(Duration::from_millis(self.config.retry_delay_ms));
                    }
                }
            }
        }

        let reason = last_error
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "审核服务不可用".to_string());
        let allowed = self.config.failure_policy == SongReviewFailurePolicy::Allow;
        SongReviewDecision {
            allowed,
            level: None,
            threshold,
            reason,
            tags: Vec::new(),
            attempts: max_attempts,
            failed_open: allowed,
        }
    }

    fn threshold(&self) -> u8 {
        self.config.max_allowed_level.clamp(1, 10)
    }

    fn review_once(&self, candidate: &SongReviewCandidate) -> Result<SongReviewResult> {
        if candidate.uri.trim().is_empty() {
            bail!("候选歌曲审核缺少 URI");
        }
        if self.config.provider.endpoint.trim().is_empty() {
            bail!("song_review.provider.endpoint 未配置");
        }
        if self.config.provider.api_key.trim().is_empty() {
            bail!("song_review.provider.api_key 未配置");
        }
        if self.config.provider.model.trim().is_empty() {
            bail!("song_review.provider.model 未配置");
        }

        let request = build_review_request(&self.config, candidate)?;
        let reply = call_review_http(
            &self.openai,
            &self.config.provider.endpoint,
            &self.config.provider.api_key,
            request,
            &self.config.provider.extra_body,
            self.request_timeout,
        )?;
        let json_text = model_reply_json_object(&reply)?;
        parse_review_result(&json_text)
    }
}

fn build_review_request(
    config: &SongReviewConfig,
    candidate: &SongReviewCandidate,
) -> Result<CreateResponse> {
    Ok(CreateResponseArgs::default()
            .model(config.provider.model.clone())
            .instructions(
                "你是点歌审核助手，负责判断候选歌曲是否适合舒缓、轻松、不吵闹的房间氛围。必须只返回合法 JSON。",
            )
            .input(build_review_prompt(
                candidate,
                &config.policy_prompt,
                &config.custom_prompt,
            ))
            .text(ResponseFormatJsonSchema {
                description: Some("候选歌曲公开播放审核结果".to_string()),
                name: "song_review".to_string(),
                schema: review_schema(),
                strict: Some(true),
            })
            .tools(vec![Tool::WebSearch(WebSearchTool::default())])
            .tool_choice(ToolChoiceParam::Mode(ToolChoiceOptions::Required))
            .temperature(0.1)
            .top_p(0.95)
            .max_output_tokens(512_u32)
            .store(false)
            .stream(false)
            .build()?)
}

fn build_review_prompt(
    candidate: &SongReviewCandidate,
    policy_prompt: &str,
    custom_prompt: &str,
) -> String {
    let candidate_json = serde_json::to_string(candidate).unwrap_or_default();
    [
        "任务：审核即将播放或加入队列的候选歌曲是否适合“舒缓、轻松、不吵闹”的房间氛围。",
        "请尽量使用联网搜索结果判断，优先参考可靠来源中的曲风标签、歌曲介绍、歌词摘要、现场/混音版本说明和公开评论里的整体听感描述。",
        "联网搜索不到足够信息时，再根据歌曲名称、歌手、候选 URI 和用户提供描述保守判断。",
        "只返回 JSON，不要解释、不要注释、不要 Markdown 代码块。",
        "必须输出结构：{\"level\":number,\"reason\":string,\"tags\":[string]}。",
        "level 必须是 1 到 10 的整数，表达歌曲对房间氛围的打扰强度，不是推荐分、匹配分或好听程度。",
        "强度参考：1=很安静很舒缓；2=轻松柔和；3=舒缓抒情；4=中低强度但仍轻松；5=中等强度；6=略偏吵或节奏偏强；7=明显吵闹或情绪过激；8=炸场、压迫感强或强电子噪音；9=重金属、硬核、鬼畜、尖锐喊叫或强攻击性；10=极端吵闹混乱，明显破坏房间氛围。",
        "reason 用一句简短中文说明评级原因；不要复述敏感歌词或扩写敏感内容。",
        "tags 是简短标签数组，例如 calm、soft、healing、lyric、medium、noisy、electronic、metal、hardcore、meme、aggressive、unknown。",
        "审核条件：",
        if policy_prompt.trim().is_empty() {
            "无"
        } else {
            policy_prompt.trim()
        },
        &format!("候选歌曲上下文：{}", candidate_json),
        "追加审核规则：",
        if custom_prompt.trim().is_empty() {
            "无"
        } else {
            custom_prompt.trim()
        },
    ]
    .join("\n")
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "level": { "type": "integer", "minimum": 1, "maximum": 10 },
            "reason": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["level", "reason", "tags"],
        "additionalProperties": false
    })
}

fn call_review_http(
    openai: &OpenAiRuntimeHandle,
    endpoint: &str,
    api_key: &str,
    request: CreateResponse,
    extra_body: &HashMap<String, Value>,
    request_timeout: Duration,
) -> Result<String> {
    let target = Target::responses(endpoint, api_key)?;
    let value = openai
        .create_response(target, request, extra_body, request_timeout)?
        .wait()?;
    response_output_text(&value)
}

fn response_output_text(value: &Value) -> Result<String> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let error_type = response_error_token(error.get("type").and_then(Value::as_str));
        let code = response_error_token(error.get("code").and_then(Value::as_str));
        bail!("候选歌曲审核响应失败 type={error_type} code={code}");
    }
    if value.get("status").and_then(Value::as_str) != Some("completed") {
        let reason = value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .or_else(|| value.get("status").and_then(Value::as_str))
            .map_or("unknown", |reason| response_error_token(Some(reason)));
        bail!("候选歌曲审核响应未完成: {reason}");
    }
    let mut texts = Vec::new();
    for output in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for content in output
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match content.get("type").and_then(Value::as_str) {
                Some("refusal") => {
                    bail!("候选歌曲审核拒绝处理");
                }
                Some("output_text") => {
                    if let Some(text) = content
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    {
                        texts.push(text);
                    }
                }
                _ => {}
            }
        }
    }
    if texts.is_empty() {
        bail!("候选歌曲审核响应缺少 output_text");
    }
    Ok(texts.join(""))
}

fn response_error_token(value: Option<&str>) -> &str {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
        .unwrap_or("unknown")
}

fn model_reply_json_object(reply: &str) -> Result<String> {
    let trimmed = reply.trim();
    if serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object()) {
        return Ok(trimmed.to_string());
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("候选歌曲审核返回无效 JSON"))?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("候选歌曲审核返回无效 JSON"))?;
    let candidate = &trimmed[start..=end];
    if serde_json::from_str::<Value>(candidate).is_ok_and(|value| value.is_object()) {
        Ok(candidate.to_string())
    } else {
        bail!("候选歌曲审核返回无效 JSON")
    }
}

fn parse_review_result(text: &str) -> Result<SongReviewResult> {
    let value: Value = serde_json::from_str(text)?;
    let level = value
        .get("level")
        .and_then(Value::as_u64)
        .and_then(|level| u8::try_from(level).ok())
        .filter(|level| (1..=10).contains(level))
        .ok_or_else(|| anyhow::anyhow!("候选歌曲审核 JSON 字段无效: level"))?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let tags = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(SongReviewResult {
        level,
        reason,
        tags,
    })
}

pub(crate) fn split_candidate_title_artist(text: &str) -> (String, String) {
    let text = text.trim().trim_start_matches('#').trim();
    if let Some((title, artist)) = text.rsplit_once(" - ") {
        return (title.trim().to_string(), artist.trim().to_string());
    }
    (text.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::runtime::clock::{Clock, ManualClock};

    fn test_openai() -> OpenAiRuntimeHandle {
        static RUNTIME: std::sync::OnceLock<crate::runtime::openai::OpenAiRuntime> =
            std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                crate::runtime::openai::OpenAiRuntime::start().expect("test OpenAI runtime")
            })
            .handle()
    }

    #[test]
    fn retry_waits_use_the_injected_delay() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let started_at = clock.now();
        let config = SongReviewConfig {
            enabled: true,
            retry_count: 2,
            retry_delay_ms: 500,
            provider: SongReviewProviderConfig {
                endpoint: "https://example.com/v1/responses".to_string(),
                api_key: "key".to_string(),
                model: "gpt-5.6".to_string(),
                http_proxy: String::new(),
                extra_body: HashMap::new(),
            },
            ..SongReviewConfig::default()
        };
        let client = SongReviewClient::new(
            &config,
            Duration::from_secs(1),
            test_openai(),
            clock.clone(),
        );
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "测试".to_string(),
            artist: "歌手".to_string(),
            uri: String::new(),
            message_type: "大厅".to_string(),
            username: "测试者".to_string(),
        };

        let decision = client.review(&candidate);

        assert_eq!(decision.attempts, 3);
        assert_eq!(clock.now(), started_at + Duration::from_secs(1));
    }

    #[test]
    fn enabled_review_configuration_requires_a_valid_responses_endpoint() {
        let mut config = SongReviewConfig {
            enabled: true,
            ..SongReviewConfig::default()
        };
        config.provider.api_key = "key".to_string();
        config.provider.model = "gpt-5.6".to_string();
        config.provider.endpoint = "https://example.com/v1/chat/completions".to_string();

        assert!(config.validate().is_err());

        config.provider.endpoint = "https://example.com/v1/responses".to_string();
        config.validate().expect("valid Responses provider");
    }

    #[test]
    fn enabled_review_configuration_rejects_an_unsupported_http_proxy() {
        let mut config = SongReviewConfig {
            enabled: true,
            ..SongReviewConfig::default()
        };
        config.provider.api_key = "key".to_string();
        config.provider.model = "gpt-5.6".to_string();
        config.provider.endpoint = "https://example.com/v1/responses".to_string();
        config.provider.http_proxy = "socks5://127.0.0.1:1080".to_string();

        assert!(config.validate().is_err());
    }

    #[test]
    fn disabled_review_configuration_still_rejects_an_unsupported_http_proxy() {
        let mut config = SongReviewConfig::default();
        config.provider.http_proxy = "socks5://127.0.0.1:1080".to_string();

        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_review_level_and_tags() {
        let result = parse_review_result(r#"{"level":4,"reason":"正常","tags":["safe"]}"#)
            .expect("review result");

        assert_eq!(result.level, 4);
        assert_eq!(result.reason, "正常");
        assert_eq!(result.tags, vec!["safe".to_string()]);
    }

    #[test]
    fn rejects_invalid_review_level() {
        assert!(parse_review_result(r#"{"level":11,"reason":"x"}"#).is_err());
        assert!(parse_review_result(r#"{"level":0,"reason":"x"}"#).is_err());
        assert!(parse_review_result(r#"{"reason":"x"}"#).is_err());
    }

    #[test]
    fn splits_candidate_text() {
        let (title, artist) = split_candidate_title_artist("# 晴天 - 周杰伦");

        assert_eq!(title, "晴天");
        assert_eq!(artist, "周杰伦");
    }

    #[test]
    fn response_output_text_handles_completed_refusal_and_incomplete_results() {
        let completed = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "{\"level\":2}" }]
            }]
        });
        assert_eq!(
            response_output_text(&completed).expect("completed output"),
            "{\"level\":2}"
        );

        let refusal = json!({
            "status": "completed",
            "output": [{ "content": [{ "type": "refusal", "refusal": "blocked" }] }]
        });
        assert!(response_output_text(&refusal).is_err());

        let incomplete = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": []
        });
        assert!(response_output_text(&incomplete).is_err());

        let sensitive_incomplete = json!({
            "status": "provider private status",
            "incomplete_details": { "reason": "provider private reason" },
            "output": []
        });
        let error = response_output_text(&sensitive_incomplete)
            .expect_err("sensitive status must still fail")
            .to_string();
        assert!(!error.contains("provider-private"));
    }

    #[test]
    fn review_request_schema_is_strict() {
        let schema = review_schema();

        assert_eq!(schema["required"], json!(["level", "reason", "tags"]));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn review_request_uses_standard_responses_web_search() {
        let mut config = SongReviewConfig::default();
        config.provider.model = "gpt-5-mini".to_string();
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            uri: "fuo://qqmusic/songs/1".to_string(),
            message_type: "大厅".to_string(),
            username: "测试".to_string(),
        };
        let request = build_review_request(&config, &candidate).expect("responses request");
        let body = serde_json::to_value(request).expect("request json");

        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], false);
        assert!(body.get("enable_search").is_none());
        assert!(body.get("search_options").is_none());
        assert!(body.get("enable_thinking").is_none());
    }
}
