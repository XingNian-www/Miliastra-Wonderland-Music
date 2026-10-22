mod ai;
mod application;
mod commands;
mod review;
mod search;

use serde::{Deserialize, Serialize};

use crate::features::chat_text::{CommandSyntax, command_identity};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
pub(crate) use commands::{command_specs, permission_id_for_command_name, platform_sources};

pub(crate) use ai::{AiCandidatePickResult, AiClient, SongAiConfig};
pub(crate) use application::{
    ResolvedSongRequest, SongRequestApplication, SongRequestContext, SongRequestDecision,
    SongRequestPort, SongSearchFailure,
};
pub(crate) use review::{
    SongReviewCandidate, SongReviewClient, SongReviewConfig, SongReviewDecision,
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

/// 解析歌曲列表 OCR 源输出的元数据格式。
///
/// 接受专辑字段且允许为空，使不完整的元数据行也能生成点歌请求。
/// 播放器搜索接口对歌名/歌手查询的支持比附带专辑后缀更稳定，因此专辑仅保留用于诊断，
/// 不追加到搜索关键词。
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
    pub(crate) fn from_str(value: &str) -> Option<Self> {
        platform_sources()
            .map(|(source, _)| source)
            .find(|source| source.as_str() == value)
    }

    pub(crate) fn permission_id(self) -> Option<&'static str> {
        commands::permission_id(self)
    }

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
    pub(crate) fn permission_id(&self) -> &'static str {
        if self.ai_assisted {
            commands::ai_permission_id()
        } else {
            self.source
                .permission_id()
                .unwrap_or_else(commands::ai_permission_id)
        }
    }

    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        if envelope.prefix() != CommandPrefix::At {
            return false;
        }
        commands::match_entry(envelope.command_text(), envelope.authority()).is_some()
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
}

fn joined_command(matched: &str, argument: &str) -> String {
    if argument.is_empty() {
        matched.to_string()
    } else {
        format!("{matched} {argument}")
    }
}

pub(crate) fn parse_hall_syntax(command: &str) -> Option<CommandSyntax<'_, SongCommand>> {
    let entry = commands::match_entry(command, CommandAuthority::HallMember)?;
    let song = parse_with_source(command, entry.matched, entry.source, entry.ai_assisted)?;
    Some(CommandSyntax {
        matched: entry.matched,
        argument: entry.argument,
        command: song,
    })
}

pub(crate) fn parse_friend_command(command: &str, username: &str) -> Option<SongCommand> {
    let entry = commands::match_entry(command, CommandAuthority::Friend)?;
    let mut song = parse_with_source(command, entry.matched, entry.source, entry.ai_assisted)?;
    // Only the default friend alias recognizes a direct BV number; explicit QQ
    // and hall requests retain their selected source.
    if entry.matched == "点歌" && entry.source == SongSource::QqMusic {
        if let Some(bvid) = miliastra_playback::bilibili_normalize_bvid(&song.keyword) {
            song.source = SongSource::Bilibili;
            song.keyword = bvid;
        }
    }
    song.friend_username = username.to_string();
    Some(song)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_options_round_trip_through_the_same_registry() {
        let platforms: Vec<_> = platform_sources().collect();
        assert_eq!(platforms.len(), 4);
        for (source, label) in platforms {
            assert_ne!(source, SongSource::All);
            assert_eq!(SongSource::from_str(source.as_str()), Some(source));
            assert_eq!(source.permission_id().unwrap(), format!("@{label}"));
        }
        for invalid in ["", "all", "unknown", "AI点歌"] {
            assert_eq!(SongSource::from_str(invalid), None);
        }
    }

    #[test]
    fn registry_aliases_parse_with_independent_platform_permissions() {
        for spec in command_specs() {
            for name in spec.all_names() {
                for authority in spec.authorities {
                    let text = format!("{name}晴天");
                    let command = match authority {
                        CommandAuthority::HallMember => parse_hall_syntax(&text).unwrap().command,
                        CommandAuthority::Friend => parse_friend_command(&text, "friend").unwrap(),
                    };
                    assert_eq!(command.keyword, "晴天");
                    assert_eq!(command.prefix, name);
                    assert_eq!(command.permission_id(), format!("@{}", spec.name));
                    assert_eq!(
                        command.friend_username,
                        if *authority == CommandAuthority::Friend {
                            "friend"
                        } else {
                            ""
                        }
                    );
                    assert!(spec.matches_text(&text));
                }
            }
        }
        assert_eq!(SongSource::All.permission_id(), None);
    }

    #[test]
    fn registry_preserves_friend_bv_and_ai_source_semantics() {
        let bv = "BV1xx411c7mD";
        let friend = parse_friend_command(&format!("点歌{bv}"), "friend").unwrap();
        assert_eq!(friend.source, SongSource::Bilibili);
        assert_eq!(friend.permission_id(), "@B站点歌");
        assert_eq!(
            parse_friend_command(&format!("QQ点歌{bv}"), "friend")
                .unwrap()
                .source,
            SongSource::QqMusic
        );
        assert_eq!(
            parse_hall_syntax(&format!("点歌{bv}"))
                .unwrap()
                .command
                .source,
            SongSource::QqMusic
        );
        assert!(parse_hall_syntax(&format!("B站点歌{bv}")).is_none());
        assert_eq!(
            parse_friend_command("AI点歌晴天", "friend").unwrap().source,
            SongSource::All
        );
        assert_eq!(
            parse_hall_syntax("AI点歌晴天").unwrap().command.source,
            SongSource::QqMusic
        );
        assert_eq!(
            parse_friend_command("ai点歌晴天伴奏", "friend")
                .unwrap()
                .permission_id(),
            "@AI点歌"
        );
        assert!(
            parse_friend_command("ai点歌晴天伴奏", "friend")
                .unwrap()
                .prefer_accompaniment
        );
    }

    #[test]
    fn removed_search_aliases_are_neither_parsed_nor_enumerated() {
        for prefix in [
            "搜索",
            "AI搜索",
            "QQ搜索",
            "网易搜索",
            "网易云搜索",
            "酷狗搜索",
            "B站搜索",
        ] {
            let text = format!("{prefix}晴天");
            assert!(parse_hall_syntax(&text).is_none(), "hall {prefix}");
            assert!(
                parse_friend_command(&text, "friend").is_none(),
                "friend {prefix}"
            );
            assert!(command_specs().iter().all(|spec| !spec.matches_text(&text)));
        }
    }

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
