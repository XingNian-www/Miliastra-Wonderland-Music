use std::collections::HashMap;

use super::command;

pub(super) const HALL_INFO_OCR_SAMPLES: usize = 3;
const HALL_TIME_RECOGNITION_TOLERANCE_MINUTES: u32 = 1;

#[derive(Clone, Debug)]
pub(super) struct HallInfo {
    pub(super) name: String,
    pub(super) remaining_minutes: Option<u32>,
}

#[derive(Clone, Debug)]
pub(super) struct HallInfoSample {
    pub(super) name: String,
    pub(super) time_text: String,
    pub(super) remaining_minutes: Option<u32>,
}

pub(super) fn parse_hall_remaining_minutes(text: &str) -> Option<u32> {
    let digits = text
        .chars()
        .filter_map(normalize_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let minutes = digits.parse::<u32>().ok()?;
    if (1..=180).contains(&minutes) {
        Some(minutes.saturating_sub(HALL_TIME_RECOGNITION_TOLERANCE_MINUTES))
    } else {
        None
    }
}

pub(super) fn merge_hall_info_samples(samples: &[HallInfoSample]) -> HallInfo {
    let name = most_frequent_hall_name(samples).unwrap_or_else(|| {
        samples
            .first()
            .map(|sample| sample.name.clone())
            .unwrap_or_default()
    });
    let is_public_hall = samples
        .iter()
        .filter(|sample| {
            command::normalize_lock_text(&sample.name) == command::normalize_lock_text("公共大厅")
        })
        .count()
        * 2
        >= samples.len().max(1);
    let remaining_minutes = if is_public_hall {
        None
    } else {
        most_frequent_hall_minutes(samples)
    };
    HallInfo {
        name,
        remaining_minutes,
    }
}

fn most_frequent_hall_name(samples: &[HallInfoSample]) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in samples
        .iter()
        .map(|sample| sample.name.trim())
        .filter(|name| !name.is_empty())
    {
        *counts.entry(name.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| left.0.len().cmp(&right.0.len()))
                .then_with(|| right.0.cmp(&left.0))
        })
        .map(|(name, _)| name)
}

fn most_frequent_hall_minutes(samples: &[HallInfoSample]) -> Option<u32> {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for minutes in samples.iter().filter_map(|sample| sample.remaining_minutes) {
        *counts.entry(minutes).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(minutes, _)| minutes)
}

pub(super) fn display_or_empty(text: &str) -> &str {
    if text.is_empty() { "空" } else { text }
}

fn normalize_ascii_digit(ch: char) -> Option<char> {
    if ch.is_ascii_digit() {
        return Some(ch);
    }
    if ('\u{ff10}'..='\u{ff19}').contains(&ch) {
        return char::from_u32(ch as u32 - 0xfee0);
    }
    None
}

pub(super) fn format_hall_remaining_suffix(minutes: Option<u32>) -> String {
    minutes
        .map(|value| format!("，剩余{}分钟", value))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hall_remaining_minutes_with_tolerance() {
        assert_eq!(parse_hall_remaining_minutes("剩余10分钟"), Some(9));
        assert_eq!(parse_hall_remaining_minutes("剩余９分钟"), Some(8));
        assert_eq!(parse_hall_remaining_minutes("剩余1分钟"), Some(0));
    }

    #[test]
    fn rejects_invalid_hall_remaining_minutes() {
        assert_eq!(parse_hall_remaining_minutes("公共大厅"), None);
        assert_eq!(parse_hall_remaining_minutes("剩余181分钟"), None);
    }
}
