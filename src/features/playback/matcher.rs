use super::MatchConfig;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SongIdentityMatch {
    Match { score: f64, reason: String },
    Unknown { reason: String },
}

pub(crate) fn match_song_identity(
    config: &MatchConfig,
    request: &str,
    observed_title: &str,
    observed_artist: &str,
) -> SongIdentityMatch {
    let (requested_title, requested_artist) = split_title_artist(request);
    let observed_title = observed_title.trim();
    let observed_artist = observed_artist.trim();
    if requested_title.is_empty() || observed_title.is_empty() {
        return SongIdentityMatch::Unknown {
            reason: "缺少歌曲名，无法本地确认".to_string(),
        };
    }

    let title_match = same_song_query(&requested_title, observed_title);
    if !title_match {
        return SongIdentityMatch::Unknown {
            reason: "歌曲名存在差异，需要 AI 判断别名或版本".to_string(),
        };
    }

    if requested_artist.is_empty() || observed_artist.is_empty() {
        return SongIdentityMatch::Unknown {
            reason: "缺少歌手信息，不能仅凭歌曲名确认".to_string(),
        };
    }
    if !same_song_query(&requested_artist, observed_artist) {
        return SongIdentityMatch::Unknown {
            reason: "歌手存在差异，需要 AI 判断别名或合作艺人".to_string(),
        };
    }

    let score = 1.0;
    if score < config.min_song_name_score {
        return SongIdentityMatch::Unknown {
            reason: format!(
                "本地匹配分数 {:.2} 低于阈值 {:.2}",
                score, config.min_song_name_score
            ),
        };
    }
    SongIdentityMatch::Match {
        score,
        reason: "歌曲名和歌手归一化后匹配".to_string(),
    }
}

fn split_title_artist(value: &str) -> (String, String) {
    let text = value.trim();
    if let Some((title, artist)) = text.split_once(" - ") {
        return (title.trim().to_string(), artist.trim().to_string());
    }
    (text.to_string(), String::new())
}

pub fn same_song_query(left: &str, right: &str) -> bool {
    let left = normalize(left);
    let right = normalize(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    if left.chars().count() >= 2 && right.contains(&left) {
        return true;
    }
    right.chars().count() >= 2 && left.contains(&right)
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_whitespace() || is_punctuation(ch) {
                None
            } else if ('\u{ff01}'..='\u{ff5e}').contains(&ch) {
                char::from_u32(ch as u32 - 0xfee0)
            } else {
                Some(ch)
            }
        })
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn is_punctuation(ch: char) -> bool {
    ch.is_ascii_punctuation()
        || matches!(
            ch,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '？'
                | '！'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '￥'
                | '·'
                | '—'
                | '～'
                | '…'
                | '♪'
                | '♫'
                | '★'
                | '☆'
                | '「'
                | '」'
                | '・'
        )
}

#[cfg(test)]
mod tests {
    use super::{SongIdentityMatch, same_song_query};
    use crate::features::playback::MatchConfig;

    #[test]
    fn normalizes_punctuation_and_case() {
        assert!(same_song_query(" Hello！ ", "hello"));
    }

    #[test]
    fn rejects_empty_and_short_substrings() {
        assert!(!same_song_query("", "hello"));
        assert!(!same_song_query("a", "a-long-title"));
    }

    #[test]
    fn accepts_a_longer_query_containing_the_shorter_title() {
        assert!(same_song_query("晴天", "周杰伦 晴天 现场"));
    }

    #[test]
    fn match_config_confirms_normalized_title_and_artist_without_character_scoring() {
        let result = MatchConfig::default().match_song_identity(
            "好想爱这个世界啊 - 华晨宇",
            "好想爱这个世界啊",
            "华晨宇",
        );
        assert!(matches!(result, SongIdentityMatch::Match { score, .. } if score == 1.0));
    }

    #[test]
    fn match_config_defers_aliases_and_missing_artist_to_ai() {
        let config = MatchConfig::default();
        assert!(matches!(
            config.match_song_identity("晴天 - 周杰伦", "晴天 (Live)", "周杰伦"),
            SongIdentityMatch::Match { .. }
        ));
        assert!(matches!(
            config.match_song_identity("晴天 - 周杰伦", "晴天", ""),
            SongIdentityMatch::Unknown { .. }
        ));
    }
}
