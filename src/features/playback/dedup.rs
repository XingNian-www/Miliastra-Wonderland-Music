use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::SongDedupConfig;
use crate::runtime::clock::WallClock;

#[derive(Clone, Debug)]
pub(crate) struct SongDedupCandidate {
    pub(super) uri: String,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) source: String,
    pub(super) prefer_accompaniment: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SongDedupEntry {
    uri: String,
    title: String,
    artist: String,
    source: String,
    prefer_accompaniment: bool,
    played_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SongDedupHistoryFile {
    entries: Vec<SongDedupEntry>,
}

pub(crate) struct PersistentSongDedupHistory {
    path: PathBuf,
    entries: Vec<SongDedupEntry>,
    clock: Arc<dyn WallClock>,
}

impl PersistentSongDedupHistory {
    pub(crate) fn load(path: PathBuf, clock: Arc<dyn WallClock>) -> Result<Self> {
        let entries = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("读取长时间同歌去重历史失败: {}", path.display()))?;
            parse_history_entries(&text)
                .with_context(|| format!("解析长时间同歌去重历史失败: {}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            entries,
            clock,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_limited(
        &self,
        config: &SongDedupConfig,
        candidate: &SongDedupCandidate,
    ) -> bool {
        if !config.enabled {
            return false;
        }
        let now = self.clock.unix_seconds();
        let window_start = now.saturating_sub(config.window_seconds);
        let count = self
            .entries
            .iter()
            .filter(|entry| entry.played_at >= window_start)
            .filter(|entry| same_song(candidate, entry))
            .count() as u32;
        count >= config.max_count
    }

    pub(crate) fn record_playback(
        &mut self,
        config: &SongDedupConfig,
        candidate: SongDedupCandidate,
    ) -> Result<()> {
        if !config.enabled {
            return Ok(());
        }
        let now = self.clock.unix_seconds();
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
    Ok(serde_json::from_str::<SongDedupHistoryFile>(text)?.entries)
}

fn same_song(candidate: &SongDedupCandidate, entry: &SongDedupEntry) -> bool {
    let candidate_uri = candidate.uri.trim();
    let entry_uri = entry.uri.trim();
    !candidate_uri.is_empty() && !entry_uri.is_empty() && candidate_uri == entry_uri
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    crate::adapters::file_store::write_atomic(path, text.as_bytes(), "长时间同歌去重历史")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::runtime::clock::{ManualClock, SystemClock};

    #[test]
    fn existing_empty_history_file_is_rejected_as_incomplete() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mwm-empty-dedup-{suffix}.json"));
        fs::write(&path, "").unwrap();

        let error = match PersistentSongDedupHistory::load(path.clone(), Arc::new(SystemClock)) {
            Ok(_) => panic!("an existing history file must use the current wrapped format"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("解析长时间同歌去重历史失败"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dedup_window_uses_the_injected_wall_clock() {
        let clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 1_000));
        let mut history = PersistentSongDedupHistory::load(PathBuf::new(), clock.clone()).unwrap();
        history.entries.push(SongDedupEntry {
            uri: "fuo://qqmusic/songs/1".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            played_at: 1_000,
        });
        let candidate = SongDedupCandidate {
            uri: "fuo://qqmusic/songs/1".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
        };
        let config = SongDedupConfig {
            window_seconds: 60,
            max_count: 1,
            ..SongDedupConfig::default()
        };

        assert!(history.is_limited(&config, &candidate));
        clock.advance(Duration::from_secs(61)).unwrap();
        assert!(!history.is_limited(&config, &candidate));
    }

    #[test]
    fn uri_match_is_limited() {
        let history = PersistentSongDedupHistory {
            path: PathBuf::new(),
            clock: Arc::new(SystemClock),
            entries: vec![SongDedupEntry {
                uri: "fuo://qqmusic/songs/1".to_string(),
                title: "晴天".to_string(),
                artist: "周杰伦".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: false,
                played_at: SystemClock.unix_seconds(),
            }],
        };
        let candidate = SongDedupCandidate {
            uri: "fuo://qqmusic/songs/1".to_string(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: false,
        };
        assert!(history.is_limited(&SongDedupConfig::default(), &candidate));
    }

    #[test]
    fn accompaniment_and_original_are_separate() {
        let history = PersistentSongDedupHistory {
            path: PathBuf::new(),
            clock: Arc::new(SystemClock),
            entries: vec![SongDedupEntry {
                uri: String::new(),
                title: "晴天".to_string(),
                artist: "周杰伦".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: true,
                played_at: SystemClock.unix_seconds(),
            }],
        };
        let candidate = SongDedupCandidate {
            uri: String::new(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: false,
        };
        assert!(!history.is_limited(&SongDedupConfig::default(), &candidate));
    }

    #[test]
    fn missing_uri_never_falls_back_to_title_or_artist() {
        let history = PersistentSongDedupHistory {
            path: PathBuf::new(),
            clock: Arc::new(SystemClock),
            entries: vec![SongDedupEntry {
                uri: String::new(),
                title: "晴天".to_string(),
                artist: "周杰伦".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: false,
                played_at: SystemClock.unix_seconds(),
            }],
        };
        let candidate = SongDedupCandidate {
            uri: String::new(),
            title: "晴天".to_string(),
            artist: "周杰伦".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
        };
        assert!(!history.is_limited(&SongDedupConfig::default(), &candidate));
    }
}
