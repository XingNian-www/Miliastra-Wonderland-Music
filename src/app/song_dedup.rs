use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::song_matcher;
use crate::config::{MatchConfig, SongDedupConfig};

#[derive(Clone, Debug)]
pub(super) struct SongDedupCandidate {
    pub(super) uri: String,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) source: String,
    pub(super) prefer_accompaniment: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SongDedupEntry {
    uri: String,
    title: String,
    artist: String,
    source: String,
    prefer_accompaniment: bool,
    played_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SongDedupHistoryFile {
    entries: Vec<SongDedupEntry>,
}

#[derive(Debug)]
pub(super) struct PersistentSongDedupHistory {
    path: PathBuf,
    entries: Vec<SongDedupEntry>,
}

impl PersistentSongDedupHistory {
    pub(super) fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("读取长时间同歌去重历史失败: {}", path.display()))?;
            parse_history_entries(&text)
                .with_context(|| format!("解析长时间同歌去重历史失败: {}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Self { path, entries })
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn is_limited(
        &self,
        config: &SongDedupConfig,
        matching: &MatchConfig,
        candidate: &SongDedupCandidate,
    ) -> bool {
        if !config.enabled {
            return false;
        }
        let now = current_unix_seconds();
        let window_start = now.saturating_sub(config.window_seconds);
        let count = self
            .entries
            .iter()
            .filter(|entry| entry.played_at >= window_start)
            .filter(|entry| same_song(matching, candidate, entry))
            .count() as u32;
        count >= config.max_count
    }

    pub(super) fn record_playback(
        &mut self,
        config: &SongDedupConfig,
        candidate: SongDedupCandidate,
    ) -> Result<()> {
        if !config.enabled {
            return Ok(());
        }
        let now = current_unix_seconds();
        let window_start = now.saturating_sub(config.window_seconds);
        self.entries.retain(|entry| entry.played_at >= window_start);
        self.entries.push(SongDedupEntry {
            uri: candidate.uri,
            title: candidate.title,
            artist: candidate.artist,
            source: candidate.source,
            prefer_accompaniment: candidate.prefer_accompaniment,
            played_at: now,
        });
        self.save()
    }

    fn save(&self) -> Result<()> {
        let file = SongDedupHistoryFile {
            entries: self.entries.clone(),
        };
        let text = serde_json::to_string_pretty(&file)?;
        write_atomic(&self.path, &text)
    }
}

fn parse_history_entries(text: &str) -> Result<Vec<SongDedupEntry>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(file) = serde_json::from_str::<SongDedupHistoryFile>(text) {
        return Ok(file.entries);
    }
    Ok(serde_json::from_str::<Vec<SongDedupEntry>>(text)?)
}

fn same_song(
    matching: &MatchConfig,
    candidate: &SongDedupCandidate,
    entry: &SongDedupEntry,
) -> bool {
    let candidate_uri = candidate.uri.trim();
    let entry_uri = entry.uri.trim();
    if !candidate_uri.is_empty() && !entry_uri.is_empty() && candidate_uri == entry_uri {
        return true;
    }
    if candidate.prefer_accompaniment != entry.prefer_accompaniment {
        return false;
    }
    let candidate_title = candidate.title.trim();
    let entry_title = entry.title.trim();
    if candidate_title.is_empty() || entry_title.is_empty() {
        return false;
    }
    let candidate_artist = candidate.artist.trim();
    let entry_artist = entry.artist.trim();
    if candidate_artist.is_empty() || entry_artist.is_empty() {
        return normalize(candidate_title) == normalize(entry_title)
            && candidate_artist.is_empty()
            && entry_artist.is_empty();
    }
    let candidate_query = format!("{candidate_title} {candidate_artist}");
    let entry_query = format!("{entry_title} {entry_artist}");
    song_matcher::match_song_query(
        matching,
        &candidate_query,
        entry_title,
        entry_artist,
        candidate.prefer_accompaniment,
    )
    .ok && song_matcher::match_song_query(
        matching,
        &entry_query,
        candidate_title,
        candidate_artist,
        candidate.prefer_accompaniment,
    )
    .ok
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if is_cjk(ch) {
                Some(ch)
            } else {
                None
            }
        })
        .collect()
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    ensure_parent(path)?;
    let temporary = temporary_path(path);
    {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("创建长时间同歌去重临时文件失败: {}", temporary.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("写入长时间同歌去重临时文件失败: {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("同步长时间同歌去重临时文件失败: {}", temporary.display()))?;
    }
    replace_file(&temporary, path)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "song-dedup-history".into(), |name| name.to_os_string());
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| "替换长时间同歌去重历史文件失败")
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    fs::rename(temporary, target).with_context(|| {
        format!(
            "替换长时间同歌去重历史失败: {} -> {}",
            temporary.display(),
            target.display()
        )
    })
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建长时间同歌去重历史目录失败: {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matching_config() -> MatchConfig {
        MatchConfig::default()
    }

    #[test]
    fn uri_match_is_limited() {
        let history = PersistentSongDedupHistory {
            path: PathBuf::new(),
            entries: vec![SongDedupEntry {
                uri: "fuo://qqmusic/songs/1".to_string(),
                title: "晴天".to_string(),
                artist: "周杰伦".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: false,
                played_at: current_unix_seconds(),
            }],
        };
        let candidate = SongDedupCandidate {
            uri: "fuo://qqmusic/songs/1".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: false,
        };
        assert!(history.is_limited(&SongDedupConfig::default(), &matching_config(), &candidate));
    }

    #[test]
    fn accompaniment_and_original_are_separate() {
        let history = PersistentSongDedupHistory {
            path: PathBuf::new(),
            entries: vec![SongDedupEntry {
                uri: String::new(),
                title: "晴天".to_string(),
                artist: "周杰伦".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: true,
                played_at: current_unix_seconds(),
            }],
        };
        let candidate = SongDedupCandidate {
            uri: String::new(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: false,
        };
        assert!(!history.is_limited(&SongDedupConfig::default(), &matching_config(), &candidate));
    }
}
