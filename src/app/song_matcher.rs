use super::config::MatchConfig;

pub fn match_song_query(
    config: &MatchConfig,
    query: &str,
    returned_name: &str,
    returned_singers: &str,
    prefer_accompaniment: bool,
) -> MatchResult {
    if !prefer_accompaniment && is_accompaniment_title(returned_name) {
        return MatchResult::no("非伴奏点歌不接受伴奏版本");
    }

    let query_text = if prefer_accompaniment {
        strip_accompaniment_markers(query)
    } else {
        query.to_string()
    };
    let name_text = if prefer_accompaniment {
        strip_accompaniment_markers(returned_name)
    } else {
        returned_name.to_string()
    };
    let normalized_query = normalize(&query_text);
    let Some(name_match) = best_returned_name_match(config, &normalized_query, &name_text) else {
        return MatchResult::no("缺少点歌文本或返回歌曲名");
    };
    let normalized_name = name_match.normalized.as_str();
    if normalized_query.is_empty() {
        return MatchResult::no("缺少点歌文本或返回歌曲名");
    }

    let has_full_name = normalized_query.contains(&normalized_name);
    let name_score = name_match.score;
    if name_score < config.min_song_name_score {
        return MatchResult::no(&format!(
            "歌曲名匹配度{}%",
            (name_score * 100.0).round() as i32
        ));
    }
    if is_contained_query_name(&normalized_name, &normalized_query) {
        return MatchResult::yes("歌名包含匹配");
    }

    let singer_candidate = remove_matched_name(config, &normalized_query, &normalized_name);
    if singer_candidate.is_empty() {
        return MatchResult::yes("歌曲名匹配");
    }

    if has_full_name
        && !has_singer_separator_after_name(query, &name_match.raw)
        && singer_candidate.chars().count() <= config.max_ocr_noise_chars + 1
    {
        return MatchResult::yes(&format!("忽略OCR噪声:{}", singer_candidate));
    }

    if singer_matches(config, &singer_candidate, returned_singers) {
        return MatchResult::yes(&format!("歌手匹配:{}", singer_candidate));
    }

    if !has_full_name {
        if singer_candidate.chars().count() <= config.max_ocr_noise_chars + 1 {
            return MatchResult::yes(&format!("模糊匹配:{}", singer_candidate));
        }
        let name_cn_chars = chinese_chars(&normalized_name);
        let max_strip = singer_candidate
            .chars()
            .count()
            .saturating_sub(1)
            .min(name_cn_chars.len());
        for strip in 1..=max_strip {
            let stripped = singer_candidate.chars().skip(strip).collect::<String>();
            if singer_matches(config, &stripped, returned_singers) {
                return MatchResult::yes(&format!("剥字后歌手匹配:{}", stripped));
            }
        }
    }

    MatchResult::no(&format!("歌手不匹配:{}", singer_candidate))
}

#[derive(Clone, Debug)]
struct NameMatch {
    raw: String,
    normalized: String,
    score: f64,
}

fn best_returned_name_match(
    config: &MatchConfig,
    normalized_query: &str,
    returned_name: &str,
) -> Option<NameMatch> {
    title_match_candidates(returned_name)
        .into_iter()
        .filter_map(|raw| {
            let normalized = normalize(&raw);
            if normalized.is_empty() {
                return None;
            }
            Some(NameMatch {
                score: score_returned_name(config, &normalized, normalized_query),
                raw,
                normalized,
            })
        })
        .max_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    left.normalized
                        .chars()
                        .count()
                        .cmp(&right.normalized.chars().count())
                })
        })
}

#[derive(Clone, Debug)]
pub struct MatchResult {
    pub ok: bool,
    pub reason: String,
}

impl MatchResult {
    fn yes(reason: &str) -> Self {
        Self {
            ok: true,
            reason: reason.to_string(),
        }
    }

    fn no(reason: &str) -> Self {
        Self {
            ok: false,
            reason: reason.to_string(),
        }
    }
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

pub fn is_accompaniment_title(value: &str) -> bool {
    let lower = value.to_lowercase();
    [
        "伴奏",
        "伴唱",
        "纯伴奏",
        "纯伴唱",
        "instrumental",
        "karaoke",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn title_match_candidates(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![trimmed.to_string()];
    let without_brackets = remove_bracketed_sections(trimmed).trim().to_string();
    if !without_brackets.is_empty() {
        candidates.push(without_brackets);
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn remove_bracketed_sections(value: &str) -> String {
    let mut output = String::new();
    let mut stack = Vec::new();
    for ch in value.chars() {
        if let Some(close) = bracket_close(ch) {
            stack.push(close);
            continue;
        }
        if stack.last().is_some_and(|close| *close == ch) {
            stack.pop();
            continue;
        }
        if stack.is_empty() {
            output.push(ch);
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bracket_close(ch: char) -> Option<char> {
    match ch {
        '(' => Some(')'),
        '（' => Some('）'),
        '[' => Some(']'),
        '【' => Some('】'),
        '{' => Some('}'),
        '《' => Some('》'),
        '「' => Some('」'),
        _ => None,
    }
}

fn strip_accompaniment_markers(value: &str) -> String {
    let lower = value.to_lowercase();
    let mut output = String::new();
    let mut index = 0;
    while index < value.len() {
        let rest = &lower[index..];
        let Some(marker) = [
            "伴奏版",
            "伴唱版",
            "纯伴奏",
            "纯伴唱",
            "伴奏",
            "伴唱",
            "instrumental",
            "karaoke",
        ]
        .iter()
        .find(|marker| rest.starts_with(**marker)) else {
            let ch = value[index..].chars().next().unwrap();
            output.push(ch);
            index += ch.len_utf8();
            continue;
        };
        index += marker.len();
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn score_returned_name(config: &MatchConfig, normalized_name: &str, normalized_query: &str) -> f64 {
    if normalized_name.is_empty() || normalized_query.is_empty() {
        return 0.0;
    }
    if normalized_query.contains(normalized_name)
        || is_contained_query_name(normalized_name, normalized_query)
    {
        return 1.0;
    }
    if normalized_name.chars().count() <= 1 {
        return if normalized_query.contains(normalized_name) {
            1.0
        } else {
            0.0
        };
    }
    if is_mostly_chinese(normalized_name) {
        return score_chinese_name(config, normalized_name, normalized_query);
    }
    let cn_chars = chinese_chars(normalized_name);
    if cn_chars.len() >= 2 {
        let cn_score =
            count_chinese_char_hits(&cn_chars, normalized_query) as f64 / cn_chars.len() as f64;
        if cn_score >= config.min_song_name_score {
            return cn_score;
        }
    }
    if normalized_name.starts_with(normalized_query) && normalized_query.chars().count() >= 3 {
        return normalized_query.chars().count() as f64 / normalized_name.chars().count() as f64;
    }
    find_best_substring(
        normalized_query,
        normalized_name,
        config.en_max_edit_fraction,
    )
    .score
}

fn is_contained_query_name(normalized_name: &str, normalized_query: &str) -> bool {
    let min_length = if is_mostly_chinese(normalized_query) {
        2
    } else {
        3
    };
    normalized_query.chars().count() >= min_length && normalized_name.contains(normalized_query)
}

fn score_chinese_name(config: &MatchConfig, normalized_name: &str, normalized_query: &str) -> f64 {
    let chars = chinese_chars(normalized_name);
    if chars.is_empty() {
        return 0.0;
    }
    let hit = count_chinese_char_hits(&chars, normalized_query);
    if chars.len() <= 2 {
        return if hit == chars.len() {
            1.0
        } else {
            hit as f64 / chars.len() as f64
        };
    }
    if chars.len() <= 4 {
        return if hit
            >= chars
                .len()
                .saturating_sub(config.short_chinese_song_max_miss)
        {
            hit as f64 / chars.len() as f64
        } else {
            0.0
        };
    }
    let score = hit as f64 / chars.len() as f64;
    if score >= config.long_chinese_song_min_score {
        score
    } else {
        0.0
    }
}

fn count_chinese_char_hits(chars: &[char], normalized_query: &str) -> usize {
    let query_chars = normalized_query.chars().collect::<Vec<_>>();
    let mut used = vec![false; query_chars.len()];
    let mut hit = 0;
    for ch in chars {
        for (index, query_ch) in query_chars.iter().enumerate() {
            if !used[index] && query_ch == ch {
                used[index] = true;
                hit += 1;
                break;
            }
        }
    }
    hit
}

fn remove_matched_name(
    config: &MatchConfig,
    normalized_query: &str,
    normalized_name: &str,
) -> String {
    if normalized_query.contains(normalized_name) {
        return normalized_query.replacen(normalized_name, "", 1);
    }
    if !is_mostly_chinese(normalized_name) {
        let matched = find_best_substring(
            normalized_query,
            normalized_name,
            config.en_max_edit_fraction,
        );
        if matched.score >= config.min_song_name_score {
            let chars = normalized_query.chars().collect::<Vec<_>>();
            return chars[..matched.start]
                .iter()
                .chain(chars[matched.end..].iter())
                .collect();
        }
    }

    let chars = normalized_query.chars().collect::<Vec<_>>();
    let mut used = vec![false; chars.len()];
    let mut start_index = 0;
    for name_ch in normalized_name.chars() {
        let mut found_index = None;
        for query_index in start_index..chars.len() {
            if !used[query_index] && chars[query_index] == name_ch {
                found_index = Some(query_index);
                break;
            }
        }
        if let Some(index) = found_index {
            used[index] = true;
            start_index = index + 1;
        }
    }
    chars
        .iter()
        .enumerate()
        .filter_map(|(index, ch)| (!used[index]).then_some(*ch))
        .collect()
}

fn singer_matches(config: &MatchConfig, singer_candidate: &str, returned_singers: &str) -> bool {
    let candidate = normalize(singer_candidate);
    let singer_text = normalize(returned_singers);
    if candidate.is_empty() {
        return true;
    }
    if singer_text.is_empty() {
        return false;
    }
    if singer_text.contains(&candidate) {
        return true;
    }
    let split = split_singers(returned_singers);
    if split_singers_cover_candidate(config, &candidate, &split) {
        return true;
    }
    if split.iter().any(|singer| {
        singer.contains(&candidate) || fuzzy_singer_matches(config, &candidate, singer)
    }) {
        return true;
    }
    if !is_mostly_chinese(returned_singers) {
        return ed_singer_matches(config, &candidate, returned_singers);
    }
    false
}

fn split_singers_cover_candidate(
    config: &MatchConfig,
    candidate: &str,
    split_singers: &[String],
) -> bool {
    if split_singers.len() < 2 || candidate.chars().count() < 4 {
        return false;
    }

    let mut remaining = candidate.to_string();
    let mut singers = split_singers.iter().collect::<Vec<_>>();
    singers.sort_by_key(|singer| std::cmp::Reverse(singer.chars().count()));
    let mut matched_count = 0;
    for singer in singers {
        if singer.chars().count() < 2 {
            continue;
        }
        if let Some(index) = remaining.find(singer) {
            remaining.replace_range(index..index + singer.len(), "");
            matched_count += 1;
        }
    }

    matched_count >= 2 && remaining.chars().count() <= config.max_ocr_noise_chars + 1
}

fn ed_singer_matches(config: &MatchConfig, candidate: &str, returned_singers: &str) -> bool {
    if candidate.chars().count() < 3 {
        return false;
    }
    split_singers(returned_singers).iter().any(|singer| {
        if singer.contains(candidate) {
            return true;
        }
        if singer.chars().count() < 3 || candidate.chars().count() < 3 {
            return false;
        }
        let first = find_best_substring(candidate, singer, config.en_singer_max_edit_fraction);
        if first.score >= 0.6 {
            return true;
        }
        find_best_substring(singer, candidate, config.en_singer_max_edit_fraction).score >= 0.6
    })
}

fn fuzzy_singer_matches(config: &MatchConfig, candidate: &str, singer: &str) -> bool {
    if !config.enable_fuzzy_singer || !is_mostly_chinese(candidate) || !is_mostly_chinese(singer) {
        return false;
    }
    let candidate_chars = chinese_chars(candidate);
    let singer_chars = chinese_chars(singer);
    if candidate_chars.len() <= 2 || singer_chars.len() <= 2 {
        return false;
    }
    let hit = count_chinese_char_hits(&candidate_chars, singer);
    if candidate_chars.len() <= 4 {
        hit >= candidate_chars
            .len()
            .saturating_sub(config.short_chinese_singer_max_miss)
    } else {
        hit as f64 / candidate_chars.len() as f64 >= config.long_chinese_singer_min_score
    }
}

fn has_singer_separator_after_name(query: &str, returned_name: &str) -> bool {
    let raw_query = query.trim();
    let raw_name = returned_name.trim();
    if raw_query.is_empty() || raw_name.is_empty() {
        return false;
    }
    let Some(index) = raw_query.find(raw_name) else {
        return false;
    };
    raw_query[index + raw_name.len()..]
        .chars()
        .next()
        .is_some_and(|ch| {
            ch.is_whitespace() || matches!(ch, '-' | '_' | ':' | '：' | '/' | '／' | '|' | '｜')
        })
}

fn split_singers(value: &str) -> Vec<String> {
    value
        .split([',', '&', '，', '、', '/', '／', ';', '；', '|', '｜'])
        .map(normalize)
        .filter(|item| !item.is_empty())
        .collect()
}

fn find_best_substring(text: &str, pattern: &str, max_edit_fraction: f64) -> SubstringMatch {
    let text_chars = text.chars().collect::<Vec<_>>();
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    if text_chars.is_empty() || pattern_chars.is_empty() {
        return SubstringMatch::none(pattern_chars.len() + 1);
    }
    let max_dist = 1.max((pattern_chars.len() as f64 * max_edit_fraction).round() as usize);
    let mut best = SubstringMatch::none(pattern_chars.len() + 1);
    for start in 0..text_chars.len() {
        let max_len = (text_chars.len() - start).min(pattern_chars.len() + max_dist);
        let min_len = 2.max(pattern_chars.len().saturating_sub(max_dist));
        for len in min_len..=max_len {
            let window = text_chars[start..start + len].iter().collect::<String>();
            let dist = levenshtein_distance(&window, pattern);
            if dist < best.dist {
                best = SubstringMatch {
                    start,
                    end: start + len,
                    dist,
                    score: if dist < pattern_chars.len() {
                        1.0 - dist as f64 / pattern_chars.len() as f64
                    } else {
                        0.0
                    }
                    .max(0.0),
                };
            }
        }
    }
    if best.dist as f64 > max_dist as f64 * 1.5 {
        SubstringMatch::none(best.dist)
    } else {
        best
    }
}

#[derive(Clone, Copy)]
struct SubstringMatch {
    start: usize,
    end: usize,
    dist: usize,
    score: f64,
}

impl SubstringMatch {
    fn none(dist: usize) -> Self {
        Self {
            start: 0,
            end: 0,
            dist,
            score: 0.0,
        }
    }
}

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

fn is_mostly_chinese(value: &str) -> bool {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return false;
    }
    chinese_chars(value).len() as f64 / chars.len() as f64 >= 0.5
}

fn chinese_chars(value: &str) -> Vec<char> {
    value
        .chars()
        .filter(|ch| ('\u{4e00}'..='\u{9fff}').contains(ch))
        .collect()
}

pub fn normalize(value: &str) -> String {
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
    use super::*;

    #[test]
    fn matches_title_with_parenthesized_version_and_reordered_singers() {
        let result = match_song_query(
            &MatchConfig::default(),
            "最长的电影李心洁安崎",
            "最长的电影（乘风2026 二公现场）",
            "安崎 & 李心洁",
            false,
        );

        assert!(result.ok, "{}", result.reason);
    }

    #[test]
    fn bracketed_title_metadata_is_a_general_candidate() {
        let result = match_song_query(
            &MatchConfig::default(),
            "Lemon 米津玄师",
            "Lemon (Live at Tokyo)",
            "米津玄师",
            false,
        );

        assert!(result.ok, "{}", result.reason);
    }
}
