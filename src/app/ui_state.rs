use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use serde::Serialize;

use super::ResolvedUiTemplateArgs;
use super::chat_scan::count_chat_markers;
use super::config;
use super::template_match::best_template_hit;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UiStateKind {
    Primary,
    Secondary,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UiState {
    state: UiStateKind,
    blue_count: usize,
    yellow_count: usize,
    pink_count: usize,
    hall_visible: bool,
    friend_visible: bool,
    source: &'static str,
}

impl UiState {
    fn primary_friend() -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            hall_visible: false,
            friend_visible: true,
            source: "friend",
        }
    }

    fn primary_marker(blue_count: usize, yellow_count: usize, pink_count: usize) -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count,
            yellow_count,
            pink_count,
            hall_visible: false,
            friend_visible: false,
            source: "marker",
        }
    }

    fn secondary_hall() -> Self {
        Self {
            state: UiStateKind::Secondary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            hall_visible: true,
            friend_visible: false,
            source: "hall",
        }
    }

    fn unknown() -> Self {
        Self {
            state: UiStateKind::Unknown,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            hall_visible: false,
            friend_visible: false,
            source: "none",
        }
    }

    pub(super) fn is_primary(&self) -> bool {
        self.state == UiStateKind::Primary
    }

    pub(super) fn is_secondary(&self) -> bool {
        self.state == UiStateKind::Secondary
    }
}

impl std::fmt::Display for UiState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.state {
            UiStateKind::Primary if self.source == "friend" => {
                write!(formatter, "primary:friend")
            }
            UiStateKind::Primary => write!(
                formatter,
                "primary:marker blue={} yellow={} pink={}",
                self.blue_count, self.yellow_count, self.pink_count
            ),
            UiStateKind::Secondary => write!(formatter, "secondary:hall"),
            UiStateKind::Unknown => write!(formatter, "unknown"),
        }
    }
}

pub(super) fn detect_ui_state(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<UiState> {
    let started = Instant::now();
    let friend_started = Instant::now();
    if best_template_hit(
        image,
        Some(screen.friend_rect.into()),
        &templates.friend_template,
        templates.chat_templates.marker_threshold,
    )?
    .is_some()
    {
        let friend_ms = elapsed_ms(friend_started);
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms hall=0ms marker=0ms state=primary_friend",
            elapsed_ms(started),
            friend_ms
        );
        return Ok(UiState::primary_friend());
    }
    let friend_ms = elapsed_ms(friend_started);

    let hall_started = Instant::now();
    if best_template_hit(
        image,
        Some(screen.secondary_hall_rect.into()),
        &templates.secondary_hall_template,
        templates.chat_templates.marker_threshold,
    )?
    .is_some()
    {
        let hall_ms = elapsed_ms(hall_started);
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms hall={}ms marker=0ms state=secondary_hall",
            elapsed_ms(started),
            friend_ms,
            hall_ms
        );
        return Ok(UiState::secondary_hall());
    }
    let hall_ms = elapsed_ms(hall_started);

    let marker_started = Instant::now();
    let (blue, yellow, pink) =
        count_chat_markers(image, &templates.chat_templates, screen.chat_rect)?;
    let marker_ms = elapsed_ms(marker_started);
    if blue + yellow + pink > 0 {
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms hall={}ms marker={}ms state=primary_marker blue={} yellow={} pink={}",
            elapsed_ms(started),
            friend_ms,
            hall_ms,
            marker_ms,
            blue,
            yellow,
            pink
        );
        return Ok(UiState::primary_marker(blue, yellow, pink));
    }

    log::info!(target: "timing",
        "UI 状态检测耗时: total={}ms friend={}ms hall={}ms marker={}ms state=unknown",
        elapsed_ms(started),
        friend_ms,
        hall_ms,
        marker_ms
    );
    Ok(UiState::unknown())
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friend_anchor_uses_primary_friend_status() {
        let state = UiState::primary_friend();

        assert_eq!(state.to_string(), "primary:friend");
        assert!(state.friend_visible);
        assert_eq!(state.source, "friend");
    }
}
