use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedLyricLine {
    pub start_ms: u64,
    pub text: String,
    pub translation: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimedLyrics {
    pub lines: Vec<TimedLyricLine>,
}

impl TimedLyrics {
    pub fn new(lines: Vec<TimedLyricLine>) -> Option<Self> {
        (!lines.is_empty()).then_some(Self { lines })
    }

    pub fn line_at_ms(&self, position_ms: u64) -> Option<&str> {
        let index = self
            .lines
            .partition_point(|line| line.start_ms <= position_ms);
        let line = self.lines.get(index.checked_sub(1)?)?;
        let text = line
            .translation
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(&line.text);
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn line_at_seconds(&self, position_seconds: f64) -> Option<&str> {
        if !position_seconds.is_finite() || position_seconds < 0.0 {
            return None;
        }
        self.line_at_ms((position_seconds * 1000.0).round() as u64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LyricsParseError {
    #[error("lyric timestamp is invalid")]
    InvalidTimestamp,
}

pub fn parse_lrc_pair(
    primary: &str,
    translation: Option<&str>,
) -> Result<Option<TimedLyrics>, LyricsParseError> {
    let primary = parse_lrc_lines(primary)?;
    let translation = translation.map(parse_lrc_lines).transpose()?;
    let mut timestamps = primary.keys().copied().collect::<Vec<_>>();
    if let Some(translation) = translation.as_ref() {
        timestamps.extend(translation.keys().copied());
    }
    timestamps.sort_unstable();
    timestamps.dedup();

    let lines = timestamps
        .into_iter()
        .filter_map(|start_ms| {
            let text = primary.get(&start_ms).cloned().unwrap_or_default();
            let translated = translation
                .as_ref()
                .and_then(|items| items.get(&start_ms).cloned())
                .filter(|value| !value.trim().is_empty());
            if text.trim().is_empty() && translated.is_none() {
                return None;
            }
            Some(TimedLyricLine {
                start_ms,
                text,
                translation: translated,
            })
        })
        .collect::<Vec<_>>();
    Ok(TimedLyrics::new(lines))
}

fn parse_lrc_lines(value: &str) -> Result<BTreeMap<u64, String>, LyricsParseError> {
    let mut output = BTreeMap::<u64, String>::new();
    for raw_line in value.trim_start_matches('\u{feff}').lines() {
        let mut rest = raw_line.trim();
        let mut timestamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else {
                break;
            };
            let tag = &rest[1..end];
            if let Some(timestamp) = parse_timestamp(tag)? {
                timestamps.push(timestamp);
            }
            rest = rest[end + 1..].trim_start();
        }
        if timestamps.is_empty() {
            continue;
        }
        let text = rest.trim();
        if text.is_empty() {
            continue;
        }
        for timestamp in timestamps {
            let entry = output.entry(timestamp).or_default();
            if !entry.is_empty() {
                entry.push('\n');
            }
            entry.push_str(text);
        }
    }
    Ok(output)
}

fn parse_timestamp(value: &str) -> Result<Option<u64>, LyricsParseError> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 2 && parts.len() != 3 {
        return Ok(None);
    }
    if !parts[0].chars().all(|character| character.is_ascii_digit()) {
        return Ok(None);
    }
    let (hours, minutes, seconds) = if parts.len() == 2 {
        (0, parts[0], parts[1])
    } else {
        let hours = parts[0]
            .parse::<u64>()
            .map_err(|_| LyricsParseError::InvalidTimestamp)?;
        (hours, parts[1], parts[2])
    };
    let minutes = minutes
        .parse::<u64>()
        .map_err(|_| LyricsParseError::InvalidTimestamp)?;
    let (whole_seconds, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole_seconds = whole_seconds
        .parse::<u64>()
        .map_err(|_| LyricsParseError::InvalidTimestamp)?;
    if minutes >= 60 || whole_seconds >= 60 || fraction.len() > 3 || !fraction.is_ascii() {
        return Err(LyricsParseError::InvalidTimestamp);
    }
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => {
            fraction
                .parse::<u64>()
                .map_err(|_| LyricsParseError::InvalidTimestamp)?
                * 100
        }
        2 => {
            fraction
                .parse::<u64>()
                .map_err(|_| LyricsParseError::InvalidTimestamp)?
                * 10
        }
        _ => fraction
            .parse::<u64>()
            .map_err(|_| LyricsParseError::InvalidTimestamp)?,
    };
    Ok(Some(
        hours
            .saturating_mul(3_600_000)
            .saturating_add(minutes.saturating_mul(60_000))
            .saturating_add(whole_seconds.saturating_mul(1_000))
            .saturating_add(fraction_ms),
    ))
}

#[cfg(test)]
mod tests {
    use super::{TimedLyrics, parse_lrc_pair};

    #[test]
    fn parses_multiple_timestamps_and_prefers_non_empty_translation() {
        let lyrics = parse_lrc_pair(
            "[00:01.2][00:02.00]first\n[00:03.000]second",
            Some("[00:01.200]\n[00:03.000]translated"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(lyrics.lines[0].start_ms, 1_200);
        assert_eq!(lyrics.lines[1].start_ms, 2_000);
        assert_eq!(lyrics.line_at_ms(1_250), Some("first"));
        assert_eq!(lyrics.line_at_ms(3_000), Some("translated"));
    }

    #[test]
    fn ignores_metadata_and_returns_none_for_empty_lyrics() {
        assert_eq!(
            parse_lrc_pair("[ar:artist]\n[ti:title]", None).unwrap(),
            None
        );
    }

    #[test]
    fn supports_hour_timestamps_and_invalid_values_are_rejected() {
        let lyrics = parse_lrc_pair("[01:02:03.45]line", None).unwrap().unwrap();
        assert_eq!(lyrics.lines[0].start_ms, 3_723_450);
        assert!(parse_lrc_pair("[00:60.00]line", None).is_err());
    }

    #[test]
    fn invalid_positions_do_not_produce_a_line() {
        let lyrics = TimedLyrics::new(Vec::new());
        assert!(lyrics.is_none());
    }
}
