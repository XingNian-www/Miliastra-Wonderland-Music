use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::config::AiConfig;

const MIMO_ENDPOINT: &str = "https://api.xiaomimimo.com/v1/chat/completions";
const MIMO_MODEL: &str = "mimo-v2.5";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const OPENAI_MODEL: &str = "gpt-4o-mini";
const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";
const DEEPSEEK_MODEL: &str = "deepseek-chat";

#[derive(Clone)]
pub struct AiClient {
    config: AiConfig,
}

#[derive(Clone, Debug)]
pub struct AiMatchResult {
    pub matched: bool,
    pub reason: String,
    pub score: f64,
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
    pub fn new(config: &AiConfig) -> Self {
        Self {
            config: config.clone(),
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
}

pub fn recognize_with_query(config: &AiConfig, query: &[(String, String)]) -> Result<String> {
    let provider = resolve_provider_config(config, Some(query))?;
    let text = normalize_required(query_value(query, "text").unwrap_or(""), "text")?;
    let reply = call_ai(&provider, &build_recognize_prompt(&text), 1024)?;
    let json = model_reply_json_object(&reply)?;
    validate_recognize_json(&json)?;
    Ok(json)
}

pub fn match_with_query(config: &AiConfig, query: &[(String, String)]) -> Result<String> {
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
    )?;
    let json = model_reply_json_object(&reply)?;
    validate_match_json(&json)?;
    Ok(json)
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .rev()
        .find(|(item_key, _)| item_key == key)
        .map(|(_, value)| value.as_str())
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

fn call_ai(config: &AiProviderConfig, prompt: &str, max_tokens: usize) -> Result<String> {
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
    call_ai_powershell(config, &body)
}

fn call_ai_powershell(config: &AiProviderConfig, body: &str) -> Result<String> {
    let auth_header = match config.provider {
        AiProvider::Mimo => "api-key",
        AiProvider::OpenAi | AiProvider::DeepSeek | AiProvider::Custom => "Authorization",
    };
    let auth_value = match config.provider {
        AiProvider::Mimo => config.api_key.clone(),
        AiProvider::OpenAi | AiProvider::DeepSeek | AiProvider::Custom => {
            format!("Bearer {}", config.api_key)
        }
    };
    let payload = json!({
        "provider": format!("{:?}", config.provider),
        "authHeader": auth_header,
        "authValue": auth_value,
        "endpoint": config.endpoint,
        "body": body,
    })
    .to_string();
    let script = r#"
$data = [Console]::In.ReadToEnd() | ConvertFrom-Json
$headers = @{ 'Content-Type' = 'application/json' }
$headers[[string]$data.authHeader] = [string]$data.authValue
try {
  [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
  $response = Invoke-RestMethod -Method Post -Uri ([string]$data.endpoint) -Headers $headers -Body ([string]$data.body) -ContentType 'application/json' -TimeoutSec 30
  [string]$response.choices[0].message.content
} catch {
  $message = $_.Exception.Message
  if ($_.Exception.Response -and $_.Exception.Response.GetResponseStream()) {
    try {
      $reader = New-Object System.IO.StreamReader($_.Exception.Response.GetResponseStream())
      $body = $reader.ReadToEnd()
      if ($body) { $message = $message + ': ' + $body }
    } catch {}
  }
  Write-Error $message
  exit 1
}
"#;
    run_powershell(script, &payload, Duration::from_secs(35))
        .map_err(|error| anyhow::anyhow!("AI请求失败({:?}): {}", config.provider, error))
}

fn run_powershell(script: &str, input: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("PowerShell执行失败")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .context("PowerShell输入失败")?;
    }
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().context("PowerShell执行失败")?;
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
                }
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                bail!(if stderr.is_empty() {
                    "PowerShell执行失败".to_string()
                } else {
                    stderr
                });
            }
            Ok(None) => {
                if started_at.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("PowerShell执行超时");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("PowerShell执行失败"),
        }
    }
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
