use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use image::DynamicImage;
use image::imageops::FilterType;
use serde::Serialize;

use crate::config::{self, OcrConfig, TemplateConfig};
use crate::observation::chat::{
    ResolvedTemplateArgs, TemplateArgs, count_scanned_chat_markers, scan_chat_markers,
};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    UiEvidenceRect, UiMarkerProbeEvidence, UiStateClassification, UiStateClassifier,
    UiStateEvidence, UiStateKind as RuntimeUiStateKind, UiTemplateProbeEvidence,
};
use crate::ui::geometry::{Point, Rect, crop_canvas};
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
    pub(crate) world_wish_template: PathBuf,
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
            world_wish_template: templates.world_wish.clone(),
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
    game_entry: Option<GameEntryDetection>,
    game_fallback: GameFallbackState,
}

impl TemplateUiStateClassifier {
    pub(crate) fn new(templates: ResolvedUiTemplateArgs, screen: config::ScreenConfig) -> Self {
        Self {
            templates,
            screen,
            game_entry: None,
            game_fallback: GameFallbackState::new(2),
        }
    }

    pub(crate) fn with_game_entry_detection(
        mut self,
        ocr: OcrRuntimeHandle,
        gate_region: Rect,
        required_count: u32,
    ) -> Self {
        self.game_entry = Some(GameEntryDetection { ocr, gate_region });
        self.game_fallback = GameFallbackState::new(required_count);
        self
    }

    fn classify_at(&mut self, image: &DynamicImage, now: Instant) -> Result<UiStateClassification> {
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
            match detect_ui_state_with_evidence(image, &self.templates, &self.screen) {
                Ok(result) => result,
                Err(error) => {
                    self.game_fallback.reset();
                    return Err(error);
                }
            };
        let base =
            UiStateClassification::with_evidence(state.runtime_kind(), state.to_string(), evidence);
        if base.kind() != RuntimeUiStateKind::Unknown {
            self.game_fallback.reset();
            return Ok(base);
        }
        self.game_fallback.observe_unknown(now, base, || {
            probe_game_state(
                image,
                &self.templates,
                &self.screen,
                self.game_entry.as_ref(),
            )
        })
    }
}

impl UiStateClassifier for TemplateUiStateClassifier {
    fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification> {
        self.classify_at(image, Instant::now())
    }
}

const GAME_STATE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct GameEntryDetection {
    ocr: OcrRuntimeHandle,
    gate_region: Rect,
}

#[derive(Clone)]
struct GameFallbackState {
    required_count: u32,
    unknown_since: Option<Instant>,
    retry_after: Option<Instant>,
    candidate: Option<RuntimeUiStateKind>,
    candidate_count: u32,
    confirmed: Option<UiStateClassification>,
}

impl GameFallbackState {
    fn new(required_count: u32) -> Self {
        Self {
            required_count: required_count.max(2),
            unknown_since: None,
            retry_after: None,
            candidate: None,
            candidate_count: 0,
            confirmed: None,
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.required_count);
    }

    fn observe_unknown(
        &mut self,
        now: Instant,
        base: UiStateClassification,
        probe: impl FnOnce() -> Result<UiStateClassification>,
    ) -> Result<UiStateClassification> {
        let unknown_since = *self.unknown_since.get_or_insert(now);
        if now.saturating_duration_since(unknown_since) < GAME_STATE_PROBE_INTERVAL
            || self
                .retry_after
                .is_some_and(|retry_after| now < retry_after)
        {
            return Ok(self.confirmed.clone().unwrap_or(base));
        }

        let observation = match probe() {
            Ok(observation) => observation,
            Err(error) => {
                self.candidate = None;
                self.candidate_count = 0;
                self.confirmed = None;
                self.retry_after = Some(now + GAME_STATE_PROBE_INTERVAL);
                return Err(error);
            }
        };
        if observation.kind() == RuntimeUiStateKind::Unknown {
            self.candidate = None;
            self.candidate_count = 0;
            self.confirmed = None;
            self.retry_after = Some(now + GAME_STATE_PROBE_INTERVAL);
            return Ok(observation);
        }

        // 只有新的 OCR/模板探测才能推进确认，缓存画面不能推进。
        if self.candidate == Some(observation.kind()) {
            self.candidate_count = self.candidate_count.saturating_add(1);
        } else {
            if self.candidate.is_some() && self.confirmed.is_none() {
                self.candidate = None;
                self.candidate_count = 0;
                self.retry_after = Some(now + GAME_STATE_PROBE_INTERVAL);
                return Ok(base);
            }
            self.candidate = Some(observation.kind());
            self.candidate_count = 1;
            self.confirmed = None;
        }
        if self.candidate_count >= self.required_count {
            self.confirmed = Some(observation.clone());
            self.retry_after = Some(now + GAME_STATE_PROBE_INTERVAL);
            Ok(observation)
        } else {
            self.retry_after = None;
            Ok(UiStateClassification::with_evidence(
                RuntimeUiStateKind::Unknown,
                "unknown:game_probe",
                observation.evidence().clone(),
            ))
        }
    }
}

fn probe_game_state(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
    game_entry: Option<&GameEntryDetection>,
) -> Result<UiStateClassification> {
    if let Some(game_entry) = game_entry
        && find_enter_game_text(&game_entry.ocr, image, game_entry.gate_region)?.is_some()
    {
        return Ok(UiStateClassification::new(
            RuntimeUiStateKind::GameGate,
            "game_gate",
        ));
    }
    let region = screen.world_wish_rect.into();
    let candidate = best_template_candidate(image, Some(region), &templates.world_wish_template)?;
    let matched = candidate_matches(&candidate, templates.chat_templates.marker_threshold);
    let evidence = UiStateEvidence::new(
        vec![template_probe(
            &templates.world_wish_template,
            region,
            templates.chat_templates.marker_threshold,
            candidate.as_ref(),
        )],
        None,
        if matched {
            "world_wish_template"
        } else {
            "no_game_anchor"
        },
    );
    Ok(UiStateClassification::with_evidence(
        if matched {
            RuntimeUiStateKind::Overworld
        } else {
            RuntimeUiStateKind::Unknown
        },
        if matched { "overworld" } else { "unknown" },
        evidence,
    ))
}

pub(crate) fn find_enter_game_text(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
) -> Result<Option<Point>> {
    let lines = ocr.recognize_lines(crop_canvas(image, region)?, OcrPriority::UiConfirmation)?;
    Ok(lines.into_iter().find_map(|line| {
        let normalized: String = line.text.chars().filter(|ch| !ch.is_whitespace()).collect();
        normalized.contains("点击进入").then(|| {
            Point::new(
                region.x + line.bbox.center().x,
                region.y + line.bbox.center().y,
            )
        })
    }))
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
    let marker_hits = scan_chat_markers(image, &templates.chat_templates, screen.chat_rect)?;
    let (blue, yellow, pink) = count_scanned_chat_markers(&marker_hits);
    let marker_ms = elapsed_ms(marker_started);
    let marker_probe = UiMarkerProbeEvidence::new(
        evidence_rect(screen.chat_rect.into()),
        (image.width(), image.height()),
        blue,
        yellow,
        pink,
        marker_hits,
    );
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};

    fn game_classification(kind: RuntimeUiStateKind) -> UiStateClassification {
        UiStateClassification::new(
            kind,
            match kind {
                RuntimeUiStateKind::GameGate => "game_gate",
                RuntimeUiStateKind::Overworld => "overworld",
                _ => "unknown",
            },
        )
    }

    fn observe_game(
        fallback: &mut GameFallbackState,
        now: Instant,
        kind: RuntimeUiStateKind,
    ) -> UiStateClassification {
        fallback
            .observe_unknown(
                now,
                game_classification(RuntimeUiStateKind::Unknown),
                || Ok(game_classification(kind)),
            )
            .expect("game fallback observation")
    }

    #[test]
    fn game_fallback_delays_probes_confirms_real_results_and_cools_down() {
        let mut fallback = GameFallbackState::new(2);
        let now = Instant::now();
        for elapsed in [0, 4999] {
            let result = fallback
                .observe_unknown(
                    now + Duration::from_millis(elapsed),
                    game_classification(RuntimeUiStateKind::Unknown),
                    || panic!("supplemental probes must wait for sustained unknown"),
                )
                .unwrap();
            assert_eq!(result.kind(), RuntimeUiStateKind::Unknown);
        }
        let first = observe_game(
            &mut fallback,
            now + Duration::from_secs(5),
            RuntimeUiStateKind::Overworld,
        );
        assert_eq!(first.kind(), RuntimeUiStateKind::Unknown);
        assert_eq!(first.label(), "unknown:game_probe");
        let confirmed_at = now + Duration::from_millis(5010);
        assert_eq!(
            observe_game(&mut fallback, confirmed_at, RuntimeUiStateKind::Overworld).kind(),
            RuntimeUiStateKind::Overworld
        );
        let cached = fallback
            .observe_unknown(
                now + Duration::from_secs(8),
                game_classification(RuntimeUiStateKind::Unknown),
                || panic!("confirmed game state must not re-run OCR every frame"),
            )
            .unwrap();
        assert_eq!(cached.kind(), RuntimeUiStateKind::Overworld);
        assert_eq!(
            fallback.candidate_count, 2,
            "cache does not count as a real probe"
        );

        let missed_at = confirmed_at + GAME_STATE_PROBE_INTERVAL;
        assert_eq!(
            observe_game(&mut fallback, missed_at, RuntimeUiStateKind::Unknown).kind(),
            RuntimeUiStateKind::Unknown
        );
        assert!(fallback.confirmed.is_none());
        assert!(
            fallback
                .observe_unknown(
                    missed_at + Duration::from_secs(1),
                    game_classification(RuntimeUiStateKind::Unknown),
                    || panic!("a missed probe also starts the cooldown"),
                )
                .is_ok()
        );
    }

    #[test]
    fn game_fallback_revokes_changed_or_failed_results_and_bounds_unstable_probes() {
        let mut fallback = GameFallbackState::new(2);
        let now = Instant::now();
        observe_game(&mut fallback, now, RuntimeUiStateKind::GameGate);
        observe_game(
            &mut fallback,
            now + Duration::from_secs(5),
            RuntimeUiStateKind::GameGate,
        );
        observe_game(
            &mut fallback,
            now + Duration::from_secs(6),
            RuntimeUiStateKind::GameGate,
        );
        let changed_at = now + Duration::from_secs(11);
        let changed = observe_game(&mut fallback, changed_at, RuntimeUiStateKind::Overworld);
        assert_eq!(changed.label(), "unknown:game_probe");
        assert!(fallback.confirmed.is_none());
        let flapped = observe_game(
            &mut fallback,
            changed_at + Duration::from_millis(10),
            RuntimeUiStateKind::GameGate,
        );
        assert_eq!(flapped.label(), "unknown");
        assert!(
            fallback
                .observe_unknown(
                    changed_at + Duration::from_secs(1),
                    game_classification(RuntimeUiStateKind::Unknown),
                    || panic!("alternating candidates must not cause OCR on every frame"),
                )
                .is_ok()
        );

        let retry_at = now + Duration::from_secs(17);
        observe_game(&mut fallback, retry_at, RuntimeUiStateKind::Overworld);
        observe_game(
            &mut fallback,
            retry_at + Duration::from_millis(10),
            RuntimeUiStateKind::Overworld,
        );
        let failed = fallback.observe_unknown(
            retry_at + Duration::from_secs(6),
            game_classification(RuntimeUiStateKind::Unknown),
            || anyhow::bail!("OCR unavailable"),
        );
        assert!(failed.is_err());
        assert!(fallback.confirmed.is_none());
        fallback.reset();
        assert!(fallback.unknown_since.is_none());
        assert!(fallback.retry_after.is_none());
    }

    struct TestGateOcr {
        calls: Arc<AtomicUsize>,
        text: Arc<Mutex<String>>,
    }

    impl OcrDevice for TestGateOcr {
        fn recognize_lines(&mut self, _: &DynamicImage) -> Result<Vec<OcrLine>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![OcrLine {
                text: self.text.lock().unwrap().clone(),
                confidence: 1.0,
                bbox: Rect::new(10, 20, 80, 30),
            }])
        }
    }

    #[test]
    fn game_gate_ocr_accepts_spaced_exact_text_and_offsets_its_center() {
        let text = Arc::new(Mutex::new("点 击\t进 入".to_string()));
        let ocr = OcrRuntime::start(
            TestGateOcr {
                calls: Arc::new(AtomicUsize::new(0)),
                text: Arc::clone(&text),
            },
            1,
        )
        .unwrap();
        let image = DynamicImage::new_rgb8(400, 200);
        let region = Rect::new(100, 50, 200, 100);
        let point = find_enter_game_text(&ocr.handle(), &image, region)
            .unwrap()
            .unwrap();
        assert_eq!((point.x, point.y), (150, 85));
        *text.lock().unwrap() = "点击进人".to_string();
        assert!(
            find_enter_game_text(&ocr.handle(), &image, region)
                .unwrap()
                .is_none()
        );
        ocr.shutdown().unwrap();
    }

    #[test]
    fn world_wish_fixture_matches_both_backgrounds_and_shifted_icon() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        for (fixture, wish_x) in [
            ("tests/fixtures/ui/world-wish-top-bar.png", 266),
            ("tests/fixtures/ui/world-wish-overworld-top-bar.png", 348),
        ] {
            let bar = image::open(fixture).unwrap();
            let hit = best_template_hit(
                &bar,
                None,
                &config.templates.world_wish,
                config.templates.marker_threshold,
            )
            .unwrap()
            .expect(fixture);
            assert_eq!((hit.x, hit.y), (wish_x as i32 + 3, 13), "{fixture}");

            let wish = bar.crop_imm(wish_x, 0, 82, 100).to_rgba8();
            let neighbor = bar.crop_imm(wish_x + 82, 0, 82, 100).to_rgba8();
            let mut shifted = bar.to_rgba8();
            image::imageops::replace(&mut shifted, &neighbor, i64::from(wish_x), 0);
            image::imageops::replace(&mut shifted, &wish, i64::from(wish_x + 82), 0);
            let shifted_hit = best_template_hit(
                &DynamicImage::ImageRgba8(shifted),
                None,
                &config.templates.world_wish,
                config.templates.marker_threshold,
            )
            .unwrap()
            .expect(fixture);
            assert_eq!(
                (shifted_hit.x, shifted_hit.y),
                (wish_x as i32 + 85, 13),
                "{fixture}"
            );
        }
    }

    #[test]
    fn classifier_keeps_chat_priority_and_probes_game_states_only_after_unknown_grace() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let calls = Arc::new(AtomicUsize::new(0));
        let text = Arc::new(Mutex::new("点击进入".to_string()));
        let ocr = OcrRuntime::start(
            TestGateOcr {
                calls: Arc::clone(&calls),
                text: Arc::clone(&text),
            },
            1,
        )
        .unwrap();
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone())
            .with_game_entry_detection(ocr.handle(), Rect::new(500, 800, 500, 100), 2);
        let secondary =
            image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg").unwrap();
        let bar = image::open("tests/fixtures/ui/world-wish-top-bar.png").unwrap();
        let mut world = image::RgbaImage::new(1920, 1080);
        image::imageops::replace(&mut world, &bar.to_rgba8(), 1300, 0);
        let world = DynamicImage::ImageRgba8(world);
        let now = Instant::now();
        for elapsed in [0, 10] {
            assert_eq!(
                classifier
                    .classify_at(&secondary, now + Duration::from_secs(elapsed))
                    .unwrap()
                    .kind(),
                RuntimeUiStateKind::Secondary
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(20))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::Unknown
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(25))
                .unwrap()
                .label(),
            "unknown:game_probe"
        );
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(26))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::GameGate,
            "gate OCR has priority over the wish template"
        );
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(27))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::GameGate
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        *text.lock().unwrap() = String::new();
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(31))
                .unwrap()
                .label(),
            "unknown:game_probe"
        );
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(32))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::Overworld
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            classifier
                .classify_at(&secondary, now + Duration::from_secs(33))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::Secondary
        );
        assert!(classifier.game_fallback.confirmed.is_none());
        assert_eq!(
            classifier
                .classify_at(&world, now + Duration::from_secs(34))
                .unwrap()
                .kind(),
            RuntimeUiStateKind::Unknown
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        classifier.screen.friend_rect.width = 3000;
        assert!(
            classifier
                .classify_at(&world, now + Duration::from_secs(40))
                .is_err()
        );
        assert!(classifier.game_fallback.unknown_since.is_none());
        ocr.shutdown().unwrap();
    }

    #[test]
    fn friend_anchor_uses_primary_friend_status() {
        let state = UiState::primary_friend();

        assert_eq!(state.to_string(), "primary:friend");
        assert!(state.friend_visible);
        assert_eq!(state.source, "friend");
    }

    #[test]
    fn fixed_scrolled_friend_list_uses_the_back_anchor_for_secondary_state() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml"))
            .expect("load default config");
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
