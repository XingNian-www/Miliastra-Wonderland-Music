use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use async_openai::types::responses::{
    CreateResponse, CreateResponseArgs, OutputItem, OutputMessageContent, Response,
    ResponseFormatJsonSchema, Status, Tool, ToolChoiceOptions, ToolChoiceParam, WebSearchTool,
};
use miliastra_playback::TrackKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::ai::{AiConfig, ResolvedAiTarget, WireApi};
use miliastra_kernel::ai::OpenAiRuntimeHandle;
use miliastra_kernel::clock::Delay;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SongReviewConfig {
    pub enabled: bool,
    pub max_allowed_level: u8,
    pub failure_policy: SongReviewFailurePolicy,
    pub retry_count: u32,
    pub retry_delay_ms: u64,
    pub reply_reason_max_chars: usize,
    /// Responses API 的系统提示词；留空时使用内置默认值。
    #[serde(default = "default_song_review_system_prompt")]
    pub system_prompt: String,
    /// 审核提示词模板，可使用 {{policy_prompt}}、{{custom_prompt}}、{{candidate}}。
    #[serde(default = "default_song_review_prompt")]
    pub review_prompt: String,
    pub policy_prompt: String,
    pub custom_prompt: String,
    /// 可选的审核专用模型；留空沿用 ai.model。
    #[serde(default)]
    pub model: String,
    /// 思考强度；unset 表示不显式发送。
    #[serde(default)]
    pub reasoning: crate::config::ai::ReasoningEffort,
    /// 是否要求模型联网搜索后再判断；网关不支持 web_search 时应关闭。
    #[serde(default = "default_web_search")]
    pub web_search: bool,
    /// 单次审核允许生成的最大 Token。
    #[serde(default = "default_review_max_tokens")]
    pub max_tokens: u32,
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
            system_prompt: default_song_review_system_prompt(),
            review_prompt: default_song_review_prompt(),
            policy_prompt: default_song_review_policy_prompt(),
            custom_prompt: String::new(),
            model: String::new(),
            reasoning: Default::default(),
            web_search: default_web_search(),
            max_tokens: default_review_max_tokens(),
        }
    }
}

impl SongReviewConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_tokens == 0 {
            bail!("song_review.max_tokens 必须大于 0");
        }
        if !self.enabled {
            return Ok(());
        }
        if !(1..=10).contains(&self.max_allowed_level) {
            bail!("song_review.max_allowed_level 必须在 1 到 10 之间");
        }
        if self.reply_reason_max_chars == 0 {
            bail!("song_review.reply_reason_max_chars 必须大于 0");
        }
        Ok(())
    }
}

fn default_web_search() -> bool {
    true
}

fn default_review_max_tokens() -> u32 {
    512
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

#[derive(Clone)]
pub(crate) struct SongReviewClient {
    config: SongReviewConfig,
    ai: AiConfig,
    request_timeout: Duration,
    openai: OpenAiRuntimeHandle,
    retry_delay: Arc<dyn Delay>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SongReviewCandidate {
    pub source: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<u64>,
    pub track_key: TrackKey,
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
        ai: &AiConfig,
        request_timeout: Duration,
        openai: OpenAiRuntimeHandle,
        retry_delay: Arc<dyn Delay>,
    ) -> Self {
        Self {
            config: config.clone(),
            ai: ai.clone(),
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
        if self.config.web_search && self.ai.wire_api != WireApi::Responses {
            bail!("歌曲审核需要 web_search，必须使用 ai.wire_api=responses");
        }
        let target = self.ai.resolve_with(&crate::config::ai::AiCallOptions {
            model: self.config.model.clone(),
            reasoning: self.config.reasoning,
        })?;
        let request = build_review_request(&self.config, &target, candidate)?;
        let reply = call_review_http(&self.openai, &target, request, self.request_timeout)?;
        let json_text = model_reply_json_object(&reply)?;
        parse_review_result(&json_text)
    }
}

fn build_review_request(
    config: &SongReviewConfig,
    target: &ResolvedAiTarget,
    candidate: &SongReviewCandidate,
) -> Result<CreateResponse> {
    let mut request = CreateResponseArgs::default();
    request
        .model(target.model.clone())
        .instructions(if config.system_prompt.trim().is_empty() {
            default_song_review_system_prompt()
        } else {
            config.system_prompt.clone()
        })
        .input(render_song_review_prompt(
            &config.review_prompt,
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
        .temperature(0.1_f32)
        .top_p(0.95_f32)
        .max_output_tokens(config.max_tokens)
        .store(false)
        .stream(false);
    if config.web_search {
        request
            .tools(vec![Tool::WebSearch(WebSearchTool::default())])
            .tool_choice(ToolChoiceParam::Mode(ToolChoiceOptions::Required));
    }
    Ok(request.build()?)
}

fn default_song_review_system_prompt() -> String {
    "你是点歌审核助手，负责判断候选歌曲是否适合舒缓、轻松、不吵闹的房间氛围。必须只返回合法 JSON。"
        .to_string()
}

fn default_song_review_prompt() -> String {
    [
        "任务：审核即将播放或加入队列的候选歌曲是否适合“舒缓、轻松、不吵闹”的房间氛围。",
        "请尽量使用联网搜索结果判断，优先参考可靠来源中的曲风标签、歌曲介绍、歌词摘要、现场/混音版本说明和公开评论里的整体听感描述。",
        "联网搜索不到足够信息时，再根据歌曲名称、歌手、候选 URI、歌曲时长和用户提供描述保守判断。",
        "歌曲时长是辅助参考：短时长可能是片段或循环版本，超长可能是现场、合集或特殊版本，不得仅凭时长单独决定评级。",
        "只返回 JSON，不要解释、不要注释、不要 Markdown 代码块。",
        "必须输出结构：{\"level\":number,\"reason\":string,\"tags\":[string]}。",
        "level 必须是 1 到 10 的整数，表达歌曲对房间氛围的打扰强度，不是推荐分、匹配分或好听程度。",
        "强度参考：1=很安静很舒缓；2=轻松柔和；3=舒缓抒情；4=中低强度但仍轻松；5=中等强度；6=略偏吵或节奏偏强；7=明显吵闹或情绪过激；8=炸场、压迫感强或强电子噪音；9=重金属、硬核、鬼畜、尖锐喊叫或强攻击性；10=极端吵闹混乱，明显破坏房间氛围。",
        "reason 用一句简短中文说明评级原因；不要复述敏感歌词或扩写敏感内容。",
        "tags 是简短标签数组，例如 calm、soft、healing、lyric、medium、noisy、electronic、metal、hardcore、meme、aggressive、unknown。",
        "审核条件：\n{{policy_prompt}}",
        "候选歌曲上下文：{{candidate}}",
        "追加审核规则：\n{{custom_prompt}}",
    ]
    .join("\n")
}

fn render_song_review_prompt(
    template: &str,
    candidate: &SongReviewCandidate,
    policy_prompt: &str,
    custom_prompt: &str,
) -> String {
    let candidate_json = serde_json::to_string(candidate).unwrap_or_default();
    let policy_prompt = if policy_prompt.trim().is_empty() {
        "无"
    } else {
        policy_prompt.trim()
    };
    let custom_prompt = if custom_prompt.trim().is_empty() {
        "无"
    } else {
        custom_prompt.trim()
    };
    let template = if template.trim().is_empty() {
        default_song_review_prompt()
    } else {
        template.to_string()
    };
    let mut rendered = template
        .replace("{{policy_prompt}}", policy_prompt)
        .replace("{{custom_prompt}}", custom_prompt)
        .replace("{{candidate}}", &candidate_json);
    let mut missing_context = Vec::new();
    if !template.contains("{{policy_prompt}}") {
        missing_context.push(format!("审核条件：{policy_prompt}"));
    }
    if !template.contains("{{custom_prompt}}") {
        missing_context.push(format!("追加审核规则：{custom_prompt}"));
    }
    if !template.contains("{{candidate}}") {
        missing_context.push(format!("候选歌曲上下文：{candidate_json}"));
    }
    if !missing_context.is_empty() {
        rendered.push('\n');
        rendered.push_str(&missing_context.join("\n"));
    }
    rendered
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
    target: &ResolvedAiTarget,
    request: CreateResponse,
    request_timeout: Duration,
) -> Result<String> {
    let value = openai
        .create_response(
            target.build_target()?,
            request,
            &crate::config::ai::merged_extra_body(target),
            request_timeout,
        )?
        .wait()?;
    response_output_text(&value)
}

fn response_output_text(value: &Response) -> Result<String> {
    if let Some(error) = &value.error {
        let code = response_error_token(Some(error.code.as_str()));
        bail!("候选歌曲审核响应失败 code={code}");
    }
    if value.status != Status::Completed {
        let reason = value
            .incomplete_details
            .as_ref()
            .map(|details| details.reason.as_str())
            .or_else(|| {
                Some(match value.status {
                    Status::Failed => "failed",
                    Status::InProgress => "in_progress",
                    Status::Cancelled => "cancelled",
                    Status::Queued => "queued",
                    Status::Incomplete => "incomplete",
                    Status::Completed => "unknown",
                })
            })
            .map_or("unknown", |reason| response_error_token(Some(reason)));
        bail!("候选歌曲审核响应未完成: {reason}");
    }
    let mut texts = Vec::new();
    for output in &value.output {
        if let OutputItem::Message(message) = output {
            for content in &message.content {
                match content {
                    OutputMessageContent::Refusal(_) => {
                        bail!("候选歌曲审核拒绝处理");
                    }
                    OutputMessageContent::OutputText(text) => {
                        let text = text.text.trim();
                        if !text.is_empty() {
                            texts.push(text);
                        }
                    }
                }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::features::playback::test_track;
    use miliastra_kernel::clock::{Clock, ManualClock};

    fn test_openai() -> OpenAiRuntimeHandle {
        static RUNTIME: std::sync::OnceLock<miliastra_kernel::ai::OpenAiRuntime> =
            std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                miliastra_kernel::ai::OpenAiRuntime::start().expect("test OpenAI runtime")
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
            ..SongReviewConfig::default()
        };
        let client = SongReviewClient::new(
            &config,
            &test_ai_config(),
            Duration::from_secs(1),
            test_openai(),
            clock.clone(),
        );
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "测试".to_string(),
            artist: "歌手".to_string(),
            duration_ms: None,
            track_key: test_track("miliastra://track/qqmusic/retry-test", "测试 - 歌手")
                .track_ref
                .key,
            message_type: "大厅".to_string(),
            username: "测试者".to_string(),
        };

        let decision = client.review(&candidate);

        assert_eq!(decision.attempts, 3);
        assert_eq!(clock.now(), started_at + Duration::from_secs(1));
    }

    fn test_ai_config() -> AiConfig {
        AiConfig {
            provider: "openai".to_string(),
            api_key: "key".to_string(),
            endpoint: "https://example.com/v1/chat/completions".to_string(),
            model: "gpt-5.6".to_string(),
            ..AiConfig::default()
        }
    }

    #[test]
    fn web_search_review_requires_the_responses_wire_api() {
        let config = SongReviewConfig {
            enabled: true,
            web_search: true,
            ..SongReviewConfig::default()
        };
        let mut ai = test_ai_config();
        ai.wire_api = WireApi::ChatCompletions;
        let client = SongReviewClient::new(
            &config,
            &ai,
            Duration::from_secs(1),
            test_openai(),
            Arc::new(ManualClock::new(Instant::now())),
        );
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "测试".to_string(),
            artist: "歌手".to_string(),
            duration_ms: None,
            track_key: test_track("miliastra://track/qqmusic/wire", "测试 - 歌手")
                .track_ref
                .key,
            message_type: "大厅".to_string(),
            username: "测试者".to_string(),
        };

        let decision = client.review(&candidate);

        assert!(decision.reason.contains("wire_api"), "{}", decision.reason);
    }

    #[test]
    fn review_max_tokens_must_be_positive() {
        let mut config = SongReviewConfig {
            enabled: true,
            ..SongReviewConfig::default()
        };
        config.max_tokens = 0;

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
    fn configured_review_prompt_keeps_policy_and_candidate_context() {
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            duration_ms: Some(240_000),
            track_key: test_track("miliastra://track/qqmusic/prompt", "晴天 - 周杰伦")
                .track_ref
                .key,
            message_type: "大厅".to_string(),
            username: "测试".to_string(),
        };
        let rendered = render_song_review_prompt(
            "自定义审核规则",
            &candidate,
            "只允许舒缓歌曲",
            "本房间偏好轻音乐",
        );
        assert!(rendered.contains("自定义审核规则"));
        assert!(rendered.contains("审核条件：只允许舒缓歌曲"));
        assert!(rendered.contains("追加审核规则：本房间偏好轻音乐"));
        assert!(rendered.contains("晴天"));
    }

    #[test]
    fn response_output_text_handles_completed_refusal_and_incomplete_results() {
        let completed: Response = serde_json::from_value(json!({
            "id": "resp-test",
            "created_at": 0,
            "model": "test-model",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg-test",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "{\"level\":2}", "annotations": [] }]
            }]
        }))
        .expect("typed completed response");
        assert_eq!(
            response_output_text(&completed).expect("completed output"),
            "{\"level\":2}"
        );

        let refusal: Response = serde_json::from_value(json!({
            "id": "resp-test",
            "created_at": 0,
            "model": "test-model",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg-test",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "refusal", "refusal": "blocked" }]
            }]
        }))
        .expect("typed refusal response");
        assert!(response_output_text(&refusal).is_err());

        let incomplete: Response = serde_json::from_value(json!({
            "id": "resp-test",
            "created_at": 0,
            "model": "test-model",
            "object": "response",
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": []
        }))
        .expect("typed incomplete response");
        assert!(response_output_text(&incomplete).is_err());

        let sensitive_incomplete: Response = serde_json::from_value(json!({
            "id": "resp-test",
            "created_at": 0,
            "model": "test-model",
            "object": "response",
            "status": "incomplete",
            "incomplete_details": { "reason": "provider private reason" },
            "output": []
        }))
        .expect("typed sensitive response");
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
        config.model = "gpt-5-mini".to_string();
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            duration_ms: None,
            track_key: test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")
                .track_ref
                .key,
            message_type: "大厅".to_string(),
            username: "测试".to_string(),
        };
        let target = test_ai_config()
            .resolve_with(&crate::config::ai::AiCallOptions {
                model: config.model.clone(),
                reasoning: config.reasoning,
            })
            .expect("resolved target");
        let request =
            build_review_request(&config, &target, &candidate).expect("responses request");
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

    #[test]
    fn configured_review_prompt_replaces_policy_and_candidate_variables() {
        let mut config = SongReviewConfig::default();
        config.model = "gpt-5-mini".to_string();
        config.system_prompt = "自定义审核角色".to_string();
        config.review_prompt =
            "规则={{policy_prompt}}\n歌曲={{candidate}}\n附加={{custom_prompt}}".to_string();
        config.policy_prompt = "只允许舒缓歌曲".to_string();
        config.custom_prompt = "本房间偏好钢琴曲".to_string();
        let candidate = SongReviewCandidate {
            source: "qqmusic".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            duration_ms: Some(269_000),
            track_key: test_track("miliastra://track/qqmusic/prompt-test", "晴天 - 周杰伦")
                .track_ref
                .key,
            message_type: "大厅".to_string(),
            username: "测试者".to_string(),
        };
        let target = test_ai_config()
            .resolve_with(&crate::config::ai::AiCallOptions {
                model: config.model.clone(),
                reasoning: config.reasoning,
            })
            .expect("resolved target");
        let request =
            build_review_request(&config, &target, &candidate).expect("responses request");
        let body = serde_json::to_value(request).expect("request json");
        assert_eq!(body["instructions"], "自定义审核角色");
        let input = body["input"].as_str().expect("input text");
        assert!(input.contains("只允许舒缓歌曲"));
        assert!(input.contains("prompt-test"));
        assert!(input.contains("本房间偏好钢琴曲"));
    }
}
