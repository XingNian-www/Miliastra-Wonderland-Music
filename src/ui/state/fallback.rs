//! 未知态回退探针：`GameFallbackState`、游戏入口 OCR 与祈愿顶栏探针。

use crate::config;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::crop_canvas;
use crate::runtime::ocr::OcrPriority;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::ui::UiStateClassification;
use crate::runtime::ui::UiStateEvidence;
use crate::runtime::ui::UiStateKind as RuntimeUiStateKind;
use crate::ui::template::best_template_candidate;
use anyhow::Result;
use image::DynamicImage;
use std::time::Duration;
use std::time::Instant;

use super::anchors::{qianxing_map_anchor_rect, qianxing_map_label_via_ocr};
use super::args::ResolvedUiTemplateArgs;
use super::diagnostic::{candidate_matches, template_probe};
use super::world::world_is_teyvat_in;

pub(super) const GAME_STATE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct GameEntryDetection {
    pub(super) ocr: OcrRuntimeHandle,
    pub(super) gate_region: Rect,
}

#[derive(Clone)]
pub(super) struct GameFallbackState {
    pub(super) required_count: u32,
    pub(super) unknown_since: Option<Instant>,
    pub(super) retry_after: Option<Instant>,
    pub(super) candidate: Option<RuntimeUiStateKind>,
    pub(super) candidate_count: u32,
    pub(super) confirmed: Option<UiStateClassification>,
}

impl GameFallbackState {
    pub(super) fn new(required_count: u32) -> Self {
        Self {
            required_count: required_count.max(2),
            unknown_since: None,
            retry_after: None,
            candidate: None,
            candidate_count: 0,
            confirmed: None,
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new(self.required_count);
    }

    pub(super) fn observe_unknown(
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

/// 离线 fallback 探针入口：`game_entry` 固定为 None（OCR 运行时不可用）。
///
/// 单独开这个薄包装是为了**不把 `GameEntryDetection` 暴露出去**——
/// 调用方只需要一个「在没有 OCR 的情况下探 `world-wish`」的能力。
pub(crate) fn probe_game_state_without_entry(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<UiStateClassification> {
    probe_game_state(image, templates, screen, None)
}

pub(super) fn probe_game_state(
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
    let wish_matched = candidate_matches(&candidate, templates.chat_templates.marker_threshold);
    // 祈愿顶栏命中**还不够**：千星大厅的多人面板也会命中它（第 59 节实测）。
    // 再叠一道正交的世界判据——必须「有 HUD 且判为提瓦特」才算大世界。
    let world_is_teyvat = wish_matched && world_is_teyvat_in(image, screen.hall_bar_rect.into());
    let rule = match (wish_matched, world_is_teyvat) {
        (true, true) => "world_wish_template",
        // 明确区分「顶栏命中但世界判据否掉」：这条规则名让误判在日志里一眼可见，
        // 不会再伪装成正常的大世界判定。
        (true, false) => "world_wish_without_hud_bottom_bar",
        (false, _) => "no_game_anchor",
    };
    let evidence = UiStateEvidence::new(
        vec![template_probe(
            &templates.world_wish_template,
            region,
            templates.chat_templates.marker_threshold,
            candidate.as_ref(),
        )],
        None,
        rule,
    );
    // **可选 OCR 后备**（不参与常规路径）：模板锚点没命中时（例如标签被缩放 /
    // 抗锯齿改写导致形状分掉到阈值以下），再用同一块右下角 ROI 的文本确认一次。
    // 只在回退探针里跑（`Unknown` 且每 `GAME_STATE_PROBE_INTERVAL` 一次），
    // 不给每帧分类增加 OCR 负担；离线门禁与单测走 `game_entry = None` 分支，
    // 模板路径完全不依赖 OCR。
    if !world_is_teyvat
        && let Some(game_entry) = game_entry
        && let Some(region) = qianxing_map_anchor_rect(image)
        && qianxing_map_label_via_ocr(&game_entry.ocr, image, region)?
    {
        return Ok(UiStateClassification::new(
            RuntimeUiStateKind::Secondary,
            "secondary:qianxing_map",
        ));
    }
    Ok(UiStateClassification::with_evidence(
        if world_is_teyvat {
            RuntimeUiStateKind::Overworld
        } else {
            RuntimeUiStateKind::Unknown
        },
        if world_is_teyvat {
            "overworld"
        } else {
            "unknown"
        },
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
