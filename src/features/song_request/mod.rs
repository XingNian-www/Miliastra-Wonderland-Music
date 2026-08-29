mod ai;
mod application;
mod review;
mod search;

use serde::{Deserialize, Serialize};

use crate::features::chat_text::{
    CommandSyntax, command_identity, parse_prefixed_command, strip_ascii_case_prefix,
};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
#[cfg(test)]
use crate::text::normalize_comparison_text;

pub(crate) use ai::{AiCandidatePickResult, AiClient, AiConfig};
pub(crate) use application::{
    ResolvedSongRequest, SongRequestApplication, SongRequestContext, SongRequestDecision,
    SongRequestPort, SongSearchFailure, select_ai_candidate,
};
pub(crate) use review::{
    SongReviewCandidate, SongReviewClient, SongReviewConfig, SongReviewDecision,
    split_candidate_title_artist,
};
pub use search::{CandidateEligibility, PickedCandidate, SearchCandidate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StructuredSongInput {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album: String,
}

impl StructuredSongInput {
    pub(crate) fn keyword(&self) -> String {
        [self.title.as_str(), self.artist.as_str()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Parses the metadata format emitted by the song-list OCR source.
///
/// The album field is accepted (and may be empty) so that a partially populated metadata line
/// still becomes a song request. The player daemon search endpoint accepts a title/artist query more
/// reliably than an album suffix, so the album is retained for diagnostics but is not appended to
/// the search keyword.
pub(crate) fn parse_structured_song_text(text: &str) -> Option<StructuredSongInput> {
    let text = text.trim();
    let title_start = text.find("歌曲名")?;
    let title_label_end = title_start + "歌曲名".len();
    let artist_start = text[title_label_end..].find("歌手名")? + title_label_end;
    let artist_label_end = artist_start + "歌手名".len();
    let album_start = text[artist_label_end..]
        .find("专辑名")
        .map(|offset| offset + artist_label_end);

    let title = structured_song_field(&text[title_label_end..artist_start])?;
    let artist_end = album_start.unwrap_or(text.len());
    let artist = structured_song_field(&text[artist_label_end..artist_end])?;
    let album = album_start
        .and_then(|start| structured_song_field(&text[start + "专辑名".len()..]))
        .unwrap_or_default();
    if title.is_empty() || artist.is_empty() {
        return None;
    }

    Some(StructuredSongInput {
        title,
        artist,
        album,
    })
}

fn structured_song_field(value: &str) -> Option<String> {
    let value = value
        .trim()
        .strip_prefix(':')
        .or_else(|| value.trim().strip_prefix('：'))?
        .trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, ',' | '，' | ';' | '；' | '。' | '.')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SongCommand {
    pub(crate) keyword: String,
    pub(crate) source: SongSource,
    pub(crate) prefix: String,
    pub(crate) prefer_accompaniment: bool,
    pub(crate) ai_assisted: bool,
    pub(crate) friend_username: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum SongSource {
    All,
    QqMusic,
    Netease,
    Bilibili,
    Kugou,
}

impl SongSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::All => "",
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
            Self::Kugou => "kugou",
        }
    }
}

impl SongCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        if envelope.prefix() != CommandPrefix::At {
            return false;
        }
        let commands = match envelope.authority() {
            CommandAuthority::HallMember => HALL_COMMANDS,
            CommandAuthority::Friend => FRIEND_COMMANDS,
        };
        commands.iter().any(|(prefix, _, _)| {
            strip_ascii_case_prefix(envelope.command_text(), prefix).is_some()
        })
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        match envelope.authority() {
            CommandAuthority::HallMember => {
                let parsed = parse_hall_syntax(envelope.command_text())?;
                let raw = joined_command(parsed.matched, parsed.argument);
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    raw,
                    parsed.command,
                ))
            }
            CommandAuthority::Friend => {
                let command = parse_friend_command(envelope.command_text(), envelope.username())?;
                let raw = format!(
                    "{} {} {}",
                    envelope.username(),
                    command.prefix,
                    command.keyword
                );
                Some(FeatureCommandMatch::new(
                    command.prefix.clone(),
                    raw,
                    command,
                ))
            }
        }
    }

    pub(crate) fn lock_key(&self) -> String {
        format!(
            "song:{}:{}:{}:{}",
            command_identity(&self.friend_username),
            self.source.as_str(),
            if self.prefer_accompaniment { 1 } else { 0 },
            command_identity(&self.keyword)
        )
    }

    #[cfg(test)]
    pub(crate) fn same_request(&self, other: &Self) -> bool {
        command_identity(&self.friend_username) == command_identity(&other.friend_username)
            && self.source == other.source
            && self.prefer_accompaniment == other.prefer_accompaniment
            && same_lock_keyword(&self.keyword, &other.keyword)
    }
}

fn joined_command(matched: &str, argument: &str) -> String {
    if argument.is_empty() {
        matched.to_string()
    } else {
        format!("{matched} {argument}")
    }
}

#[cfg(test)]
fn same_lock_keyword(left: &str, right: &str) -> bool {
    let left = normalize_comparison_text(left);
    let right = normalize_comparison_text(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    let min_length = left.chars().count().min(right.chars().count());
    if left.contains(&right) || right.contains(&left) {
        return min_length >= 2;
    }
    if min_length < 4 {
        return false;
    }
    let prefix_length = 16.min(min_length);
    let left_prefix = left.chars().take(prefix_length).collect::<String>();
    let right_prefix = right.chars().take(prefix_length).collect::<String>();
    levenshtein_distance(&left_prefix, &right_prefix) <= 1.max(prefix_length / 4)
}

#[cfg(test)]
fn levenshtein_distance(left: &str, right: &str) -> usize {
    let right_chars = right.chars().collect::<Vec<_>>();
    let mut costs = (0..=right_chars.len()).collect::<Vec<_>>();
    for (i, left_ch) in left.chars().enumerate() {
        let mut last = i;
        costs[0] = i + 1;
        for (j, right_ch) in right_chars.iter().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if left_ch == *right_ch {
                last
            } else {
                1 + last.min(costs[j]).min(costs[j + 1])
            };
            last = old;
        }
    }
    costs[right_chars.len()]
}

pub(crate) fn parse_hall_syntax(command: &str) -> Option<CommandSyntax<'_, SongCommand>> {
    for (prefix, source, ai_assisted) in HALL_COMMANDS {
        let Some(argument) = parse_prefixed_command(command, prefix, true) else {
            continue;
        };
        let song = parse_with_source(command, prefix, *source, *ai_assisted)?;
        return Some(CommandSyntax {
            matched: prefix,
            argument,
            command: song,
        });
    }
    None
}

pub(crate) fn parse_friend_command(command: &str, username: &str) -> Option<SongCommand> {
    parse_with_commands(command, username, FRIEND_COMMANDS)
}

fn parse_with_commands(
    command: &str,
    username: &str,
    commands: &[(&str, SongSource, bool)],
) -> Option<SongCommand> {
    for (prefix, source, ai_assisted) in commands {
        if strip_ascii_case_prefix(command, prefix).is_some() {
            let mut song = parse_with_source(command, prefix, *source, *ai_assisted)?;
            song.friend_username = username.to_string();
            return Some(song);
        }
    }
    None
}

fn parse_with_source(
    command: &str,
    prefix: &str,
    source: SongSource,
    ai_assisted: bool,
) -> Option<SongCommand> {
    let raw_keyword = command[prefix.len()..].trim();
    let prefer_accompaniment = raw_keyword.contains("伴奏");
    let keyword = raw_keyword
        .replace("伴奏", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return None;
    }
    Some(SongCommand {
        keyword,
        source,
        prefix: prefix.to_string(),
        prefer_accompaniment,
        ai_assisted,
        friend_username: String::new(),
    })
}

const HALL_COMMANDS: &[(&str, SongSource, bool)] = &[
    ("AI点歌", SongSource::QqMusic, true),
    ("AI搜索", SongSource::QqMusic, true),
    ("QQ点歌", SongSource::QqMusic, false),
    ("QQ搜索", SongSource::QqMusic, false),
    ("网易点歌", SongSource::Netease, false),
    ("网易云点歌", SongSource::Netease, false),
    ("网易搜索", SongSource::Netease, false),
    ("酷狗点歌", SongSource::Kugou, false),
    ("点歌", SongSource::QqMusic, false),
    ("搜索", SongSource::QqMusic, false),
];

// 好友点歌默认开放全部音源；AI 点歌和 AI 搜索使用 All，由后端在全部音源中选择结果。
const FRIEND_COMMANDS: &[(&str, SongSource, bool)] = &[
    ("AI点歌", SongSource::All, true),
    ("AI搜索", SongSource::All, true),
    ("QQ点歌", SongSource::QqMusic, false),
    ("QQ搜索", SongSource::QqMusic, false),
    ("网易点歌", SongSource::Netease, false),
    ("网易云点歌", SongSource::Netease, false),
    ("网易搜索", SongSource::Netease, false),
    ("酷狗点歌", SongSource::Kugou, false),
    ("B站点歌", SongSource::Bilibili, false),
    ("B站搜索", SongSource::Bilibili, false),
    ("点歌", SongSource::QqMusic, false),
    ("搜索", SongSource::QqMusic, false),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_song_metadata_and_ignores_album_in_keyword() {
        let input = parse_structured_song_text(
            "歌曲名：醉，歌手名：三无Marblue，专辑名：三无Marblue（170509更新）",
        )
        .expect("structured song input");

        assert_eq!(input.title, "醉");
        assert_eq!(input.artist, "三无Marblue");
        assert_eq!(input.album, "三无Marblue（170509更新）");
        assert_eq!(input.keyword(), "醉 三无Marblue");
    }

    #[test]
    fn accepts_ascii_colons_and_missing_album_field() {
        let input = parse_structured_song_text("歌曲名:流月人间,歌手名:泠鸢yousa")
            .expect("structured song input");

        assert_eq!(input.keyword(), "流月人间 泠鸢yousa");
        assert!(input.album.is_empty());
    }

    #[test]
    fn rejects_incomplete_structured_song_metadata() {
        assert!(parse_structured_song_text("歌曲名：醉，专辑名：专辑").is_none());
        assert!(parse_structured_song_text("歌手名：三无，专辑名：专辑").is_none());
        assert!(parse_structured_song_text("歌曲名：，歌手名：三无").is_none());
    }
}
