use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::{FeelUOwnConfig, TimingConfig};

const VOLUME_CURVE_POWER: f64 = 0.5;
const VOLUME_SMOOTH_STEPS: i64 = 8;

#[derive(Clone, Debug)]
pub struct FeelUOwnClient {
    host: String,
    port: u16,
    timeout: Duration,
    volume_smooth_step_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayerStatus {
    pub status: String,
    pub current_uri: String,
    pub name: String,
    pub singer: String,
    pub album_name: String,
    pub lyric_line_text: String,
    pub duration: f64,
    pub progress: f64,
    pub playback_rate: f64,
    pub volume: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchCandidate {
    pub text: String,
    pub uri: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlaySearchResult {
    pub message: String,
    pub raw_search_result: String,
    pub candidate: Option<SearchCandidate>,
}

impl FeelUOwnClient {
    pub fn new(config: &FeelUOwnConfig, timing: &TimingConfig) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            timeout: Duration::from_millis(timing.feeluown_rpc_timeout_ms),
            volume_smooth_step_ms: timing.volume_smooth_step_ms,
        }
    }

    pub fn request(&self, command: &str) -> Result<String> {
        let mut stream = self.connect()?;
        self.send_command(&mut stream, command)?;
        let status_line = read_line(&mut stream).context("read FeelUOwn ACK")?;
        let (ok, body_len) = parse_ack(&status_line)?;
        let mut body = vec![0_u8; body_len];
        stream.read_exact(&mut body).context("read FeelUOwn body")?;
        let body = String::from_utf8_lossy(&body).to_string();
        if ok {
            Ok(body)
        } else if body.trim().is_empty() {
            bail!(status_line)
        } else {
            bail!(body)
        }
    }

    fn connect(&self) -> Result<TcpStream> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream =
            TcpStream::connect(&addr).with_context(|| format!("连接 FeelUOwn 失败: {}", addr))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .context("set FeelUOwn read timeout")?;
        stream
            .set_write_timeout(Some(self.timeout))
            .context("set FeelUOwn write timeout")?;

        let welcome = read_line(&mut stream).context("read FeelUOwn welcome")?;
        if !welcome.to_ascii_uppercase().starts_with("OK ") {
            bail!("FeelUOwn 连接失败: {}", welcome);
        }
        Ok(stream)
    }

    fn send_command(&self, stream: &mut TcpStream, command: &str) -> Result<()> {
        let command_text = if command.ends_with('\n') {
            command.to_string()
        } else {
            format!("{}\n", command)
        };
        stream
            .write_all(command_text.as_bytes())
            .context("send FeelUOwn command")?;
        Ok(())
    }

    pub fn play_uri(&self, uri: &str) -> Result<String> {
        if !uri.starts_with("fuo://") {
            bail!("只允许 fuo:// URI");
        }
        self.request(&format!("play {}", shell_quote(uri)))
    }

    pub fn exec(&self, code: &str) -> Result<String> {
        self.request(&format!("exec << EOF\n{}\nEOF\n", code))
    }

    pub fn status(&self) -> Result<PlayerStatus> {
        let json = parse_json_from_text(&self.exec(STATUS_SCRIPT)?)?;
        serde_json::from_str(&json).with_context(|| format!("parse FeelUOwn status: {}", json))
    }

    pub fn play(&self) -> Result<String> {
        self.request("resume")
    }

    pub fn pause(&self) -> Result<String> {
        self.request("pause")
    }

    pub fn next(&self) -> Result<String> {
        self.request("next")
    }

    pub fn previous(&self) -> Result<String> {
        self.request("previous")
    }

    pub fn set_volume(&self, volume: &str) -> Result<String> {
        if !is_valid_volume(volume) {
            bail!("volume 参数必须是 0-100");
        }
        let target = map_input_volume(volume.parse::<f64>().unwrap_or(0.0));
        self.exec(&volume_smooth_script(target, self.volume_smooth_step_ms))
    }

    pub fn search(&self, keyword: &str, source: &str) -> Result<String> {
        let command = search_command(keyword, source, None);
        self.request(&command)
    }

    fn search_json(&self, keyword: &str, source: &str) -> Result<String> {
        let command = search_command(keyword, source, Some("json"));
        self.request(&command)
    }

    pub fn search_candidates(&self, keyword: &str, source: &str) -> Result<Vec<SearchCandidate>> {
        match self
            .search_json(keyword, source)
            .and_then(|text| extract_search_candidates_from_json(&text))
        {
            Ok(candidates) if !candidates.is_empty() => Ok(candidates),
            Ok(_) | Err(_) => Ok(extract_search_candidates(&self.search(keyword, source)?)),
        }
    }

    pub fn play_keyword(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<PlaySearchResult> {
        let picked = self
            .search_and_pick(keyword, source, prefer_accompaniment)?
            .ok_or_else(|| anyhow!("平台无对应歌曲音源"))?;
        self.request(&format!("play {}", shell_quote(&picked.0.uri)))?;
        Ok(PlaySearchResult {
            message: format!(
                "正在搜索: {}{}",
                keyword,
                if prefer_accompaniment {
                    " (伴奏优先)"
                } else {
                    ""
                }
            ),
            raw_search_result: picked.1,
            candidate: Some(picked.0),
        })
    }

    pub fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<(SearchCandidate, String)>> {
        let has_accompaniment_word = is_accompaniment_text(keyword);
        let mut keywords = Vec::new();
        if prefer_accompaniment && !has_accompaniment_word {
            keywords.push(format!("{} 伴奏", keyword));
        }
        keywords.push(keyword.to_string());

        let mut fallback = None;
        for search_text in keywords {
            let candidates = self.search_candidates(&search_text, source)?;
            let result = format_search_candidates(&candidates);
            let chosen = pick_search_candidate(&candidates, prefer_accompaniment);
            if let Some(candidate) = chosen {
                if !prefer_accompaniment || is_accompaniment_text(&candidate.text) {
                    return Ok(Some((candidate, result)));
                }
                if fallback.is_none() {
                    fallback = Some((candidate, result));
                }
            }
        }
        Ok(fallback)
    }
}

fn format_search_candidates(candidates: &[SearchCandidate]) -> String {
    candidates
        .iter()
        .map(|candidate| format!("{}\t# {}", candidate.uri, candidate.text))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_status(status: &PlayerStatus) -> String {
    let title = song_title(&status.name, &status.singer);
    let progress = format_seconds(status.progress);
    let duration = format_seconds(status.duration);
    if title.is_empty() {
        format!(
            "状态: {} ({}/{}) 音量{}",
            status.status, progress, duration, status.volume
        )
    } else {
        format!(
            "状态: {} {} ({}/{}) 音量{}",
            status.status, title, progress, duration, status.volume
        )
    }
}

pub fn format_lyrics(status: &PlayerStatus) -> String {
    let text = status.lyric_line_text.trim();
    if text.is_empty() {
        "当前无歌词".to_string()
    } else {
        format!("歌词: {}", text)
    }
}

fn read_line(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&bytes)
        .trim_end_matches('\r')
        .to_string())
}

fn parse_ack(line: &str) -> Result<(bool, usize)> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 3 || !parts[0].eq_ignore_ascii_case("ACK") {
        bail!("无效的 FeelUOwn 响应: {}", line);
    }
    let ok = parts[1].eq_ignore_ascii_case("ok");
    let len = parts[2]
        .parse::<usize>()
        .context("parse FeelUOwn body length")?;
    Ok((ok, len))
}

fn parse_json_from_text(text: &str) -> Result<String> {
    let body = text.trim();
    if body.is_empty() {
        bail!("FeelUOwn 未返回状态数据");
    }
    if body.starts_with('{') && body.ends_with('}') {
        return Ok(body.to_string());
    }
    let start = body
        .rfind('{')
        .ok_or_else(|| anyhow!("无法解析 FeelUOwn 状态: {}", body))?;
    let end = body
        .rfind('}')
        .ok_or_else(|| anyhow!("无法解析 FeelUOwn 状态: {}", body))?;
    if end <= start {
        bail!("无法解析 FeelUOwn 状态: {}", body);
    }
    Ok(body[start..=end].to_string())
}

fn source_args(source: &str) -> String {
    source
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| format!("--source={}", shell_quote(item)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn search_command(keyword: &str, source: &str, format: Option<&str>) -> String {
    let mut parts = vec!["search".to_string(), shell_quote(keyword)];
    let sources = source_args(source);
    if !sources.is_empty() {
        parts.push(sources);
    }
    if let Some(format) = format {
        parts.push(format!("--format={}", shell_quote(format)));
    }
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn is_accompaniment_text(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower.contains("伴奏")
        || lower.contains("伴唱")
        || lower.contains("纯伴奏")
        || lower.contains("纯伴唱")
        || lower.contains("instrumental")
        || lower.contains("karaoke")
}

pub fn extract_search_candidates(text: &str) -> Vec<SearchCandidate> {
    let mut candidates = Vec::new();
    let mut block: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let uris = extract_fuo_uris(trimmed);
        if uris.is_empty() {
            block.push(trimmed.to_string());
        } else {
            let text_part = remove_fuo_uris(trimmed).trim().to_string();
            let candidate_text = [block.join(" "), text_part]
                .into_iter()
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            for uri in uris {
                candidates.push(SearchCandidate {
                    text: candidate_text.clone(),
                    uri,
                });
            }
            block.clear();
        }
    }
    if candidates.is_empty() {
        for uri in extract_fuo_uris(text) {
            candidates.push(SearchCandidate {
                text: String::new(),
                uri,
            });
        }
    }
    candidates
}

fn extract_search_candidates_from_json(text: &str) -> Result<Vec<SearchCandidate>> {
    let value: Value = serde_json::from_str(text).context("parse FeelUOwn search JSON")?;
    let mut candidates = Vec::new();
    collect_search_candidates_from_json(&value, &mut candidates);
    Ok(candidates)
}

fn collect_search_candidates_from_json(value: &Value, candidates: &mut Vec<SearchCandidate>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_search_candidates_from_json(item, candidates);
            }
        }
        Value::Object(map) => {
            if let Some(songs) = map.get("songs").and_then(Value::as_array) {
                for song in songs {
                    if let Some(candidate) = json_song_candidate(song) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        _ => {}
    }
}

fn json_song_candidate(value: &Value) -> Option<SearchCandidate> {
    let uri = json_string(value, "uri").or_else(|| {
        let source = json_string(value, "source").or_else(|| json_string(value, "provider"))?;
        let identifier = json_string(value, "identifier")?;
        Some(format!("fuo://{}/songs/{}", source, identifier))
    })?;

    Some(SearchCandidate {
        text: json_song_text(value).unwrap_or_else(|| uri.clone()),
        uri,
    })
}

fn json_song_text(value: &Value) -> Option<String> {
    let title = json_string(value, "title").or_else(|| json_string(value, "name"))?;
    let artists = json_string(value, "artists_name")
        .or_else(|| json_string(value, "artist_name"))
        .or_else(|| json_string(value, "singer"))
        .or_else(|| json_artist_names(value.get("artists")));
    Some(match artists {
        Some(artists) if !artists.is_empty() => format!("{} - {}", title, artists),
        _ => title,
    })
}

fn json_artist_names(value: Option<&Value>) -> Option<String> {
    let names = value?
        .as_array()?
        .iter()
        .filter_map(|artist| json_string(artist, "name").or_else(|| json_string(artist, "title")))
        .collect::<Vec<_>>();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn pick_search_candidate(
    candidates: &[SearchCandidate],
    prefer_accompaniment: bool,
) -> Option<SearchCandidate> {
    if candidates.is_empty() {
        return None;
    }
    if prefer_accompaniment {
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| is_accompaniment_text(&candidate.text))
        {
            return Some(candidate.clone());
        }
    } else if let Some(candidate) = candidates
        .iter()
        .find(|candidate| !is_accompaniment_text(&candidate.text))
    {
        return Some(candidate.clone());
    } else {
        return None;
    }
    candidates.first().cloned()
}

fn extract_fuo_uris(text: &str) -> Vec<String> {
    let mut uris = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("fuo://") {
        let after = &rest[start..];
        let end = after
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, _)| index)
            .unwrap_or(after.len());
        let uri = after[..end]
            .trim_end_matches(|ch: char| ch == ',' || ch == ';')
            .to_string();
        if !uri.is_empty() {
            uris.push(uri);
        }
        rest = &after[end..];
    }
    uris
}

fn remove_fuo_uris(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("fuo://") {
        output.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, _)| index)
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    output.push_str(rest);
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_valid_volume(value: &str) -> bool {
    if value == "100" {
        return true;
    }
    let bytes = value.as_bytes();
    match bytes.len() {
        1 => bytes[0].is_ascii_digit(),
        2 => bytes[0].is_ascii_digit() && bytes[0] != b'0' && bytes[1].is_ascii_digit(),
        _ => false,
    }
}

fn map_input_volume(volume: f64) -> i64 {
    let value = volume.clamp(0.0, 100.0);
    ((value / 100.0).powf(VOLUME_CURVE_POWER) * 100.0).round() as i64
}

fn format_seconds(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0:00".to_string();
    }
    let seconds = value.round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn song_title(name: &str, singer: &str) -> String {
    let name = name.trim();
    let singer = singer.trim();
    match (name.is_empty(), singer.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name.to_string(),
        (true, false) => singer.to_string(),
        (false, false) => format!("{} - {}", name, singer),
    }
}

fn volume_smooth_script(target: i64, step_ms: u64) -> String {
    format!(
        r#"import math, time

target = {target}
steps = {steps}
delay = {delay:.3}

try:
    current = float(getattr(app.player, 'volume', target))
except Exception:
    current = target

if math.isnan(current) or math.isinf(current):
    current = target

current = max(0, min(100, current))
target = max(0, min(100, target))

if steps <= 1 or abs(target - current) < 1:
    app.player.volume = int(round(target))
else:
    for index in range(1, steps + 1):
        value = current + (target - current) * index / steps
        app.player.volume = int(round(max(0, min(100, value))))
        if index < steps:
            time.sleep(delay)

print('OK')"#,
        target = target,
        steps = VOLUME_SMOOTH_STEPS,
        delay = step_ms as f64 / 1000.0
    )
}

const STATUS_SCRIPT: &str = r#"
import json, math

try:
    from feeluown.library import reverse
except Exception:
    reverse = None

VOLUME_CURVE_POWER = 0.5

player = app.player
playlist = app.playlist
song = getattr(playlist, 'current_song', None)
metadata = getattr(player, 'current_metadata', {}) or {}

def meta_get(key, default=''):
    try:
        return metadata.get(key, default)
    except Exception:
        return default

def text(value):
    if value is None:
        return ''
    if isinstance(value, (list, tuple)):
        return ', '.join(str(item) for item in value)
    return str(value)

def attr(obj, name, default=''):
    try:
        return getattr(obj, name, default) if obj is not None else default
    except Exception:
        return default

def model_uri(model):
    if model is None or reverse is None:
        return ''
    try:
        return reverse(model)
    except Exception:
        return ''

def number(value, default=0):
    try:
        if value is None:
            return default
        value = float(value)
        if math.isnan(value) or math.isinf(value):
            return default
        return value
    except Exception:
        return default

def display_volume(value):
    raw = max(0, min(100, number(value, 0)))
    if raw <= 0:
        return 0
    return int(round(math.pow(raw / 100, 1 / VOLUME_CURVE_POWER) * 100))

state = attr(attr(player, 'state', None), 'name', 'stopped')
duration = number(attr(player, 'duration', 0), 0)
if not duration and song is not None:
    raw_duration = attr(song, 'duration', 0)
    if raw_duration:
        duration = number(raw_duration, 0) / 1000

payload = {
    'status': state,
    'currentUri': text(model_uri(song) or meta_get('uri', '')),
    'name': text(attr(song, 'title', '') or meta_get('title', '')),
    'singer': text(attr(song, 'artists_name', '') or meta_get('artists', '') or meta_get('artist', '')),
    'albumName': text(attr(song, 'album_name', '') or meta_get('album', '')),
    'lyricLineText': text(attr(app.live_lyric, 'current_sentence', '')),
    'duration': duration,
    'progress': number(attr(player, 'position', 0), 0),
    'playbackRate': 1,
    'volume': display_volume(attr(player, 'volume', 0))
}
print(json.dumps(payload, ensure_ascii=False))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_args_uses_repeated_rpc_options() {
        assert_eq!(
            source_args("qqmusic,netease"),
            "--source='qqmusic' --source='netease'"
        );
    }

    #[test]
    fn search_command_can_request_json_format() {
        assert_eq!(
            search_command("晴天", "qqmusic,netease", Some("json")),
            "search '晴天' --source='qqmusic' --source='netease' --format='json'"
        );
    }

    #[test]
    fn extracts_plain_search_candidates() {
        let text = "fuo://qqmusic/songs/97773    \t# 晴天 - 周杰伦\n\
fuo://netease/songs/3334653818\t# 晴天 - 周杰伦";
        let candidates = extract_search_candidates(text);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].uri, "fuo://qqmusic/songs/97773");
        assert_eq!(candidates[0].text, "# 晴天 - 周杰伦");
        assert_eq!(candidates[1].uri, "fuo://netease/songs/3334653818");
    }

    #[test]
    fn extracts_full_song_candidates_from_search_json() {
        let text = r#"[
  {
    "songs": [
      {
        "identifier": "BV12sdZB2Ese",
        "source": "bilibili",
        "title": "“这世界其实就是如此荒诞又温柔的一场梦”完整版标题测试",
        "artists_name": "指尖灬旋律丿和另一个很长很长的歌手名",
        "uri": "fuo://bilibili/songs/BV12sdZB2Ese"
      }
    ]
  }
]"#;
        let candidates = extract_search_candidates_from_json(text).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].uri, "fuo://bilibili/songs/BV12sdZB2Ese");
        assert_eq!(
            candidates[0].text,
            "“这世界其实就是如此荒诞又温柔的一场梦”完整版标题测试 - 指尖灬旋律丿和另一个很长很长的歌手名"
        );
    }
}
