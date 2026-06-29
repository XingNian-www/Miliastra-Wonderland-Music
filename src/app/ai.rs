use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};

use super::config::{AiConfig, TimingConfig};
use super::feeluown::{FeelUOwnClient, SearchCandidate};

const MIMO_ENDPOINT: &str = "https://api.xiaomimimo.com/v1/chat/completions";
const MIMO_MODEL: &str = "mimo-v2.5";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const OPENAI_MODEL: &str = "gpt-4o-mini";
const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";
const DEEPSEEK_MODEL: &str = "deepseek-chat";
const CANDIDATE_PICK_LIMIT: usize = 30;

#[derive(Clone)]
pub struct AiClient {
    config: AiConfig,
    timing: TimingConfig,
}

#[derive(Clone, Debug)]
pub struct AiMatchResult {
    pub matched: bool,
    pub reason: String,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AiCandidatePickResult {
    pub uri: String,
    pub reason: String,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AiSearchResult {
    pub request: String,
    pub candidates: Vec<SearchCandidate>,
    pub pick: Option<AiCandidatePickResult>,
}

#[derive(Clone, Debug)]
struct AiProviderConfig {
    provider: AiProvider,
    endpoint: String,
    api_key: String,
    model: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiProvider {
    Mimo,
    OpenAi,
    DeepSeek,
    Custom,
}

impl AiClient {
    pub fn new(config: &AiConfig, timing: &TimingConfig) -> Self {
        Self {
            config: config.clone(),
            timing: timing.clone(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.config.api_key.trim().is_empty()
    }

    pub fn match_same_song(
        &self,
        request: &str,
        song_name: &str,
        song_singer: &str,
    ) -> Result<AiMatchResult> {
        let provider = resolve_provider_config(&self.config, None)?;
        let request = normalize_required(request, "request")?;
        let song_name = normalize_required(song_name, "songName")?;
        let song_singer = assert_no_control_chars(song_singer, "songSinger")?
            .trim()
            .to_string();
        let reply = call_ai(
            &provider,
            &build_match_prompt(&request, &song_name, &song_singer),
            1024,
            &self.timing,
        )?;
        let json_text = model_reply_json_object(&reply)?;
        validate_match_json(&json_text)?;
        let value: Value = serde_json::from_str(&json_text)?;
        Ok(AiMatchResult {
            matched: value.get("match").and_then(Value::as_bool).unwrap_or(false)
                || value.get("decision").and_then(Value::as_str) == Some("match"),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            score: value.get("score").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }

    pub fn pick_song_candidate(
        &self,
        request: &str,
        prefer_accompaniment: bool,
        candidates: &[SearchCandidate],
    ) -> Result<AiCandidatePickResult> {
        let provider = resolve_provider_config(&self.config, None)?;
        let request = normalize_required(request, "request")?;
        let candidates = candidates
            .iter()
            .take(CANDIDATE_PICK_LIMIT)
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            bail!("缺少搜索候选");
        }
        let reply = call_ai(
            &provider,
            &build_candidate_pick_prompt(&request, prefer_accompaniment, &candidates),
            2048,
            &self.timing,
        )?;
        let json_text = model_reply_json_object(&reply)?;
        validate_candidate_pick_json(&json_text, &candidates)?;
        parse_candidate_pick_result(&json_text)
    }

    pub fn search_and_pick(
        &self,
        feeluown: &FeelUOwnClient,
        keyword: &str,
        prefer_accompaniment: bool,
    ) -> Result<AiSearchResult> {
        let candidates = feeluown.search_candidates(keyword, "")?;
        if candidates.is_empty() {
            return Ok(AiSearchResult {
                request: keyword.to_string(),
                candidates: Vec::new(),
                pick: None,
            });
        }
        let pick = self.pick_song_candidate(keyword, prefer_accompaniment, &candidates)?;
        Ok(AiSearchResult {
            request: keyword.to_string(),
            candidates,
            pick: Some(pick),
        })
    }
}

pub fn recognize_with_query(
    config: &AiConfig,
    timing: &TimingConfig,
    query: &[(String, String)],
) -> Result<String> {
    let provider = resolve_provider_config(config, Some(query))?;
    let text = normalize_required(query_value(query, "text").unwrap_or(""), "text")?;
    let reply = call_ai(&provider, &build_recognize_prompt(&text), 1024, timing)?;
    let json = model_reply_json_object(&reply)?;
    validate_recognize_json(&json)?;
    Ok(json)
}

pub fn match_with_query(
    config: &AiConfig,
    timing: &TimingConfig,
    query: &[(String, String)],
) -> Result<String> {
    let provider = resolve_provider_config(config, Some(query))?;
    let request = normalize_required(query_value(query, "request").unwrap_or(""), "request")?;
    let song_name = normalize_required(query_value(query, "songName").unwrap_or(""), "songName")?;
    let song_singer =
        assert_no_control_chars(query_value(query, "songSinger").unwrap_or(""), "songSinger")?
            .trim()
            .to_string();
    let reply = call_ai(
        &provider,
        &build_match_prompt(&request, &song_name, &song_singer),
        1024,
        timing,
    )?;
    let json = model_reply_json_object(&reply)?;
    validate_match_json(&json)?;
    Ok(json)
}

pub fn pick_with_query(
    config: &AiConfig,
    timing: &TimingConfig,
    query: &[(String, String)],
) -> Result<String> {
    let provider = resolve_provider_config(config, Some(query))?;
    let request = normalize_required(query_value(query, "request").unwrap_or(""), "request")?;
    let prefer_accompaniment = parse_bool(query_value(query, "preferAccompaniment"));
    let candidates = parse_query_candidates(query_value(query, "candidates").unwrap_or(""))?;
    let reply = call_ai(
        &provider,
        &build_candidate_pick_prompt(&request, prefer_accompaniment, &candidates),
        2048,
        timing,
    )?;
    let json = model_reply_json_object(&reply)?;
    validate_candidate_pick_json(&json, &candidates)?;
    Ok(json)
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .rev()
        .find(|(item_key, _)| item_key == key)
        .map(|(_, value)| value.as_str())
}

fn parse_bool(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or("").trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_query_candidates(text: &str) -> Result<Vec<SearchCandidate>> {
    let value: Value = serde_json::from_str(text).context("candidates参数必须是JSON数组")?;
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("candidates参数必须是JSON数组"))?;
    let mut candidates = Vec::new();
    for item in array {
        let uri = item.get("uri").and_then(Value::as_str).unwrap_or("").trim();
        if uri.is_empty() {
            continue;
        }
        candidates.push(SearchCandidate {
            uri: uri.to_string(),
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
        });
    }
    if candidates.is_empty() {
        bail!("candidates参数缺少有效候选");
    }
    Ok(candidates)
}

fn resolve_provider_config(
    config: &AiConfig,
    query: Option<&[(String, String)]>,
) -> Result<AiProviderConfig> {
    let query_value = |key| query.and_then(|items| query_value(items, key));
    let provider = parse_provider(query_value("provider").unwrap_or(&config.provider))?;
    let api_key = normalize_api_key(
        query_value("apiKey")
            .or_else(|| query_value("api_key"))
            .unwrap_or(config.api_key.as_str()),
    )?;
    let endpoint = resolve_endpoint(
        provider,
        query_value("endpoint").unwrap_or(config.endpoint.as_str()),
    )?;
    let model = resolve_model(
        provider,
        query_value("model").unwrap_or(config.model.as_str()),
    )?;
    Ok(AiProviderConfig {
        provider,
        endpoint,
        api_key,
        model,
    })
}

fn parse_provider(value: &str) -> Result<AiProvider> {
    match normalize_optional_text(value, "provider")?
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "mimo" => Ok(AiProvider::Mimo),
        "openai" => Ok(AiProvider::OpenAi),
        "deepseek" => Ok(AiProvider::DeepSeek),
        "custom" | "openai-compatible" | "openai_compatible" => Ok(AiProvider::Custom),
        _ => bail!("provider只允许mimo/openai/deepseek/custom"),
    }
}

fn resolve_endpoint(provider: AiProvider, value: &str) -> Result<String> {
    let text = normalize_optional_text(value, "endpoint")?;
    let endpoint = if text.is_empty() {
        match provider {
            AiProvider::Mimo => MIMO_ENDPOINT,
            AiProvider::OpenAi => OPENAI_ENDPOINT,
            AiProvider::DeepSeek => DEEPSEEK_ENDPOINT,
            AiProvider::Custom => bail!("custom provider缺少endpoint"),
        }
        .to_string()
    } else {
        text
    };
    if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
        bail!("endpoint必须以http://或https://开头")
    }
    Ok(endpoint)
}

fn resolve_model(provider: AiProvider, value: &str) -> Result<String> {
    let text = normalize_optional_text(value, "model")?;
    if !text.is_empty() {
        return Ok(text);
    }
    Ok(match provider {
        AiProvider::Mimo => MIMO_MODEL,
        AiProvider::OpenAi => OPENAI_MODEL,
        AiProvider::DeepSeek => DEEPSEEK_MODEL,
        AiProvider::Custom => bail!("custom provider缺少model"),
    }
    .to_string())
}

fn normalize_api_key(value: &str) -> Result<String> {
    let text = normalize_required(value, "apiKey")?;
    Ok(text)
}

fn normalize_optional_text(value: &str, name: &str) -> Result<String> {
    Ok(assert_no_control_chars(value, name)?.trim().to_string())
}

fn normalize_required(value: &str, name: &str) -> Result<String> {
    let text = assert_no_control_chars(value, name)?.trim().to_string();
    if text.is_empty() {
        bail!("缺少{}参数", name);
    }
    Ok(text)
}

fn assert_no_control_chars<'a>(value: &'a str, name: &str) -> Result<&'a str> {
    if value
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
    {
        bail!("{}参数包含控制字符", name);
    }
    Ok(value)
}

fn build_recognize_prompt(text: &str) -> String {
    [
        "任务：读点歌文本。",
        "只返回 JSON，不要解释、不要注释、不要 Markdown 代码块。",
        "必须输出结构：{\"recognizedText\":string,\"songName\":string,\"songSinger\":string,\"searchText\":string,\"confidence\":number}。",
        "confidence 范围 0 到 1。所有字段必须存在，不确定的字符串字段填空字符串。",
        "最高优先级：不要漏字、不要漏符号。完整抄下 @AI点歌/@点歌/@QQ点歌/@网易点歌 后面的所有可见内容。",
        "不要纠错、不要翻译、不要转写，不要删除或改写任何可见字符。",
        "不要根据歌手名、常识或热门歌曲补全；看不清的字符用 ? 占位，confidence 降低，也不要猜成另一个字。",
        "recognizedText=命令后的完整原文；searchText 默认等于 recognizedText，只能去掉首尾空白。",
        "songName/songSinger 只是附加猜测；如果分不清，就 songName=recognizedText，songSinger 置空。",
        "示例：{\"recognizedText\":\"晴天 周杰伦\",\"songName\":\"晴天\",\"songSinger\":\"周杰伦\",\"searchText\":\"晴天 周杰伦\",\"confidence\":0.95}",
        &format!("文本补充：{}", text),
    ]
    .join("\n")
}

fn build_match_prompt(request: &str, song_name: &str, song_singer: &str) -> String {
    [
        "任务：判断用户点歌文字和平台返回歌曲是否同一首。",
        "只返回 JSON，不要解释、不要注释、不要 Markdown 代码块。",
        "必须输出结构：{\"match\":boolean,\"decision\":\"match\"|\"no_match\",\"score\":number,\"reason\":string}。",
        "score 范围 0 到 1。所有字段必须存在。",
        "允许：1-2处错别字/OCR误读、漏字/多字、大小写/空格/标点差异、歌手别名、日文歌中文译名/罗马音/假名汉字差异、版本后缀。",
        "不要只因歌手相同就判同一首；歌名明显不同必须 no_match。",
        "如果基本确定同一首，decision=match；不确定或明显不同，decision=no_match。",
        "decision=match 时 match=true；decision=no_match 时 match=false。",
        "示例：{\"match\":false,\"decision\":\"no_match\",\"score\":0.12,\"reason\":\"歌名不同\"}",
        &format!("用户点歌：{}", request),
        &format!("平台歌名：{}", song_name),
        &format!("平台歌手：{}", song_singer),
    ]
    .join("\n")
}

fn build_candidate_pick_prompt(
    request: &str,
    prefer_accompaniment: bool,
    candidates: &[SearchCandidate],
) -> String {
    let candidates_json = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            json!({
                "index": index + 1,
                "uri": candidate.uri,
                "text": candidate.text,
            })
        })
        .collect::<Vec<_>>();
    [
        "任务：从 FeelUOwn 搜索候选中选出最适合用户点歌的一首。".to_string(),
        "只返回 JSON，不要解释、不要注释、不要 Markdown 代码块。".to_string(),
        "必须输出结构：{\"uri\":string,\"score\":number,\"reason\":string}。".to_string(),
        "uri 必须逐字等于候选列表中的一个 uri，不能编造，不能改写。".to_string(),
        "歌名和歌手以字面匹配为主：用户输入的每个关键词应在候选标题中找到对应文字（允许大小写、空格、标点差异）。".to_string(),
        "翻译名、别名、罗马音可作为补充匹配，但优先级低于字面匹配。".to_string(),
        "不要仅凭语义相近就选择字面完全不同的歌名或歌手。".to_string(),
        "优先原唱、正式版、清晰标题。".to_string(),
        "避开翻唱、DJ、钢琴版、纯音乐、Live、片段、伴奏，除非用户明确要求。".to_string(),
        "伴奏标记包括但不限于：伴奏、伴唱、纯伴奏、纯伴唱、Inst.、Instrumental、Karaoke、KTV、消音、minus one，看到这些标记视为伴奏版。".to_string(),
        if prefer_accompaniment {
            "用户明确要求伴奏或伴唱，优先选择伴奏/伴唱候选。".to_string()
        } else {
            "用户没有要求伴奏，不要选择任何带伴奏标记的候选。".to_string()
        },
        "不要因为平台偏好压过歌名和歌手的匹配度。".to_string(),
        "score 范围 0 到 1，reason 简短说明选择原因。".to_string(),
        format!("用户点歌：{}", request),
        format!("候选列表：{}", serde_json::to_string(&candidates_json).unwrap_or_default()),
    ]
    .join("\n")
}

fn parse_candidate_pick_result(text: &str) -> Result<AiCandidatePickResult> {
    let value: Value = serde_json::from_str(text)?;
    Ok(AiCandidatePickResult {
        uri: json_string(&value, "uri"),
        reason: json_string(&value, "reason"),
        score: value.get("score").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

fn json_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn call_ai(
    config: &AiProviderConfig,
    prompt: &str,
    max_tokens: usize,
    timing: &TimingConfig,
) -> Result<String> {
    let body = json!({
        "model": config.model,
        "messages": [
            { "role": "system", "content": "你是点歌 JSON 结构化输出助手。必须只返回合法 JSON。" },
            { "role": "user", "content": [{ "type": "text", "text": prompt }] }
        ],
        "response_format": { "type": "json_object" },
        "temperature": 0.1,
        "stream": false,
        "max_completion_tokens": max_tokens,
        "top_p": 0.95,
        "thinking": { "type": "disabled" }
    })
    .to_string();
    call_ai_http(config, &body, timing)
}

fn call_ai_http(config: &AiProviderConfig, body: &str, timing: &TimingConfig) -> Result<String> {
    let response = Client::builder()
        .timeout(Duration::from_millis(timing.ai_request_timeout_ms))
        .build()
        .context("创建AI HTTP客户端失败")?
        .post(&config.endpoint)
        .headers(ai_headers(config)?)
        .body(body.to_string())
        .send()
        .with_context(|| format!("AI请求失败({:?})", config.provider))?;
    let status = response.status();
    let text = response.text().context("读取AI响应失败")?;
    if !status.is_success() {
        bail!(
            "AI请求失败({:?}) status={}: {}",
            config.provider,
            status,
            error_excerpt(&text)
        );
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("解析AI响应失败: {}", error_excerpt(&text)))?;
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("AI响应缺少choices[0].message.content"))
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

fn ai_headers(config: &AiProviderConfig) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    match config.provider {
        AiProvider::Mimo => {
            headers.insert(
                HeaderName::from_static("api-key"),
                HeaderValue::from_str(&config.api_key).context("api_key不是有效HTTP header")?,
            );
        }
        AiProvider::OpenAi | AiProvider::DeepSeek | AiProvider::Custom => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", config.api_key))
                    .context("api_key不是有效HTTP header")?,
            );
        }
    }
    Ok(headers)
}

fn model_reply_json_object(reply: &str) -> Result<String> {
    let trimmed = reply.trim();
    if serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object()) {
        return Ok(trimmed.to_string());
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("AI返回无效JSON"))?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("AI返回无效JSON"))?;
    let candidate = &trimmed[start..=end];
    if serde_json::from_str::<Value>(candidate).is_ok_and(|value| value.is_object()) {
        Ok(candidate.to_string())
    } else {
        bail!("AI返回无效JSON")
    }
}

fn validate_recognize_json(text: &str) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    for key in ["recognizedText", "songName", "songSinger", "searchText"] {
        if !value.get(key).is_some_and(Value::is_string) {
            bail!("AI返回JSON字段无效: {}", key);
        }
    }
    if !value
        .get("confidence")
        .and_then(Value::as_f64)
        .is_some_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
    {
        bail!("AI返回JSON字段无效: confidence");
    }
    Ok(())
}

fn validate_candidate_pick_json(text: &str, candidates: &[SearchCandidate]) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if uri.is_empty() || !candidates.iter().any(|candidate| candidate.uri == uri) {
        bail!("AI返回JSON字段无效: uri");
    }
    if !value
        .get("score")
        .and_then(Value::as_f64)
        .is_some_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
    {
        bail!("AI返回JSON字段无效: score");
    }
    if !value.get("reason").is_some_and(Value::is_string) {
        bail!("AI返回JSON字段无效: reason");
    }
    Ok(())
}

fn validate_match_json(text: &str) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    if !value.get("match").is_some_and(Value::is_boolean) {
        bail!("AI返回JSON字段无效: match");
    }
    if !matches!(
        value.get("decision").and_then(Value::as_str),
        Some("match" | "no_match")
    ) {
        bail!("AI返回JSON字段无效: decision");
    }
    if !value
        .get("score")
        .and_then(Value::as_f64)
        .is_some_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
    {
        bail!("AI返回JSON字段无效: score");
    }
    if !value.get("reason").is_some_and(Value::is_string) {
        bail!("AI返回JSON字段无效: reason");
    }
    Ok(())
}
