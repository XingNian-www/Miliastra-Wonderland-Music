//! 本地曲库候选检索：不是准确命中判定，也不授予播放权限。
use std::collections::HashSet;

use crate::{PlayableTrack, PlaybackEligibility, SearchCandidate, SearchQuery};

pub const MAX_LIBRARY_SEARCH_RESULTS: usize = 10;

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

/// 从曲目元数据中选取有限候选；每个关键词必须落在歌名或歌手中。
/// 结果保持 Unknown，由正常解析/播放链路验证当前账号和资源是否可用。
pub fn search_library_tracks(
    query: &SearchQuery,
    tracks: impl IntoIterator<Item = PlayableTrack>,
) -> Vec<SearchCandidate> {
    let keyword = normalize(&query.keyword);
    let terms: Vec<_> = keyword.split_whitespace().collect();
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
        if title.is_empty()
            || !terms
                .iter()
                .all(|term| title.contains(term) || artists.contains(term))
        {
            continue;
        }
        if !seen.insert(track.track_ref.key.clone()) {
            continue;
        }
        let exact = keyword == title
            || keyword == format!("{title} {artists}")
            || keyword == format!("{artists} {title}");
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
}
