//! 本地曲库候选检索：不是准确命中判定，也不授予播放权限。
//! OCR 会删除中文之间的空格，因此关键词匹配不依赖空格。
use std::collections::HashSet;

use crate::{PlayableTrack, PlaybackEligibility, SearchCandidate, SearchQuery};

pub const MAX_LIBRARY_SEARCH_RESULTS: usize = 10;

/// 去掉全部空白，用于比对 OCR 可能丢失空格的连写关键词。
fn compact(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// 仅归一化大小写、全角 ASCII 和空白；保留版本词及有意义的标点。
fn normalize(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '　' => ' ',
            '！'..='～' => char::from_u32(ch as u32 - 0xfee0).unwrap_or(ch),
            _ => ch,
        })
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 连写片段短于这个长度时，必须等于整个字段，避免把关键词拆成无意义碎片。
const MIN_JOINED_PIECE_CHARS: usize = 2;
/// 连写关键词过长时不再做片段覆盖，交给 AI 语义改写后的二次搜索处理。
const MAX_JOINED_KEYWORD_CHARS: usize = 32;

/// OCR 会删掉中文之间的空格，关键词会把歌名和歌手连写在一起，先后顺序也不固定
/// （有些来源的歌名本身就含演唱者）。
///
/// 判定方式是经典的「字典分词」动态规划（Word Break）：目标串是连写关键词，
/// 词典是该曲目的歌名与各位歌手，reachable[i] 表示前 i 个字符能被若干片段依次覆盖。
/// 片段顺序任意，所以歌名与歌手的先后位置不影响结果。
fn covers_joined_fields(keyword: &str, fields: &[String]) -> bool {
    let chars: Vec<char> = keyword.chars().collect();
    if chars.is_empty() || chars.len() > MAX_JOINED_KEYWORD_CHARS {
        return false;
    }
    let mut reachable = vec![false; chars.len() + 1];
    reachable[0] = true;
    for start in 0..chars.len() {
        if !reachable[start] {
            continue;
        }
        for end in (start + 1)..=chars.len() {
            let piece: String = chars[start..end].iter().collect();
            // 单字片段只在正好是某个完整字段时成立，单字歌名（如「懒」「痒」）依赖这条规则。
            let matched = fields.iter().any(|field| {
                if piece.chars().count() < MIN_JOINED_PIECE_CHARS {
                    field == &piece
                } else {
                    field.contains(&piece)
                }
            });
            if matched {
                reachable[end] = true;
            }
        }
    }
    reachable[chars.len()]
}

/// 从曲目元数据中选取有限候选；每个关键词必须落在歌名或歌手中，
/// 或由连写关键词拆分后在歌名/歌手中依次出现（OCR 丢空格的场景）。
/// 结果保持 Unknown，由正常解析/播放链路验证当前账号和资源是否可用。
pub fn search_library_tracks(
    query: &SearchQuery,
    tracks: impl IntoIterator<Item = PlayableTrack>,
) -> Vec<SearchCandidate> {
    let keyword = normalize(&query.keyword);
    let terms: Vec<_> = keyword.split_whitespace().collect();
    let compact_keyword = compact(&keyword);
    let limit = query.limit.min(MAX_LIBRARY_SEARCH_RESULTS);
    if limit == 0 || terms.is_empty() || terms.len() > 16 || keyword.chars().count() > 256 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut ranked = Vec::new();
    for track in tracks {
        let provider = track.track_ref.key.provider;
        if !query.providers.is_empty() && !query.providers.contains(&provider) {
            continue;
        }
        let title = normalize(&track.metadata.title);
        let artists = normalize(&track.metadata.artists.join(" "));
        if title.is_empty() {
            continue;
        }
        let (compact_title, compact_artists) = (compact(&title), compact(&artists));
        let forward = format!("{compact_title}{compact_artists}");
        let backward = format!("{compact_artists}{compact_title}");
        let matched = terms
            .iter()
            .all(|term| title.contains(term) || artists.contains(term))
            || (!compact_keyword.is_empty() && {
                let mut fields = Vec::with_capacity(track.metadata.artists.len() + 1);
                fields.push(compact_title.clone());
                fields.extend(
                    track
                        .metadata
                        .artists
                        .iter()
                        .map(|artist| compact(&normalize(artist)))
                        .filter(|field| !field.is_empty()),
                );
                covers_joined_fields(&compact_keyword, &fields)
            });
        if !matched {
            continue;
        }
        if !seen.insert(track.track_ref.key.clone()) {
            continue;
        }
        let exact = keyword == title
            || keyword == format!("{title} {artists}")
            || keyword == format!("{artists} {title}")
            || compact_keyword == forward
            || compact_keyword == backward;
        let key = track.track_ref.key.to_string();
        let mut candidate = SearchCandidate {
            track_ref: track.track_ref,
            metadata: track.metadata,
            eligibility: PlaybackEligibility::Unknown,
            text: String::new(),
        };
        candidate.text = format!(
            "{} [{}·曲库]",
            candidate.selection_text(),
            provider.as_str()
        );
        ranked.push((exact, key, candidate));
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        ranked.truncate(limit);
    }
    ranked
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderId, ResolverLocator, TrackKey, TrackMetadata, TrackRef};

    fn track(provider: ProviderId, id: &str, title: &str, artist: &str) -> PlayableTrack {
        PlayableTrack {
            track_ref: TrackRef {
                key: TrackKey::new(provider, id).unwrap(),
                resolver_locator: None,
            },
            metadata: TrackMetadata {
                title: title.into(),
                artists: vec![artist.into()],
                album: None,
                duration_ms: None,
            },
        }
    }
    fn query(keyword: &str) -> SearchQuery {
        SearchQuery {
            keyword: keyword.into(),
            ..SearchQuery::default()
        }
    }

    #[test]
    fn library_search_matches_all_terms_in_either_order_and_retains_versions() {
        let tracks = vec![
            track(ProviderId::QqMusic, "1", "晴天", "周杰伦"),
            track(ProviderId::QqMusic, "2", "晴天 (Live)", "周杰伦"),
        ];
        assert_eq!(
            search_library_tracks(&query("周杰伦 晴天"), tracks.clone()).len(),
            2
        );
        assert_eq!(
            search_library_tracks(&query("晴天 周杰伦 LIVE"), tracks.clone())[0]
                .track_ref
                .key
                .id,
            "2"
        );
        assert!(search_library_tracks(&query("晴天 周杰伦 伴奏"), tracks).is_empty());
    }

    #[test]
    fn library_search_normalizes_case_width_and_whitespace_not_symbols() {
        let tracks = vec![track(
            ProviderId::QqMusic,
            "1",
            "Love Story",
            "Taylor Swift",
        )];
        assert_eq!(
            search_library_tracks(&query("　ＬＯＶＥ  story　taylor"), tracks.clone()).len(),
            1
        );
        assert!(search_library_tracks(&query("Love! Story"), tracks).is_empty());
    }

    #[test]
    fn library_search_filters_sources_and_deduplicates_keys_not_titles() {
        let mut q = query("晴天");
        q.providers = vec![ProviderId::QqMusic];
        let a = track(ProviderId::QqMusic, "1", "晴天", "周杰伦");
        let b = track(ProviderId::QqMusic, "2", "晴天", "翻唱歌手");
        let c = track(ProviderId::Bilibili, "BV1", "晴天", "周杰伦");
        let results = search_library_tracks(&q, vec![a.clone(), a, b, c]);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|item| item.eligibility == PlaybackEligibility::Unknown)
        );
    }

    #[test]
    fn library_search_preserves_locator_and_prioritizes_exact_title() {
        let mut exact = track(ProviderId::QqMusic, "z", "晴天", "周杰伦");
        exact.track_ref.resolver_locator =
            Some(ResolverLocator::new("qqmusic:v2:z:media").unwrap());
        let partial = track(ProviderId::QqMusic, "a", "晴天 (Live)", "周杰伦");
        let results = search_library_tracks(&query("晴天"), vec![partial, exact.clone()]);
        assert_eq!(results[0].track_ref, exact.track_ref);
    }

    #[test]
    fn library_search_scans_beyond_first_page_and_bounds_results() {
        let tracks = (0..120).map(|i| {
            track(
                ProviderId::QqMusic,
                &i.to_string(),
                if i == 119 { "目标" } else { "其他" },
                "歌手",
            )
        });
        assert_eq!(
            search_library_tracks(&query("目标"), tracks)[0]
                .track_ref
                .key
                .id,
            "119"
        );
        let tracks = (0..30).map(|i| track(ProviderId::QqMusic, &i.to_string(), "目标", "歌手"));
        assert_eq!(
            search_library_tracks(&query("目标"), tracks).len(),
            MAX_LIBRARY_SEARCH_RESULTS
        );
    }

    #[test]
    fn library_search_rejects_empty_or_excessive_queries() {
        let a = track(ProviderId::QqMusic, "1", "晴天", "周杰伦");
        assert!(search_library_tracks(&query("  "), vec![a.clone()]).is_empty());
        assert!(search_library_tracks(&query(&"晴".repeat(257)), vec![a.clone()]).is_empty());
        let mut q = query("晴天");
        q.limit = 0;
        assert!(search_library_tracks(&q, vec![a]).is_empty());
    }

    #[test]
    fn library_search_matches_queries_whose_spaces_were_dropped_by_ocr() {
        let tracks = vec![track(ProviderId::QqMusic, "1", "晴天", "周杰伦")];
        assert_eq!(
            search_library_tracks(&query("晴天周杰伦"), tracks.clone()).len(),
            1
        );
        assert_eq!(
            search_library_tracks(&query("周杰伦晴天"), tracks.clone()).len(),
            1
        );
        // 连写关键词按整体命中处理，排在只覆盖一部分的候选前面。
        let mut exact = track(ProviderId::QqMusic, "z", "晴天", "周杰伦");
        exact.track_ref.resolver_locator =
            Some(ResolverLocator::new("qqmusic:v2:z:media").unwrap());
        let partial = track(ProviderId::QqMusic, "a", "晴天", "周杰伦爱唱歌");
        let results = search_library_tracks(&query("晴天周杰伦"), vec![partial, exact.clone()]);
        assert_eq!(results[0].track_ref, exact.track_ref);
    }

    #[test]
    fn library_search_joins_multiple_artists_and_keeps_symbol_sensitivity() {
        let mut duo = track(ProviderId::QqMusic, "1", "晴天", "周杰伦");
        duo.metadata.artists = vec!["周杰伦".into(), "方文山".into()];
        assert_eq!(
            search_library_tracks(&query("周杰伦方文山"), vec![duo.clone()]).len(),
            1
        );
        assert_eq!(
            search_library_tracks(&query("晴天周杰伦方文山"), vec![duo]).len(),
            1
        );
        let english = vec![track(
            ProviderId::QqMusic,
            "2",
            "Love Story",
            "Taylor Swift",
        )];
        assert!(search_library_tracks(&query("Love!Story"), english.clone()).is_empty());
        assert_eq!(
            search_library_tracks(&query("LoveStory"), english.clone()).len(),
            1
        );
        assert_eq!(
            search_library_tracks(&query("TaylorSwift"), english).len(),
            1
        );
    }

    #[test]
    fn library_search_covers_joined_queries_across_title_and_every_artist() {
        let mut multi = track(ProviderId::Kugou, "k", "芊芊", "西瓜JUN");
        multi.metadata.artists = vec!["西瓜JUN".into(), "排骨教主".into()];
        let ordered = track(ProviderId::QqMusic, "q", "芊芊", "排骨教主");
        let embedded = track(
            ProviderId::Bilibili,
            "bv",
            "【西瓜JUNx排骨教主】11.11年度巨献《芊芊》（纯男声）",
            "排骨教主",
        );
        let all = vec![multi.clone(), ordered.clone(), embedded.clone()];
        // 片段可以分别落在歌名和不同歌手上，先后顺序不限。
        assert_eq!(
            search_library_tracks(&query("芊芊排骨教主"), all.clone()).len(),
            3
        );
        assert_eq!(
            search_library_tracks(&query("排骨教主芊芊"), all.clone()).len(),
            3
        );
        // 歌名自身含演唱者、片段顺序与歌名内部顺序不同时同样命中。
        assert_eq!(
            search_library_tracks(&query("芊芊排骨教主西瓜jun"), all).len(),
            2
        );
    }

    #[test]
    fn library_search_joined_fragments_must_all_be_present() {
        let tracks = vec![track(ProviderId::QqMusic, "1", "芊芊", "排骨教主")];
        assert!(search_library_tracks(&query("芊芊林俊杰"), tracks.clone()).is_empty());
        assert!(search_library_tracks(&query("芊芊排骨云韵"), tracks.clone()).is_empty());
        // 单字片段必须等于整个字段，不能只靠包含关系。
        assert!(search_library_tracks(&query("芊芊排"), tracks.clone()).is_empty());
        assert_eq!(
            search_library_tracks(&query("芊芊排骨"), tracks.clone()).len(),
            1
        );
        // 单字歌名依赖「整字段相等」这条规则。
        let short = vec![
            track(ProviderId::QqMusic, "2", "懒", "花粥"),
            track(ProviderId::QqMusic, "3", "痒", "黄龄"),
        ];
        assert_eq!(
            search_library_tracks(&query("懒花粥"), short.clone()).len(),
            1
        );
        assert_eq!(search_library_tracks(&query("痒黄龄"), short).len(), 1);
    }
}
