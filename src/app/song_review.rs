use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{SongReviewConfig, SongReviewFailurePolicy, TimingConfig};

#[derive(Clone)]
pub(super) struct SongReviewClient {
    config: SongReviewConfig,
    timing: TimingConfig,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SongReviewCandidate {
    pub source: String,
    pub title: String,
    pub artist: String,
    pub uri: String,
    pub message_type: String,
    pub username: String,
}

#[derive(Clone, Debug)]
pub(super) struct SongReviewDecision {
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
    pub(super) fn new(config: &SongReviewConfig, timing: &TimingConfig) -> Self {
        Self {
            config: config.clone(),
            timing: timing.clone(),
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub(super) fn reply_reason_max_chars(&self) -> usize {
        self.config.reply_reason_max_chars
    }

    pub(super) fn review(&self, candidate: &SongReviewCandidate) -> SongReviewDecision {
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
                        sleep(Duration::from_millis(self.config.retry_delay_ms));
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

        let body = json!({
            "model": self.config.provider.model,
            "messages": [
                {
                    "role": "system",
                    "content": "你是点歌审核助手，负责判断候选歌曲是否适合舒缓、轻松、不吵闹的房间氛围。必须只返回合法 JSON。"
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": build_review_prompt(
                                candidate,
                                &self.config.policy_prompt,
                                &self.config.custom_prompt,
                            )
                        }
                    ]
                }
            ],
            "response_format": { "type": "json_object" },
            "enable_search": true,
            "search_options": {
                "forced_search": true
            },
            "enable_thinking": false,
            "temperature": 0.1,
            "stream": false,
            "max_completion_tokens": 512,
            "top_p": 0.95
        })
        .to_string();
        let reply = call_review_http(
            &self.config.provider.endpoint,
            &self.config.provider.api_key,
            &body,
            &self.timing,
        )?;
        let json_text = model_reply_json_object(&reply)?;
        parse_review_result(&json_text)
    }
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

fn call_review_http(
    endpoint: &str,
    api_key: &str,
    body: &str,
    timing: &TimingConfig,
) -> Result<String> {
    let response = Client::builder()
        .timeout(Duration::from_millis(timing.external.ai_request_timeout_ms))
        .build()
        .context("创建候选歌曲审核 HTTP 客户端失败")?
        .post(endpoint)
        .headers(review_headers(api_key)?)
        .body(body.to_string())
        .send()
        .context("候选歌曲审核请求失败")?;
    let status = response.status();
    let text = response.text().context("读取候选歌曲审核响应失败")?;
    if !status.is_success() {
        bail!(
            "候选歌曲审核响应失败 status={} body={}",
            status,
            error_excerpt(&text)
        );
    }
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("候选歌曲审核响应缺少 choices[0].message.content"))
}

fn review_headers(api_key: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key))
            .context("song_review.provider.api_key 不是有效 HTTP header")?,
    );
    Ok(headers)
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

fn error_excerpt(text: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 500;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_ERROR_BODY_CHARS {
        normalized
    } else {
        format!(
            "{}...",
            normalized
                .chars()
                .take(MAX_ERROR_BODY_CHARS)
                .collect::<String>()
        )
    }
}

pub(super) fn split_candidate_title_artist(text: &str) -> (String, String) {
    let text = text.trim().trim_start_matches('#').trim();
    if let Some((title, artist)) = text.rsplit_once(" - ") {
        return (title.trim().to_string(), artist.trim().to_string());
    }
    (text.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
