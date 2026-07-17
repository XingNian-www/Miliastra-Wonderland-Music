use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use image::imageops::FilterType;
use serde::Serialize;

use crate::config::{self, OcrConfig, TemplateConfig};
use crate::observation::chat::{ResolvedTemplateArgs, TemplateArgs, count_chat_markers};
use crate::runtime::ui::{
    UiEvidenceRect, UiMarkerProbeEvidence, UiStateClassification, UiStateClassifier,
    UiStateEvidence, UiStateKind as RuntimeUiStateKind, UiTemplateProbeEvidence,
};
use crate::ui::geometry::Rect;
#[cfg(test)]
use crate::ui::template::best_template_hit;
use crate::ui::template::{TemplateHit, best_template_candidate};

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

    fn runtime_kind(&self) -> RuntimeUiStateKind {
        match self.state {
            UiStateKind::Primary => RuntimeUiStateKind::Primary,
            UiStateKind::Secondary => RuntimeUiStateKind::Secondary,
            UiStateKind::Unknown => RuntimeUiStateKind::Unknown,
        }
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

#[derive(Clone)]
pub(crate) struct TemplateUiStateClassifier {
    templates: ResolvedUiTemplateArgs,
    screen: config::ScreenConfig,
}

impl TemplateUiStateClassifier {
    pub(crate) fn new(templates: ResolvedUiTemplateArgs, screen: config::ScreenConfig) -> Self {
        Self { templates, screen }
    }
}

impl UiStateClassifier for TemplateUiStateClassifier {
    fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification> {
        let normalized;
        let image = if image.width() == self.screen.expected_width
            && image.height() == self.screen.expected_height
        {
            image
        } else {
            normalized = image.resize_exact(
                self.screen.expected_width,
                self.screen.expected_height,
                FilterType::Triangle,
            );
            &normalized
        };
        let (state, evidence) =
            detect_ui_state_with_evidence(image, &self.templates, &self.screen)?;
        Ok(UiStateClassification::with_evidence(
            state.runtime_kind(),
            state.to_string(),
            evidence,
        ))
    }
}

#[cfg(test)]
fn detect_ui_state(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<UiState> {
    Ok(detect_ui_state_with_evidence(image, templates, screen)?.0)
}

fn detect_ui_state_with_evidence(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<(UiState, UiStateEvidence)> {
    let started = Instant::now();
    let threshold = templates.chat_templates.marker_threshold;
    let mut template_probes = Vec::with_capacity(2);

    let friend_started = Instant::now();
    let friend_region: Rect = screen.friend_rect.into();
    let friend_candidate =
        best_template_candidate(image, Some(friend_region), &templates.friend_template)?;
    let friend_visible = candidate_matches(&friend_candidate, threshold);
    template_probes.push(template_probe(
        &templates.friend_template,
        friend_region,
        threshold,
        friend_candidate.as_ref(),
    ));
    if friend_visible {
        let friend_ms = elapsed_ms(friend_started);
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms back=0ms marker=0ms state=primary_friend",
            elapsed_ms(started),
            friend_ms
        );
        return Ok((
            UiState::primary_friend(),
            UiStateEvidence::new(template_probes, None, "primary_friend_template"),
        ));
    }
    let friend_ms = elapsed_ms(friend_started);

    let back_started = Instant::now();
    let back_region: Rect = screen.secondary_back_rect.into();
    let back_candidate =
        best_template_candidate(image, Some(back_region), &templates.secondary_back_template)?;
    let back_visible = candidate_matches(&back_candidate, threshold);
    template_probes.push(template_probe(
        &templates.secondary_back_template,
        back_region,
        threshold,
        back_candidate.as_ref(),
    ));
    if back_visible {
        let back_ms = elapsed_ms(back_started);
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms friend={}ms back={}ms marker=0ms state=secondary_chat",
            elapsed_ms(started),
            friend_ms,
            back_ms
        );
        return Ok((
            UiState::secondary_chat(),
            UiStateEvidence::new(template_probes, None, "secondary_back_template"),
        ));
    }
    let back_ms = elapsed_ms(back_started);

    let marker_started = Instant::now();
    let (blue, yellow, pink) =
        count_chat_markers(image, &templates.chat_templates, screen.chat_rect)?;
    let marker_ms = elapsed_ms(marker_started);
    let marker_probe =
        UiMarkerProbeEvidence::new(evidence_rect(screen.chat_rect.into()), blue, yellow, pink);
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
        return Ok((
            UiState::primary_marker(blue, yellow, pink),
            UiStateEvidence::new(template_probes, Some(marker_probe), "primary_chat_markers"),
        ));
    }

    log::info!(target: "timing",
        "UI 状态检测耗时: total={}ms friend={}ms back={}ms marker={}ms state=unknown",
        elapsed_ms(started),
        friend_ms,
        back_ms,
        marker_ms
    );
    Ok((
        UiState::unknown(),
        UiStateEvidence::new(template_probes, Some(marker_probe), "no_reliable_anchor"),
    ))
}

fn candidate_matches(candidate: &Option<TemplateHit>, threshold: f32) -> bool {
    candidate
        .as_ref()
        .is_some_and(|candidate| candidate.score >= threshold)
}

fn template_probe(
    template: &std::path::Path,
    search_rect: Rect,
    threshold: f32,
    candidate: Option<&TemplateHit>,
) -> UiTemplateProbeEvidence {
    let hit = candidate.filter(|candidate| candidate.score >= threshold);
    let outcome = match (candidate, hit) {
        (_, Some(_)) => "matched",
        (Some(_), None) => "below_threshold",
        (None, None) => "template_not_comparable",
    };
    UiTemplateProbeEvidence::new(
        template.display().to_string(),
        evidence_rect(search_rect),
        candidate.map(|candidate| candidate.score),
        threshold,
        hit.map(|hit| evidence_rect(hit.rect())),
        outcome,
    )
}

fn evidence_rect(rect: Rect) -> UiEvidenceRect {
    UiEvidenceRect::new(rect.x, rect.y, rect.width, rect.height)
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
            assert_eq!(state.runtime_kind(), RuntimeUiStateKind::Secondary);
        }

        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier
            .classify(&image)
            .expect("classify fixed secondary-chat screenshot");
        let evidence = classification.evidence();
        assert_eq!(evidence.final_rule(), "secondary_back_template");
        assert_eq!(evidence.template_probes().len(), 2);
        assert!(
            evidence
                .template_probes()
                .iter()
                .all(|probe| probe.best_score().is_some())
        );
        assert!(evidence.template_probes()[0].hit_rect().is_none());
        assert!(evidence.template_probes()[1].hit_rect().is_some());
    }
}
