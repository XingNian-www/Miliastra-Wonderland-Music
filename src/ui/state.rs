use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use serde::Serialize;

use crate::config::{self, OcrConfig, TemplateConfig};
use crate::observation::chat::{ResolvedTemplateArgs, TemplateArgs, count_chat_markers};
use crate::ui::template::best_template_hit;

#[derive(Clone, Debug, Default)]
pub(crate) struct UiTemplateArgs {
    friend_template: Option<PathBuf>,
    secondary_back_template: Option<PathBuf>,
    chat_templates: TemplateArgs,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedUiTemplateArgs {
    pub(crate) friend_template: PathBuf,
    pub(crate) secondary_back_template: PathBuf,
    pub(crate) chat_templates: ResolvedTemplateArgs,
}

impl UiTemplateArgs {
    pub(crate) fn resolve(
        &self,
        templates: &TemplateConfig,
        ocr: &OcrConfig,
    ) -> ResolvedUiTemplateArgs {
        ResolvedUiTemplateArgs {
            friend_template: self
                .friend_template
                .clone()
                .unwrap_or_else(|| templates.friend.clone()),
            secondary_back_template: self
                .secondary_back_template
                .clone()
                .unwrap_or_else(|| templates.secondary_back.clone()),
            chat_templates: self.chat_templates.resolve(templates, ocr),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UiStateKind {
    Primary,
    Secondary,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UiState {
    state: UiStateKind,
    blue_count: usize,
    yellow_count: usize,
    pink_count: usize,
    secondary_visible: bool,
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
            secondary_visible: false,
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
            secondary_visible: false,
            friend_visible: false,
            source: "marker",
        }
    }

    fn secondary_chat() -> Self {
        Self {
            state: UiStateKind::Secondary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: true,
            friend_visible: false,
            source: "back",
        }
    }

    fn unknown() -> Self {
        Self {
            state: UiStateKind::Unknown,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "none",
        }
    }

    pub(crate) fn is_primary(&self) -> bool {
        self.state == UiStateKind::Primary
    }

    pub(crate) fn is_secondary(&self) -> bool {
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
            UiStateKind::Secondary => write!(formatter, "secondary:chat"),
            UiStateKind::Unknown => write!(formatter, "unknown"),
        }
    }
}

pub(crate) fn detect_ui_state(
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
            "UI 状态检测耗时: total={}ms friend={}ms back=0ms marker=0ms state=primary_friend",
            elapsed_ms(started),
            friend_ms
        );
        return Ok(UiState::primary_friend());
    }
    let friend_ms = elapsed_ms(friend_started);

    let back_started = Instant::now();
    if best_template_hit(
        image,
        Some(screen.secondary_back_rect.into()),
        &templates.secondary_back_template,
        templates.chat_templates.marker_threshold,
    )?
    .is_some()
    {
        let back_ms = elapsed_ms(back_started);
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms back={}ms marker=0ms state=secondary_chat",
            elapsed_ms(started),
            friend_ms,
            back_ms
        );
        return Ok(UiState::secondary_chat());
    }
    let back_ms = elapsed_ms(back_started);

    let marker_started = Instant::now();
    let (blue, yellow, pink) =
        count_chat_markers(image, &templates.chat_templates, screen.chat_rect)?;
    let marker_ms = elapsed_ms(marker_started);
    if blue + yellow + pink > 0 {
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms back={}ms marker={}ms state=primary_marker blue={} yellow={} pink={}",
            elapsed_ms(started),
            friend_ms,
            back_ms,
            marker_ms,
            blue,
            yellow,
            pink
        );
        return Ok(UiState::primary_marker(blue, yellow, pink));
    }

    log::info!(target: "timing",
        "UI 状态检测耗时: total={}ms friend={}ms back={}ms marker={}ms state=unknown",
        elapsed_ms(started),
        friend_ms,
        back_ms,
        marker_ms
    );
    Ok(UiState::unknown())
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn friend_anchor_uses_primary_friend_status() {
        let state = UiState::primary_friend();

        assert_eq!(state.to_string(), "primary:friend");
        assert!(state.friend_visible);
        assert_eq!(state.source, "friend");
    }

    #[test]
    fn back_anchor_identifies_secondary_chat_without_the_hall_row() {
        let state = UiState::secondary_chat();

        assert_eq!(state.to_string(), "secondary:chat");
        assert!(state.secondary_visible);
        assert_eq!(state.source, "back");
    }

    #[test]
    fn fixed_scrolled_friend_list_uses_the_back_anchor_for_secondary_state() {
        let config = AppConfig::load(Path::new("config.yaml")).expect("load default config");
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");
        assert_eq!((image.width(), image.height()), (1920, 1080));

        let hall_hit = best_template_hit(
            &image,
            Some(config.screen.secondary_hall_rect.into()),
            &config.templates.secondary_hall,
            config.templates.marker_threshold,
        )
        .expect("match hall template");
        assert!(
            hall_hit.is_none(),
            "scrolled list must not depend on the hall row"
        );

        let back_hit = best_template_hit(
            &image,
            Some(config.screen.secondary_back_rect.into()),
            &config.templates.secondary_back,
            config.templates.marker_threshold,
        )
        .expect("match secondary back template");
        assert!(
            back_hit.is_some(),
            "secondary state requires the fixed back anchor"
        );

        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        for _ in 0..2 {
            let state = detect_ui_state(&image, &templates, &config.screen)
                .expect("detect fixed secondary-chat screenshot");

            assert_eq!(state.to_string(), "secondary:chat");
            assert!(state.is_secondary());
        }
    }
}
