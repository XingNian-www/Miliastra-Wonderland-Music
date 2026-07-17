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
    use super::same_song_query;

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
}
