use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{FeelUOwnConfig, TimingConfig};
use crate::runtime::player::{RawPlayerSample, TransportState};
use crate::runtime::player_io::{
    ControlDispatchOutcome, PickedCandidate as RuntimePickedCandidate, PlayerControl,
    PlayerControlPort, PlayerObservationPort, PlayerObservationReadError, PlayerSearchError,
    PlayerSearchPort, SearchCandidate as RuntimeSearchCandidate,
};

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

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawPlayerStatus {
    status: Option<String>,
    current_uri: Option<String>,
    name: Option<String>,
    singer: Option<String>,
    album_name: Option<String>,
    lyric_line_text: Option<String>,
    duration: Option<f64>,
    progress: Option<f64>,
    playback_rate: Option<f64>,
    volume: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchCandidate {
    pub text: String,
    pub uri: String,
}

impl From<RawPlayerStatus> for RawPlayerSample {
    fn from(status: RawPlayerStatus) -> Self {
        Self {
            uri: status.current_uri.and_then(nonempty_text),
            transport: status.status.as_deref().and_then(parse_transport_state),
            title: status.name.and_then(nonempty_text),
            artist: status.singer.and_then(nonempty_text),
            album_name: status.album_name.and_then(nonempty_text),
            lyric_line_text: status.lyric_line_text.map(|line| line.trim().to_string()),
            progress: status.progress.and_then(nonnegative_duration),
            duration: status.duration.and_then(nonnegative_duration),
            playback_rate: status
                .playback_rate
                .filter(|rate| rate.is_finite() && *rate > 0.0),
            volume: status.volume.filter(|volume| (0..=100).contains(volume)),
        }
    }
}

impl From<RawPlayerStatus> for PlayerStatus {
    fn from(status: RawPlayerStatus) -> Self {
        Self {
            status: status.status.unwrap_or_else(|| "stopped".to_string()),
            current_uri: status.current_uri.unwrap_or_default(),
            name: status.name.unwrap_or_default(),
            singer: status.singer.unwrap_or_default(),
            album_name: status.album_name.unwrap_or_default(),
            lyric_line_text: status.lyric_line_text.unwrap_or_default(),
            duration: status.duration.unwrap_or(0.0),
            progress: status.progress.unwrap_or(0.0),
            playback_rate: status.playback_rate.unwrap_or(1.0),
            volume: status.volume.unwrap_or(0),
        }
    }
}

fn nonempty_text(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_transport_state(value: &str) -> Option<TransportState> {
    match value.trim().to_ascii_lowercase().as_str() {
        "playing" => Some(TransportState::Playing),
        "paused" => Some(TransportState::Paused),
        "stopped" | "stoped" => Some(TransportState::Stopped),
        _ => None,
    }
}

fn nonnegative_duration(value: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(value).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckCode {
    Ok,
    Oops,
}

struct RpcAcknowledgement {
    code: AckCode,
    status_line: String,
    body: Vec<u8>,
    body_error: Option<anyhow::Error>,
}

impl RpcAcknowledgement {
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    fn control_message(&self) -> String {
        let body = self.body_text();
        if body.trim().is_empty() {
            self.status_line.clone()
        } else {
            body
        }
    }

    fn into_legacy_result(self) -> Result<String> {
        if let Some(error) = self.body_error {
            return Err(error);
        }
        let body = self.body_text();
        match self.code {
            AckCode::Ok => Ok(body),
            AckCode::Oops if body.trim().is_empty() => Err(anyhow!(self.status_line)),
            AckCode::Oops => Err(anyhow!(body)),
        }
    }
}

enum RpcRequestOutcome {
    Acknowledgement(RpcAcknowledgement),
    NotSent(anyhow::Error),
    OutcomeUnknown(anyhow::Error),
}

impl FeelUOwnClient {
    pub fn new(config: &FeelUOwnConfig, timing: &TimingConfig) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            timeout: Duration::from_millis(timing.external.feeluown_rpc_timeout_ms),
            volume_smooth_step_ms: timing.external.volume_smooth_step_ms,
        }
    }

    pub fn request(&self, command: &str) -> Result<String> {
        match self.request_once(command) {
            RpcRequestOutcome::Acknowledgement(acknowledgement) => {
                acknowledgement.into_legacy_result()
            }
            RpcRequestOutcome::NotSent(error) | RpcRequestOutcome::OutcomeUnknown(error) => {
                Err(error)
            }
        }
    }

    fn request_once(&self, command: &str) -> RpcRequestOutcome {
        let mut stream = match self.connect() {
            Ok(stream) => stream,
            Err(error) => return RpcRequestOutcome::NotSent(error),
        };
        if let Err(error) = self.send_command(&mut stream, command) {
            return RpcRequestOutcome::OutcomeUnknown(error);
        }
        let status_line = match read_line(&mut stream).context("read FeelUOwn ACK") {
            Ok(status_line) => status_line,
            Err(error) => return RpcRequestOutcome::OutcomeUnknown(error),
        };
        let (code, body_len) = match parse_ack(&status_line) {
            Ok(ack) => ack,
            Err(error) => return RpcRequestOutcome::OutcomeUnknown(error),
        };
        let (body, body_error) = read_rpc_body(&mut stream, body_len);
        RpcRequestOutcome::Acknowledgement(RpcAcknowledgement {
            code,
            status_line,
            body,
            body_error,
        })
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
        self.raw_status().map(PlayerStatus::from)
    }

    fn raw_status(&self) -> Result<RawPlayerStatus> {
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

impl PlayerControlPort for FeelUOwnClient {
    fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatchOutcome {
        if matches!(control, PlayerControl::PlayUri(uri) if !uri.starts_with("fuo://")) {
            return ControlDispatchOutcome::not_sent("只允许 fuo:// URI");
        }
        if matches!(control, PlayerControl::SetVolume(volume) if *volume > 100) {
            return ControlDispatchOutcome::not_sent("volume 参数必须是 0-100");
        }

        let command = match control {
            PlayerControl::PlayUri(uri) => format!("play {}", shell_quote(uri)),
            PlayerControl::Pause => "pause".to_string(),
            PlayerControl::Resume => "resume".to_string(),
            PlayerControl::Next => "next".to_string(),
            PlayerControl::Previous => "previous".to_string(),
            PlayerControl::SetVolume(volume) => {
                let target = map_input_volume(f64::from(*volume));
                format!(
                    "exec << EOF\n{}\nEOF\n",
                    volume_smooth_script(target, self.volume_smooth_step_ms)
                )
            }
        };
        self.dispatch_control_command(&command)
    }
}

impl PlayerObservationPort for FeelUOwnClient {
    fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
        self.raw_status()
            .map(RawPlayerSample::from)
            .map_err(|error| PlayerObservationReadError::new(error.to_string()))
    }
}

impl PlayerSearchPort for FeelUOwnClient {
    fn search_text(&mut self, keyword: &str, source: &str) -> Result<String, PlayerSearchError> {
        self.search(keyword, source)
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }

    fn search_candidates(
        &mut self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<RuntimeSearchCandidate>, PlayerSearchError> {
        FeelUOwnClient::search_candidates(self, keyword, source)
            .map(|candidates| {
                candidates
                    .into_iter()
                    .map(|candidate| RuntimeSearchCandidate::new(candidate.text, candidate.uri))
                    .collect()
            })
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }

    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<RuntimePickedCandidate>, PlayerSearchError> {
        FeelUOwnClient::search_and_pick(self, keyword, source, prefer_accompaniment)
            .map(|picked| {
                picked.map(|(candidate, formatted_candidates)| {
                    RuntimePickedCandidate::new(
                        RuntimeSearchCandidate::new(candidate.text, candidate.uri),
                        formatted_candidates,
                    )
                })
            })
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }
}

impl FeelUOwnClient {
    fn dispatch_control_command(&self, command: &str) -> ControlDispatchOutcome {
        match self.request_once(command) {
            RpcRequestOutcome::Acknowledgement(acknowledgement) => {
                let message = acknowledgement.control_message();
                match acknowledgement.code {
                    AckCode::Ok => ControlDispatchOutcome::acknowledged(message),
                    AckCode::Oops => ControlDispatchOutcome::rejected(message),
                }
            }
            RpcRequestOutcome::NotSent(error) => {
                ControlDispatchOutcome::not_sent(error.to_string())
            }
            RpcRequestOutcome::OutcomeUnknown(error) => {
                ControlDispatchOutcome::outcome_unknown(error.to_string())
            }
        }
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

fn read_rpc_body(stream: &mut TcpStream, expected_len: usize) -> (Vec<u8>, Option<anyhow::Error>) {
    let mut body = Vec::with_capacity(expected_len);
    while body.len() < expected_len {
        let remaining = expected_len - body.len();
        let mut chunk = [0_u8; 8 * 1024];
        let chunk_len = remaining.min(chunk.len());
        match stream.read(&mut chunk[..chunk_len]) {
            Ok(0) => {
                let source = std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "FeelUOwn body ended after {} of {} bytes",
                        body.len(),
                        expected_len
                    ),
                );
                return (
                    body,
                    Some(anyhow::Error::new(source).context("read FeelUOwn body")),
                );
            }
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(error) => {
                return (
                    body,
                    Some(anyhow::Error::new(error).context("read FeelUOwn body")),
                );
            }
        }
    }
    (body, None)
}

fn parse_ack(line: &str) -> Result<(AckCode, usize)> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    let [protocol, code, body_len] = parts.as_slice() else {
        bail!("无效的 FeelUOwn 响应: {}", line);
    };
    if *protocol != "ACK" {
        bail!("无效的 FeelUOwn 响应: {}", line);
    }
    let code = match *code {
        "OK" => AckCode::Ok,
        "Oops" => AckCode::Oops,
        _ => bail!("无效的 FeelUOwn ACK 状态: {}", line),
    };
    let len = body_len
        .parse::<usize>()
        .context("parse FeelUOwn body length")?;
    Ok((code, len))
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
        let uri = after[..end].trim_end_matches([',', ';']).to_string();
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

def attr(obj, name):
    try:
        return getattr(obj, name) if obj is not None else None
    except Exception:
        return None

def meta_get(metadata, key):
    try:
        return metadata.get(key) if metadata is not None else None
    except Exception:
        return None

def text(value):
    if value is None:
        return None
    try:
        if isinstance(value, (list, tuple)):
            return ', '.join(str(item) for item in value)
        return str(value)
    except Exception:
        return None

def model_uri(model):
    if model is None or reverse is None:
        return None
    try:
        return reverse(model)
    except Exception:
        return None

def number(value):
    try:
        if value is None:
            return None
        value = float(value)
        if math.isnan(value) or math.isinf(value):
            return None
        return value
    except Exception:
        return None

def display_volume(value):
    raw = number(value)
    if raw is None:
        return None
    raw = max(0, min(100, raw))
    if raw <= 0:
        return 0
    return int(round(math.pow(raw / 100, 1 / VOLUME_CURVE_POWER) * 100))

player = attr(app, 'player')
playlist = attr(app, 'playlist')
song = attr(playlist, 'current_song')
metadata = attr(player, 'current_metadata')

state = attr(attr(player, 'state'), 'name')
duration = number(attr(player, 'duration'))
if duration is None or duration == 0:
    raw_duration = number(attr(song, 'duration'))
    if raw_duration is not None and raw_duration > 0:
        duration = raw_duration / 1000

payload = {
    'status': state,
    'currentUri': text(model_uri(song) or meta_get(metadata, 'uri')),
    'name': text(attr(song, 'title') or meta_get(metadata, 'title')),
    'singer': text(attr(song, 'artists_name') or meta_get(metadata, 'artists') or meta_get(metadata, 'artist')),
    'albumName': text(attr(song, 'album_name') or meta_get(metadata, 'album')),
    'lyricLineText': text(attr(attr(app, 'live_lyric'), 'current_sentence')),
    'duration': duration,
    'progress': number(attr(player, 'position')),
    'playbackRate': number(attr(player, 'playback_rate')),
    'volume': display_volume(attr(player, 'volume'))
}
print(json.dumps(payload, ensure_ascii=False))
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use crate::runtime::player::{RawPlayerSample, TransportState};
    use crate::runtime::player_io::{
        ControlDispatchOutcome, PlayerControl, PlayerControlPort, PlayerObservationPort,
        PlayerSearchPort,
    };

    fn test_client(port: u16) -> FeelUOwnClient {
        FeelUOwnClient {
            host: "127.0.0.1".to_string(),
            port,
            timeout: Duration::from_millis(500),
            volume_smooth_step_ms: 0,
        }
    }

    fn spawn_rpc_server(response: Vec<u8>) -> (u16, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake FeelUOwn RPC");
        let port = listener.local_addr().expect("fake RPC address").port();
        let (command_tx, command_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept fake RPC client");
            stream
                .write_all(b"OK FeelUOwn RPC ready\n")
                .expect("write fake RPC welcome");
            let command = read_line(&mut stream).expect("read fake RPC command");
            if command == "exec << EOF" {
                loop {
                    let line = read_line(&mut stream).expect("read fake RPC exec body");
                    if line == "EOF" {
                        break;
                    }
                }
            }
            command_tx.send(command).expect("record fake RPC command");
            stream
                .write_all(&response)
                .expect("write fake RPC response");
        });
        (port, command_rx, handle)
    }

    #[test]
    fn control_validation_failure_is_not_sent() {
        let mut client = test_client(1);

        let uri_outcome = PlayerControlPort::dispatch(
            &mut client,
            &PlayerControl::PlayUri("https://example.invalid/song".to_string()),
        );
        let volume_outcome =
            PlayerControlPort::dispatch(&mut client, &PlayerControl::SetVolume(101));

        assert!(matches!(
            uri_outcome,
            ControlDispatchOutcome::NotSent { .. }
        ));
        assert!(matches!(
            volume_outcome,
            ControlDispatchOutcome::NotSent { .. }
        ));
    }

    #[test]
    fn connection_and_welcome_failures_are_not_sent() {
        let unused_listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve unused RPC port");
        let unused_port = unused_listener.local_addr().unwrap().port();
        drop(unused_listener);
        let mut disconnected_client = test_client(unused_port);

        let disconnected =
            PlayerControlPort::dispatch(&mut disconnected_client, &PlayerControl::Pause);

        assert!(matches!(
            disconnected,
            ControlDispatchOutcome::NotSent { .. }
        ));

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind invalid welcome RPC");
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"ERROR not ready\n").unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).unwrap();
            assert!(
                received.is_empty(),
                "command must not precede a valid welcome"
            );
        });
        let mut invalid_welcome_client = test_client(port);

        let invalid_welcome =
            PlayerControlPort::dispatch(&mut invalid_welcome_client, &PlayerControl::Pause);

        assert!(matches!(
            invalid_welcome,
            ControlDispatchOutcome::NotSent { .. }
        ));
        server.join().unwrap();
    }

    #[test]
    fn positive_control_ack_is_acknowledged() {
        let body = "paused";
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Pause);

        assert_eq!(outcome, ControlDispatchOutcome::acknowledged("paused"));
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn negative_control_ack_is_rejected() {
        let body = "permission denied";
        let response = format!("ACK Oops {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Pause);

        assert_eq!(
            outcome,
            ControlDispatchOutcome::rejected("permission denied")
        );
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn disconnect_after_control_write_has_unknown_outcome() {
        let (port, command_rx, server) = spawn_rpc_server(Vec::new());
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Pause);

        assert!(matches!(
            outcome,
            ControlDispatchOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn malformed_control_response_has_unknown_outcome() {
        let (port, command_rx, server) = spawn_rpc_server(b"this is not an ACK\n".to_vec());
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Next);

        assert!(matches!(
            outcome,
            ControlDispatchOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(command_rx.recv().unwrap(), "next");
        server.join().unwrap();
    }

    #[test]
    fn unknown_control_ack_code_has_unknown_outcome() {
        let (port, command_rx, server) = spawn_rpc_server(b"ACK banana 0\n".to_vec());
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Next);

        assert!(matches!(
            outcome,
            ControlDispatchOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(command_rx.recv().unwrap(), "next");
        server.join().unwrap();
    }

    #[test]
    fn positive_control_ack_header_determines_outcome_when_body_is_truncated() {
        let (port, command_rx, server) = spawn_rpc_server(b"ACK OK 12\npartial".to_vec());
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Pause);

        assert_eq!(outcome, ControlDispatchOutcome::acknowledged("partial"));
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn negative_control_ack_header_determines_outcome_when_body_is_truncated() {
        let (port, command_rx, server) = spawn_rpc_server(b"ACK Oops 12\ndenied".to_vec());
        let mut client = test_client(port);

        let outcome = PlayerControlPort::dispatch(&mut client, &PlayerControl::Pause);

        assert_eq!(outcome, ControlDispatchOutcome::rejected("denied"));
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn legacy_request_rejects_a_truncated_positive_body() {
        let (port, command_rx, server) = spawn_rpc_server(b"ACK OK 12\npartial".to_vec());
        let client = test_client(port);

        let result = client.request("pause");

        assert!(result.is_err());
        assert_eq!(command_rx.recv().unwrap(), "pause");
        server.join().unwrap();
    }

    #[test]
    fn every_supported_control_is_dispatched_once() {
        let cases = [
            (
                PlayerControl::PlayUri("fuo://netease/songs/123".to_string()),
                "play 'fuo://netease/songs/123'",
            ),
            (PlayerControl::Pause, "pause"),
            (PlayerControl::Resume, "resume"),
            (PlayerControl::Next, "next"),
            (PlayerControl::Previous, "previous"),
            (PlayerControl::SetVolume(50), "exec << EOF"),
        ];

        for (control, expected_command) in cases {
            let (port, command_rx, server) = spawn_rpc_server(b"ACK OK 2\nOK".to_vec());
            let mut client = test_client(port);

            let outcome = PlayerControlPort::dispatch(&mut client, &control);

            assert_eq!(outcome, ControlDispatchOutcome::acknowledged("OK"));
            assert_eq!(command_rx.recv().unwrap(), expected_command);
            server.join().unwrap();
        }
    }

    #[test]
    fn observation_port_returns_a_typed_raw_sample() {
        let body = r#"{"status":"PLAYING","currentUri":" fuo://netease/songs/123 ","name":" Song ","singer":" Artist ","albumName":" Album ","lyricLineText":" lyric line ","duration":123.5,"progress":5.25,"playbackRate":1.25,"volume":80}"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let sample = PlayerObservationPort::read_sample(&mut client).unwrap();

        assert_eq!(
            sample,
            RawPlayerSample {
                uri: Some("fuo://netease/songs/123".to_string()),
                transport: Some(TransportState::Playing),
                title: Some("Song".to_string()),
                artist: Some("Artist".to_string()),
                album_name: Some("Album".to_string()),
                lyric_line_text: Some("lyric line".to_string()),
                progress: Some(Duration::from_secs_f64(5.25)),
                duration: Some(Duration::from_secs_f64(123.5)),
                playback_rate: Some(1.25),
                volume: Some(80),
            }
        );
        assert_eq!(command_rx.recv().unwrap(), "exec << EOF");
        server.join().unwrap();
    }

    #[test]
    fn observation_port_preserves_null_status_fields_as_unknown() {
        let body = r#"{"status":"playing","currentUri":"fuo://netease/songs/123","name":null,"singer":null,"albumName":null,"lyricLineText":"","duration":null,"progress":null,"playbackRate":null,"volume":null}"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let sample = PlayerObservationPort::read_sample(&mut client).unwrap();

        assert_eq!(sample.uri.as_deref(), Some("fuo://netease/songs/123"));
        assert_eq!(sample.transport, Some(TransportState::Playing));
        assert_eq!(sample.title, None);
        assert_eq!(sample.artist, None);
        assert_eq!(sample.album_name, None);
        assert_eq!(sample.lyric_line_text.as_deref(), Some(""));
        assert_eq!(sample.duration, None);
        assert_eq!(sample.progress, None);
        assert_eq!(sample.playback_rate, None);
        assert_eq!(sample.volume, None);
        assert_eq!(command_rx.recv().unwrap(), "exec << EOF");
        server.join().unwrap();
    }

    #[test]
    fn observation_port_preserves_missing_status_fields_as_unknown() {
        let body = r#"{"currentUri":"fuo://netease/songs/123"}"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let sample = PlayerObservationPort::read_sample(&mut client).unwrap();

        assert_eq!(sample.uri.as_deref(), Some("fuo://netease/songs/123"));
        assert_eq!(sample.transport, None);
        assert_eq!(sample.title, None);
        assert_eq!(sample.progress, None);
        assert_eq!(sample.duration, None);
        assert_eq!(sample.playback_rate, None);
        assert_eq!(sample.volume, None);
        assert_eq!(command_rx.recv().unwrap(), "exec << EOF");
        server.join().unwrap();
    }

    #[test]
    fn observation_getter_failures_do_not_become_stopped_or_zero() {
        let body = r#"{"status":null,"currentUri":null,"progress":null}"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let sample = PlayerObservationPort::read_sample(&mut client).unwrap();

        assert_eq!(sample.transport, None);
        assert_eq!(sample.uri, None);
        assert_eq!(sample.progress, None);
        assert_eq!(command_rx.recv().unwrap(), "exec << EOF");
        server.join().unwrap();
    }

    #[test]
    fn legacy_status_projects_missing_raw_fields_to_compatible_defaults() {
        let body = "{}";
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let client = test_client(port);

        let status = client.status().unwrap();

        assert_eq!(status.status, "stopped");
        assert_eq!(status.current_uri, "");
        assert_eq!(status.name, "");
        assert_eq!(status.singer, "");
        assert_eq!(status.album_name, "");
        assert_eq!(status.lyric_line_text, "");
        assert_eq!(status.duration, 0.0);
        assert_eq!(status.progress, 0.0);
        assert_eq!(status.playback_rate, 1.0);
        assert_eq!(status.volume, 0);
        assert_eq!(command_rx.recv().unwrap(), "exec << EOF");
        server.join().unwrap();
    }

    #[test]
    fn raw_sample_rejects_invalid_numeric_values() {
        let sample = RawPlayerSample::from(RawPlayerStatus {
            duration: Some(-1.0),
            progress: Some(f64::NAN),
            playback_rate: Some(f64::INFINITY),
            volume: Some(-1),
            ..RawPlayerStatus::default()
        });

        assert_eq!(sample.duration, None);
        assert_eq!(sample.progress, None);
        assert_eq!(sample.playback_rate, None);
        assert_eq!(sample.volume, None);

        let overflow = RawPlayerSample::from(RawPlayerStatus {
            duration: Some(f64::MAX),
            progress: Some(f64::NEG_INFINITY),
            playback_rate: Some(0.0),
            volume: Some(101),
            ..RawPlayerStatus::default()
        });

        assert_eq!(overflow.duration, None);
        assert_eq!(overflow.progress, None);
        assert_eq!(overflow.playback_rate, None);
        assert_eq!(overflow.volume, None);
    }

    #[test]
    fn raw_sample_accepts_legacy_stopped_and_rejects_unknown_transport() {
        let legacy = RawPlayerSample::from(RawPlayerStatus {
            status: Some(" stoped ".to_string()),
            ..RawPlayerStatus::default()
        });
        let unknown = RawPlayerSample::from(RawPlayerStatus {
            status: Some("buffering".to_string()),
            ..RawPlayerStatus::default()
        });

        assert_eq!(legacy.transport, Some(TransportState::Stopped));
        assert_eq!(unknown.transport, None);
    }

    #[test]
    fn search_port_preserves_raw_search_text() {
        let body = "raw FeelUOwn search output\nsecond line";
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let result = PlayerSearchPort::search_text(&mut client, "晴天", "netease").unwrap();

        assert_eq!(result, body);
        assert_eq!(
            command_rx.recv().unwrap(),
            "search '晴天' --source='netease'"
        );
        server.join().unwrap();
    }

    #[test]
    fn search_port_maps_structured_candidates() {
        let body = r#"[{"songs":[{"uri":"fuo://netease/songs/123","title":"晴天","artists_name":"周杰伦"}]}]"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let candidates =
            PlayerSearchPort::search_candidates(&mut client, "晴天", "netease").unwrap();

        assert_eq!(
            candidates,
            vec![crate::runtime::player_io::SearchCandidate::new(
                "晴天 - 周杰伦",
                "fuo://netease/songs/123"
            )]
        );
        assert_eq!(
            command_rx.recv().unwrap(),
            "search '晴天' --source='netease' --format='json'"
        );
        server.join().unwrap();
    }

    #[test]
    fn search_port_maps_picked_candidate_and_formatted_list() {
        let body = r#"[{"songs":[{"uri":"fuo://netease/songs/123","title":"晴天","artists_name":"周杰伦"}]}]"#;
        let response = format!("ACK OK {}\n{}", body.len(), body).into_bytes();
        let (port, command_rx, server) = spawn_rpc_server(response);
        let mut client = test_client(port);

        let picked =
            PlayerSearchPort::search_and_pick(&mut client, "晴天", "netease", false).unwrap();

        assert_eq!(
            picked,
            Some(crate::runtime::player_io::PickedCandidate::new(
                crate::runtime::player_io::SearchCandidate::new(
                    "晴天 - 周杰伦",
                    "fuo://netease/songs/123"
                ),
                "fuo://netease/songs/123\t# 晴天 - 周杰伦"
            ))
        );
        assert_eq!(
            command_rx.recv().unwrap(),
            "search '晴天' --source='netease' --format='json'"
        );
        server.join().unwrap();
    }

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
