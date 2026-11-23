//! 状态分类单元测试与合成帧夹具。
//!
//! 模块路径必须保持 `ui::state::tests`：`scripts/corpus-gate.sh` 精确执行
//! `ui::state::tests::corpus_manifest_confirmed_classification_gate`。

use crate::config;
use crate::config::AppConfig;
use crate::geometry::Rect;
use crate::runtime::ocr::OcrDevice;
use crate::runtime::ocr::OcrLine;
use crate::runtime::ocr::OcrRuntime;
use crate::runtime::ui::UiEvidenceRect;
use crate::runtime::ui::UiStateClassification;
use crate::runtime::ui::UiStateClassifier;
use crate::runtime::ui::UiStateKind;
use crate::ui::frame::Canvas;
use crate::ui::frame::resize_to_canvas;
use crate::ui::template::best_template_candidate_downscaled;
#[cfg(test)]
use crate::ui::template::best_template_hit;
use anyhow::Result;
use image::DynamicImage;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

// 子模块先于 `super::*` 展开：`super::*` 只补模块根部的公开名字，
// 内部实现必须显式从各自子模块取。
use super::anchors::*;
use super::args::*;
use super::classifier::*;
use super::classify::*;
use super::diagnostic::*;
use super::fallback::*;
use super::hall::*;
use super::paimon::*;
use super::qianxing::*;
use super::regions::*;
use super::result::*;
use super::sampling::*;
use super::world::*;

#[cfg(test)]
/// `game_gate` / `panel:<label>`；负样本写 `!<同上>`。
#[test]
#[ignore = "证据画廊数据导出：需要 MILIASTRA_GALLERY_FRAMES / MILIASTRA_GALLERY_OUT"]
fn dump_ui_state_evidence_for_gallery() {
    let Ok(list_path) = std::env::var("MILIASTRA_GALLERY_FRAMES") else {
        return;
    };
    let out_path = std::env::var("MILIASTRA_GALLERY_OUT").expect("MILIASTRA_GALLERY_OUT 必须给出");
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let panels = panel_regions_from(&config);
    let canvas = Canvas {
        width: config.screen.expected_width,
        height: config.screen.expected_height,
        resize: true,
    };
    let hall_bar_rect: Rect = config.screen.hall_bar_rect.into();
    let list = std::fs::read_to_string(&list_path)
        .unwrap_or_else(|error| panic!("读不到 {list_path}: {error}"));
    let mut output = String::new();
    let mut count = 0_usize;
    for (line_number, line) in list.lines().enumerate() {
        if line_number == 0 || line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            columns.len(),
            4,
            "画廊帧清单第 {} 行列数不是 4: {line:?}",
            line_number + 1
        );
        let (surface_key, role, frame, expected) = (columns[0], columns[1], columns[2], columns[3]);
        let source = image::open(frame).unwrap_or_else(|error| panic!("打不开 {frame}: {error}"));
        let normalized = resize_to_canvas(&source, &canvas, None);
        let image = normalized.as_ref().unwrap_or(&source);
        // 每帧独立分类器：避免 game_fallback 的跨帧状态污染单帧证据。
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panels);
        let classification = classifier
            .classify_at(image, Instant::now())
            .unwrap_or_else(|error| panic!("{frame} 分类失败: {error}"));
        let evidence = classification.evidence();
        let probes: Vec<serde_json::Value> = evidence
            .template_probes()
            .iter()
            .map(|probe| {
                serde_json::json!({
                    "template": probe.template(),
                    "search_rect": gallery_evidence_rect_json(probe.search_rect()),
                    "score": probe.best_score(),
                    "threshold": probe.threshold(),
                    "hit_rect": probe.hit_rect().map(gallery_evidence_rect_json),
                    "outcome": probe.outcome(),
                })
            })
            .collect();
        let marker = evidence.marker_probe().map(|marker| {
            serde_json::json!({
                "search_rect": gallery_evidence_rect_json(marker.search_rect()),
                "blue": marker.blue_count(),
                "yellow": marker.yellow_count(),
                "pink": marker.pink_count(),
                "threshold": templates.chat_templates.marker_threshold,
            })
        });
        let world = evidence.world_probe().map(|world| {
            serde_json::json!({
                "region": gallery_evidence_rect_json(world.region()),
                "count": world.count(),
                "teyvat_max": world.teyvat_max(),
                "qianxing_min": world.qianxing_min(),
                "guess": world.guess(),
            })
        });
        let exact = hall_bar_signature(image, hall_bar_rect);
        let near = hall_bar_signature_with_threshold(image, hall_bar_rect, HALL_BAR_WHITE_NEAR);
        let (dark, white, dominant) = screen_tone(image);
        let (center160, center480) = loading_center_dark_ratios(image);
        let grid = paimon_menu_grid_structure(image);
        let panel_distance = qianxing_player_panel_distance(image);
        let close_ratios = gallery_overlay_close_ratios(image);
        let surface = crate::ui::routines::test_surface_of(
            classification.kind(),
            classification.label(),
            evidence.final_rule(),
        );
        let record = serde_json::json!({
            "surface_key": surface_key,
            "role": role,
            "frame": frame,
            "expected": expected,
            "kind": classification.kind().name(),
            "label": classification.label(),
            "final_rule": evidence.final_rule(),
            "surface_debug": format!("{surface:?}"),
            "surface_chinese": surface.chinese_name(),
            "image": {"w": image.width(), "h": image.height()},
            "probes": probes,
            "panel_scores": gallery_panel_probe_scores(image, &templates, panels),
            "marker": marker,
            "world": world,
            "metrics": {
                "hall_bar_exact": {
                    "tiles": exact.0,
                    "min_height_px": exact.1,
                    "max_height_px": exact.2,
                    "top_spread_px": exact.3,
                    "looks_like_hall_keys": hall_bar_looks_like_hall_keys(exact.0, exact.1, exact.2, exact.3),
                },
                "hall_bar_near_white": {
                    "tiles": near.0,
                    "min_height_px": near.1,
                    "max_height_px": near.2,
                    "top_spread_px": near.3,
                    "looks_like_hall_keys": hall_bar_looks_like_hall_keys(near.0, near.1, near.2, near.3),
                },
                "paimon_grid": grid.map(|(tiles, rows, cols)| serde_json::json!({
                    "tiles": tiles, "rows": rows, "cols": cols,
                    "min_tiles": PAIMON_GRID_MIN_TILES,
                    "min_cols": PAIMON_GRID_MIN_COLS,
                    "min_rows": PAIMON_GRID_MIN_ROWS,
                })),
                "screen_tone": {"dark": dark, "white": white, "dominant": dominant},
                "loading_center": {"center160_dark": center160, "center480_dark": center480},
                "loading_rule": loading_rule(image),
                "qianxing_map_anchor": gallery_qianxing_map_anchor(image, &templates),
                "qianxing_panel_distance": panel_distance,
                "qianxing_panel_max": QIANXING_PANEL_MAX,
                "overlay_close": close_ratios.map(|(blue, white)| serde_json::json!({
                    "blue_ratio": blue,
                    "white_ratio": white,
                    "blue_gate": 0.20,
                    "white_threshold": 0.35,
                    "hit": is_qianxing_overlay_close(image),
                })),
                "world_is_teyvat": world_is_teyvat_in(image, hall_bar_rect),
                "beige_panel_fraction": beige_panel_fraction(image),
                "anchor_threshold": templates.chat_templates.marker_threshold.max(0.95),
            },
            "rects": {
                "chat": gallery_rect_json(config.screen.chat_rect.into()),
                "secondary_back": gallery_rect_json(config.screen.secondary_back_rect.into()),
                "secondary_chat_submit": gallery_rect_json(config.screen.secondary_chat_submit_rect.into()),
                "main_menu": gallery_rect_json(config.screen.main_menu_rect.into()),
                "hall_bar": gallery_rect_json(hall_bar_rect),
                "world_wish": gallery_rect_json(config.screen.world_wish_rect.into()),
                "panel_friend_list": gallery_rect_json(panels.friend_panel),
                "panel_search": gallery_rect_json(panels.friend_search_panel),
                "panel_block_chat": gallery_rect_json(panels.friend_block_chat),
                "panel_confirm": gallery_rect_json(panels.confirm_button),
                "panel_confirm_message": gallery_rect_json(panels.confirm_message),
                "panel_view_star": gallery_rect_json(panels.invite_view_star),
                "panel_goto_hall": gallery_rect_json(panels.invite_goto_hall),
                "panel_enter_hall": gallery_rect_json(panels.invite_enter_hall),
            },
            "threshold": templates.chat_templates.marker_threshold,
        });
        output.push_str(&serde_json::to_string(&record).unwrap());
        output.push('\n');
        count += 1;
    }
    std::fs::write(&out_path, output).unwrap_or_else(|error| panic!("写不到 {out_path}: {error}"));
    eprintln!("证据画廊：已导出 {count} 帧 -> {out_path}");
}

// ── 单帧 UI 检测报告（只读，#[ignore]） ─────────────────────────────
//
// 给 `scripts/ui-report.py` 用：读一张截图，跑**生产分类器**与生产匹配函数，
// 把 kind/label/final_rule、生产探针、诊断探针（逐模板分数/阈值/命中框）、
// 结构判据测量值、OCR 区域、耗时原样导出为 JSON。
//
// 诊断探针只是把生产匹配函数再跑一遍用于展示，**不参与判定**，也不写回分类结果；
// 这里没有任何新的锚点或阈值。

#[cfg(test)]
fn report_probe_json(
    image: &DynamicImage,
    label: &str,
    template: &PathBuf,
    region: Rect,
    threshold: f32,
    downscale: u32,
) -> serde_json::Value {
    let candidate = best_template_candidate_downscaled(image, Some(region), template, downscale)
        .ok()
        .flatten();
    serde_json::json!({
        "label": label,
        "template": template.display().to_string(),
        "search_rect": gallery_rect_json(region),
        "score": candidate.as_ref().map(|hit| hit.score),
        "threshold": threshold,
        "hit_rect": candidate
            .as_ref()
            .filter(|hit| hit.quality_ok)
            .map(|hit| gallery_rect_json(hit.rect())),
        "pass": candidate
            .as_ref()
            .is_some_and(|hit| hit.quality_ok && hit.score >= threshold),
        "downscale": downscale,
        "source": "diagnostic",
    })
}

#[cfg(test)]
fn gallery_overlay_close_ratios(image: &DynamicImage) -> Option<(f32, f32)> {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    if width < FULL_FRAME_MIN_WIDTH || height < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let mut total_blue = 0_u32;
    let mut total_samples = 0_u32;
    let mut yy = 0;
    while yy < height {
        let mut xx = 0;
        while xx < width {
            let pixel = sampler.pixel(xx, yy);
            if (pixel[2] as i32) > (pixel[0] as i32) + 15
                && pixel[2] > 60
                && pixel[0] < 120
                && pixel[1] < 160
            {
                total_blue += 1;
            }
            total_samples += 1;
            xx += 16;
        }
        yy += 16;
    }
    if total_samples == 0 {
        return None;
    }
    let x1 = width.saturating_sub(20);
    let x0 = x1.saturating_sub(80);
    let (mut white, mut total) = (0_u32, 0_u32);
    let mut y = 10;
    while y < 70.min(height) {
        let mut x = x0;
        while x < x1 {
            let pixel = sampler.pixel(x, y);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= 220 {
                white += 1;
            }
            total += 1;
            x += 2;
        }
        y += 2;
    }
    if total == 0 {
        return None;
    }
    Some((
        total_blue as f32 / total_samples as f32,
        white as f32 / total as f32,
    ))
}

#[cfg(test)]
fn report_diagnostic_probes(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    panels: PanelRegions,
    screen: &config::ScreenConfig,
    kind: UiStateKind,
) -> Vec<serde_json::Value> {
    let threshold = templates.chat_templates.marker_threshold;
    let mut values = Vec::new();
    // 主链通用锚点（与 detect_ui_state_with_evidence 使用同一区域/阈值）。
    for (label, template, region) in [
        (
            "main_menu:return_to_teyvat",
            &templates.main_menu_template,
            screen.main_menu_rect.into(),
        ),
        (
            "secondary:back_button",
            &templates.secondary_back_template,
            screen.secondary_back_rect.into(),
        ),
        (
            "secondary:chat_submit",
            &templates.secondary_chat_submit_template,
            screen.secondary_chat_submit_rect.into(),
        ),
        (
            "overworld:world_wish",
            &templates.world_wish_template,
            screen.world_wish_rect.into(),
        ),
    ] {
        values.push(report_probe_json(
            image, label, template, region, threshold, 1,
        ));
    }
    // 面板专属锚点（同 refine_panel 的模板/区域/降采样）。
    let panel_cases: [(&str, &PathBuf, Rect, u32); 6] = [
        (
            "panel:friend_list",
            &templates.friend_panel_template,
            panels.friend_panel,
            1,
        ),
        (
            "panel:more_settings",
            &templates.friend_block_chat_template,
            panels.friend_block_chat,
            4,
        ),
        (
            "panel:add_friend",
            &templates.friend_search_panel_template,
            panels.friend_search_panel,
            1,
        ),
        (
            "panel:hall_card",
            &templates.invite_enter_hall_template,
            panels.invite_enter_hall,
            templates
                .panel_downsample_factors
                .get("invite_enter_hall")
                .copied()
                .unwrap_or(2),
        ),
        (
            "panel:archive_card",
            &templates.invite_goto_hall_template,
            panels.invite_goto_hall,
            templates
                .panel_downsample_factors
                .get("invite_goto_hall")
                .copied()
                .unwrap_or(1),
        ),
        (
            "panel:avatar_popup",
            &templates.invite_view_star_template,
            panels.invite_view_star,
            templates
                .panel_downsample_factors
                .get("invite_view_star")
                .copied()
                .unwrap_or(2),
        ),
    ];
    for (label, template, region, downscale) in panel_cases {
        values.push(report_probe_json(
            image, label, template, region, threshold, downscale,
        ));
    }
    // 确认弹窗双锚点：确认按钮 + 三个关键词文案。
    values.push(report_probe_json(
        image,
        "panel:confirm(button)",
        &templates.confirm_button_template,
        panels.confirm_button,
        threshold,
        templates
            .panel_downsample_factors
            .get("confirm_button")
            .copied()
            .unwrap_or(1),
    ));
    for (label, template) in [
        (
            "panel:confirm_block_player(message)",
            &templates.confirm_message_block_template,
        ),
        (
            "panel:confirm_enter_hall(message)",
            &templates.confirm_message_random_hall_template,
        ),
        (
            "panel:confirm_leave_hall(message)",
            &templates.confirm_message_return_teyvat_template,
        ),
    ] {
        values.push(report_probe_json(
            image,
            label,
            template,
            panels.confirm_message,
            CONFIRM_MESSAGE_MIN,
            CONFIRM_MESSAGE_SCALE,
        ));
    }
    // 千星专属锚点：大厅卡/档案卡仍是全帧 + 4x 降采样 + 更严阈值（与
    // refine_panel 一致）；退出菜单已裁成 197×29 的单行模板并收窄到固定
    // 位置，区域小、1× 匹配也很便宜，因此不受下面的成本护栏限制。
    // **成本护栏**：全帧大模板匹配单次就要数秒；分类器已经给出 Primary /
    // Secondary / MainMenu / Loading / Overworld 时，refine_panel 根本不会走到
    // 这些锚点，因此只记录“未测量”，不为了报告把每帧拖慢十几秒。
    // Panel / Unknown 才是 refine_panel 的候选分支，照常实测。
    let anchor_threshold = threshold.max(0.95);
    let measure_anchors = std::env::var_os("MILIASTRA_REPORT_FULL_PROBES").is_some()
        || matches!(kind, UiStateKind::Panel | UiStateKind::Unknown);
    let qianxing_cases: [(&str, &PathBuf, Rect, u32); 1] = [(
        "panel:qianxing_page(exit)",
        &templates.qianxing_exit_menu_template,
        QIANXING_EXIT_MENU_SEARCH,
        QIANXING_EXIT_MENU_SCALE,
    )];
    for (label, template, region, scale) in qianxing_cases {
        if label == "panel:qianxing_page(exit)" || measure_anchors {
            values.push(report_probe_json(
                image,
                label,
                template,
                region,
                anchor_threshold,
                scale,
            ));
        } else {
            values.push(serde_json::json!({
                "label": label,
                "template": template.display().to_string(),
                "search_rect": gallery_rect_json(region),
                "score": null,
                "threshold": anchor_threshold,
                "hit_rect": null,
                "pass": false,
                "downscale": scale,
                "source": "diagnostic",
                "outcome": "skipped_cost_guard",
                "note": format!(
                    "分类器已判 {}/{}，未进入 refine_panel；为控制耗时未做全帧匹配",
                    kind.name(),
                    kind.label()
                ),
            }));
        }
    }
    // F2 多人面板两个按钮锚点。
    let f2_threshold = threshold.max(QIANXING_F2_BUTTON_MIN);
    for (label, template, region) in [
        (
            "secondary:qianxing_panel(hall_list)",
            &templates.qianxing_f2_hall_list_template,
            screen.hall_bar_rect.into(),
        ),
        (
            "secondary:qianxing_panel(invite)",
            &templates.qianxing_f2_invite_player_template,
            screen.qianxing_f2_invite_rect.into(),
        ),
    ] {
        values.push(report_probe_json(
            image,
            label,
            template,
            region,
            f2_threshold,
            QIANXING_F2_BUTTON_SCALE,
        ));
    }
    // 千星地图右下角锚点：专用 F1 形状匹配（不是灰度 SAD）。
    let mut anchor = gallery_qianxing_map_anchor(image, templates);
    anchor["label"] = serde_json::json!("secondary:qianxing_map(anchor)");
    anchor["source"] = serde_json::json!("diagnostic");
    anchor["search_rect"] = anchor["rect"].clone();
    anchor["downscale"] = serde_json::json!(1);
    anchor["pass"] = serde_json::json!(
        anchor["score"]
            .as_f64()
            .is_some_and(|score| score as f32 >= QIANXING_MAP_ANCHOR_SCORE_MIN)
    );
    values.push(anchor);
    values
}

fn game_classification(kind: UiStateKind) -> UiStateClassification {
    UiStateClassification::new(
        kind,
        match kind {
            UiStateKind::GameGate => "game_gate",
            UiStateKind::Overworld => "overworld",
            _ => "unknown",
        },
    )
}

fn observe_game(
    fallback: &mut GameFallbackState,
    now: Instant,
    kind: UiStateKind,
) -> UiStateClassification {
    fallback
        .observe_unknown(now, game_classification(UiStateKind::Unknown), || {
            Ok(game_classification(kind))
        })
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
                game_classification(UiStateKind::Unknown),
                || panic!("supplemental probes must wait for sustained unknown"),
            )
            .unwrap();
        assert_eq!(result.kind(), UiStateKind::Unknown);
    }
    let first = observe_game(
        &mut fallback,
        now + Duration::from_secs(5),
        UiStateKind::Overworld,
    );
    assert_eq!(first.kind(), UiStateKind::Unknown);
    assert_eq!(first.label(), "unknown:game_probe");
    let confirmed_at = now + Duration::from_millis(5010);
    assert_eq!(
        observe_game(&mut fallback, confirmed_at, UiStateKind::Overworld).kind(),
        UiStateKind::Overworld
    );
    let cached = fallback
        .observe_unknown(
            now + Duration::from_secs(8),
            game_classification(UiStateKind::Unknown),
            || panic!("confirmed game state must not re-run OCR every frame"),
        )
        .unwrap();
    assert_eq!(cached.kind(), UiStateKind::Overworld);
    assert_eq!(
        fallback.candidate_count, 2,
        "cache does not count as a real probe"
    );

    let missed_at = confirmed_at + GAME_STATE_PROBE_INTERVAL;
    assert_eq!(
        observe_game(&mut fallback, missed_at, UiStateKind::Unknown).kind(),
        UiStateKind::Unknown
    );
    assert!(fallback.confirmed.is_none());
    assert!(
        fallback
            .observe_unknown(
                missed_at + Duration::from_secs(1),
                game_classification(UiStateKind::Unknown),
                || panic!("a missed probe also starts the cooldown"),
            )
            .is_ok()
    );
}

#[test]
fn game_fallback_revokes_changed_or_failed_results_and_bounds_unstable_probes() {
    let mut fallback = GameFallbackState::new(2);
    let now = Instant::now();
    observe_game(&mut fallback, now, UiStateKind::GameGate);
    observe_game(
        &mut fallback,
        now + Duration::from_secs(5),
        UiStateKind::GameGate,
    );
    observe_game(
        &mut fallback,
        now + Duration::from_secs(6),
        UiStateKind::GameGate,
    );
    let changed_at = now + Duration::from_secs(11);
    let changed = observe_game(&mut fallback, changed_at, UiStateKind::Overworld);
    assert_eq!(changed.label(), "unknown:game_probe");
    assert!(fallback.confirmed.is_none());
    let flapped = observe_game(
        &mut fallback,
        changed_at + Duration::from_millis(10),
        UiStateKind::GameGate,
    );
    assert_eq!(flapped.label(), "unknown");
    assert!(
        fallback
            .observe_unknown(
                changed_at + Duration::from_secs(1),
                game_classification(UiStateKind::Unknown),
                || panic!("alternating candidates must not cause OCR on every frame"),
            )
            .is_ok()
    );

    let retry_at = now + Duration::from_secs(17);
    observe_game(&mut fallback, retry_at, UiStateKind::Overworld);
    observe_game(
        &mut fallback,
        retry_at + Duration::from_millis(10),
        UiStateKind::Overworld,
    );
    let failed = fallback.observe_unknown(
        retry_at + Duration::from_secs(6),
        game_classification(UiStateKind::Unknown),
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
    if crate::test_support::corpus::skip_without_corpus(
        "world_wish_fixture_matches_both_backgrounds_and_shifted_icon",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
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

/// 主菜单必须比二级聊天先判：奇域主菜单上有和二级界面同一个圆形返回按钮，
/// 但右下角多一个「返回提瓦特」按钮。没有这条规则时，8 张主菜单帧会被
/// secondary_back 误判成二级聊天（technical-docs/ui-state-machine.md §24/§44）。
///
/// 每帧都用全新分类器：菜单判成 Unknown，连续两帧 Unknown 会触发游戏态回退，
/// 那会让 final_rule 变成回退规则，测不到我们真正要测的东西。
#[test]
fn main_menu_is_classified_before_secondary_back() {
    if crate::test_support::corpus::skip_without_corpus(
        "main_menu_is_classified_before_secondary_back",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    // 6 张是稳定帧，按钮完全不透明；step-5 / step-last 是菜单淡出过程中的
    // 过渡帧，按钮只剩约两成不透明度，任何模板都不该去追这种帧（单独特判它们
    // 就是拿观测凑结果）。过渡帧由应用自身的稳定性闸门挡掉：它们不会被标成
    // stable，因此不会触发 ui.state 事件。下面两条断言分别钉住这两件事。
    let menus = [
        "wonderland-menu.png",
        "wonderland-menu-escape.png",
        "settled-3.png",
        "final.png",
        "restored.png",
        "modR-5.png",
    ];
    for name in menus {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            c.evidence().final_rule(),
            "main_menu_template",
            "{path} 应判为主菜单而不是二级聊天"
        );
    }
    // 过渡帧：不要求判成主菜单，但必须不是稳定结果，否则会推出错误的迁移事件。
    for name in ["step-5.png", "step-last.png"] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        // 淡出帧按钮只剩两成不透明度：主菜单模板抓不到，但底栏也没有二级聊天
        // 的「发送」圆钮——task-102 起 Secondary 要求**返回按钮 + 聊天锚点**同时成立，
        // 因此这类过渡帧不再被判成二级聊天（旧实现会落到 secondary_back）。
        assert_eq!(
            c.evidence().final_rule(),
            "no_reliable_anchor",
            "{path} 无聊天发送锚点，不得判成二级聊天"
        );
    }
    let secondaries = ["secondary-full.png", "sec-fixture.jpg", "round78-live.png"];
    for name in secondaries {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(c.kind(), UiStateKind::Secondary, "{path} 必须仍是二级聊天");
    }
    // 负样本：这些界面里没有「返回提瓦特」按钮，一条都不许被判成主菜单。
    // 收紧搜索区之后重跑，确认窄区域没有把负样本拉上来。
    let negatives = [
        "hall-page.png",
        "full-hall.jpg",
        "overworld-hub.png",
        "map-screen.png",
        "team-finder.png",
        "wonderland-collection.png",
        "friend-search.png",
        "journey-page.png",
        "wish-page.png",
        "final-frame.jpg",
        "cat-frame-2.jpg",
    ];
    for name in negatives {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            c.evidence().final_rule(),
            "main_menu_template",
            "{path} 不含返回提瓦特按钮，不该被判成主菜单"
        );
    }
    // 仓库里原有的二级夹具也不能被新规则抢走
    let fixture = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg").unwrap();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    // 与生产同一条构造路径（composition/application.rs）：面板判据必须注入区域，否则
    // 覆盖层会全部落回 Unknown，门禁会把"没接线"误报成"分类错误"。
    let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone())
        .with_panel_regions(PanelRegions {
            friend_panel: config.moderation.friend_panel_region.into(),
            friend_search_panel: config.moderation.search_panel_region.into(),
            friend_block_chat: config.moderation.block_chat_region.into(),
            confirm_button: config.moderation.confirm_button_region.into(),
            confirm_message: config.moderation.confirm_message_region.into(),
            invite_view_star: config.invite.view_star_region.into(),
            invite_goto_hall: config.invite.goto_hall_region.into(),
            invite_enter_hall: config.invite.enter_hall_region.into(),
        });
    let c = classifier.classify_at(&fixture, Instant::now()).unwrap();
    assert_eq!(c.kind(), UiStateKind::Secondary);
}

/// 主菜单落的是 Unknown，连续两帧就会触发游戏态回退。回退探针是 world_wish，
/// 而 world-wish 在语料里命中过 60% 的帧，所以必须确认：站在奇域主菜单上
/// 连刷多帧，绝不会被回退误报成 overworld（第 44 节新增规则后的风险点）。
/// 【已回退，见 §47】原计划用大厅底栏 👥 O 否决提瓦特的世界聊天气泡；
/// 因底栏会缩放、而匹配器是 1:1 SAD，会漏检真实大厅帧（overworld-hub.png），已回退。
/// 保留为本轮实测记录：
/// 第 46 节实机因此把提瓦特判成了千星大厅。现在标记路径必须同时命中大厅底栏
/// 的 👥 O 图标组（实测间隔 0.3944），提瓦特底栏只有一个 Enter，会落空。
/// **第 47 节：暂时忽略。** 这条测试写的是我们**想要**的行为，但当前做不到：
/// 试过用「大厅底栏 👥 O」否决提瓦特的世界聊天气泡，实测分离度 0.3944 看着很好，
/// **但真实大厅帧会漏检**——底栏除了横竖位移还会**缩放**（overworld-hub.png 里
/// 的 👥 O 明显更小），而应用的模板匹配是 1:1 灰度 SAD，容不下缩放。
/// ⇒ 在当前匹配器下做不稳，已回退。等有对缩放鲁棒的判据再打开这条测试。
/// 第 48 节：用"底栏近白像素占比"否决提瓦特的世界聊天气泡。
///
/// **正样本集是查全的**：76 帧语料里所有近白占比 >= 11.18% 的帧都列在下面，
/// 一个不落——第 47 节就是栽在正样本集不全上（漏了 overworld-hub.png）。
/// 同时把提瓦特/登录/地图这些低占比帧作为负样本钉住。
#[test]
#[ignore = "requires the local real screenshot corpus under technical-docs/ui-state-evidence or dist/sandbox-run"]
fn teyvat_world_chat_is_not_the_hall() {
    let config = crate::config::bundled_config();
    // 语料跨两个目录：证据目录与运行目录，两边都要找。
    let classify = |name: &str| {
        let path =
            if std::path::Path::new(&format!("technical-docs/ui-state-evidence/{name}")).exists() {
                format!("technical-docs/ui-state-evidence/{name}")
            } else {
                format!("dist/sandbox-run/{name}")
            };
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        classifier
            .classify_at(&image, Instant::now())
            .unwrap()
            .kind()
    };
    // 正样本：语料里全部近白占比 >= 11.18% 的帧，必须是 primary
    for name in [
        "hall-chat-open.jpg",
        "ocr-A.jpg",
        "step-1.png",
        "restored-primary.png",
        "cat.jpg",
        "input-0.png",
        "input-1.png",
        "cat-frame-1.jpg",
        "repro-a.png",
        "repro-b.png",
        "overworld-hub.png",
        "capture-0.png",
        "full-hall.jpg",
        "cat-frame-2.jpg",
        "hall-chat-closed.jpg",
        "round78-live2.png",
    ] {
        assert_eq!(
            classify(name),
            UiStateKind::Primary,
            "{name} 是千星大厅，近白占比够，不许被否决"
        );
    }
    // 负样本：提瓦特与登录界面（世界聊天也带气泡，但底栏白键太少）
    for name in [
        "cold-start/enter-2.jpg",
        "transition-to-overworld/05-overworld-teyvat.jpg",
        "login-screen.jpg",
    ] {
        assert_ne!(
            classify(name),
            UiStateKind::Primary,
            "{name} 不是千星大厅，不许判成 primary"
        );
    }
}

/// 第 55 节：加载界面是**真实状态**（两种外观），不该像以前那样落进 unknown。
///
/// 这里同时给出**分离度自证**：不只看结论，还直接把两条判据的实测区间算出来并断言
/// 两者不重叠。谁改坏了量法或阈值，这条测试都会直接失败，而不是靠"结果刚好对"。
#[test]
fn loading_screens_are_classified_as_loading() {
    if crate::test_support::corpus::skip_without_corpus("loading_screens_are_classified_as_loading")
    {
        return;
    }
    let config = crate::config::bundled_config();
    let classify_with_rule = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        (
            c.kind(),
            c.evidence().final_rule().to_string(),
            screen_tone(&image),
        )
    };

    // 黑底徽记型（传送等短加载）：实测暗像素占比 98.46~98.53%
    let mut dark_floor = f32::MAX;
    let mut dark_consistency_floor = f32::MAX;
    for name in [
        "loading-screens/loading-dark-A.jpg",
        "loading-screens/loading-dark-B.jpg",
        "loading-screens/loading-dark-C.jpg",
        "loading-screens/loading-dark-D.jpg",
        "loading-screens/loading-dark-E.jpg",
        "loading-screens/loading-dark-teleport.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let (kind, rule, (dark, _white, dominant)) = classify_with_rule(&path);
        assert_eq!(
            kind,
            UiStateKind::Loading,
            "{path} 是黑底加载界面，必须判成 loading 而不是 unknown"
        );
        assert_eq!(rule, "loading_dark_screen", "{path} 应走黑底那条规则");
        dark_floor = dark_floor.min(dark);
        dark_consistency_floor = dark_consistency_floor.min(dominant);
    }

    // 白底元素型（冷启动进世界）：实测近白占比 99.77~100%
    let mut white_floor = f32::MAX;
    let mut white_consistency_floor = f32::MAX;
    for name in [
        "blank-frame.jpg",
        "loading-screen.jpg",
        "cold-start/enter-1.jpg",
        "cold-start/stage-1.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let (kind, rule, (_dark, white, dominant)) = classify_with_rule(&path);
        assert_eq!(kind, UiStateKind::Loading, "{path} 是白底加载界面");
        assert_eq!(rule, "loading_white_screen", "{path} 应走白底那条规则");
        white_floor = white_floor.min(white);
        white_consistency_floor = white_consistency_floor.min(dominant);
    }

    // 分离度自证：真实画面里暗像素占比最高、近白占比最高、色彩一致度最高分别是多少
    let mut dark_ceiling = 0.0_f32;
    let mut white_ceiling = 0.0_f32;
    let mut consistency_ceiling = 0.0_f32;
    for name in [
        "loading-screens/teyvat-map-with-wp-panel.jpg",
        "loading-screens/after-teleport-new-scene.jpg",
        "loading-screens/00-now.jpg",
        "transition-to-overworld/01-hall-entrance.jpg",
        "hall-chat-closed.jpg",
        "transition-to-overworld/05-overworld-teyvat.jpg",
        "cold-start/enter-2.jpg",
        "cold-start/stage-2.jpg",
        "cold-start/stage-3.jpg",
        "cold-start/stage-4.jpg",
        "login-screen.jpg",
        "overworld-hub.png",
        "hall-page.png",
        "map-screen.png",
        "wonderland-menu.png",
        "wonderland-collection.png",
        "secondary-full.png",
        "sweep-49/02-map.jpg",
        "sweep-49/03-inventory.jpg",
        "sweep-49/06-wish.jpg",
        "sweep-49/10-F2.jpg",
        "sweep-49/11-activity.jpg",
        "modS2-6.png",
        "modClear-2.png",
    ] {
        let path = if Path::new(&format!("technical-docs/ui-state-evidence/{name}")).exists() {
            format!("technical-docs/ui-state-evidence/{name}")
        } else {
            format!("dist/sandbox-run/{name}")
        };
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let (dark, white, dominant) = screen_tone(&image);
        dark_ceiling = dark_ceiling.max(dark);
        white_ceiling = white_ceiling.max(white);
        consistency_ceiling = consistency_ceiling.max(dominant);
    }
    assert!(
        dark_floor > dark_ceiling,
        "黑底加载与真实画面必须不重叠：加载最低 {dark_floor} vs 真实最高 {dark_ceiling}"
    );
    assert!(
        white_floor > white_ceiling,
        "白底加载与真实画面必须不重叠：加载最低 {white_floor} vs 真实最高 {white_ceiling}"
    );
    assert!(
        dark_consistency_floor >= LOADING_COLOR_CONSISTENCY_MIN
            && white_consistency_floor >= LOADING_COLOR_CONSISTENCY_MIN,
        "加载界面色彩必须高度一致：黑底最低 {dark_consistency_floor}、白底最低 \
             {white_consistency_floor}，阈值 {LOADING_COLOR_CONSISTENCY_MIN}"
    );
    assert!(
        consistency_ceiling < LOADING_COLOR_CONSISTENCY_MIN,
        "真实画面色彩一致度最高 {consistency_ceiling} 必须低于加载阈值 \
             {LOADING_COLOR_CONSISTENCY_MIN}"
    );
}

/// 暗色但色彩分散的真实场景（如夜间大厅）：不能仅凭“暗”判成加载界面。
#[test]
fn dark_but_colorful_scene_is_not_loading() {
    let palette = [
        [220, 40, 40],
        [40, 200, 80],
        [30, 90, 220],
        [230, 200, 40],
        [200, 40, 200],
        [40, 200, 200],
        [240, 240, 240],
        [120, 120, 120],
    ];
    let image = image::RgbImage::from_fn(1920, 1080, |x, y| {
        let cell = (x / 48 + y / 48) as usize;
        if cell % 10 < 3 {
            image::Rgb(palette[cell % palette.len()])
        } else {
            image::Rgb([16, 14, 26])
        }
    });
    let image = image::DynamicImage::ImageRgb8(image);
    let (dark, _white, dominant) = screen_tone(&image);
    assert!(
        dark >= LOADING_DARK_RATIO_MIN,
        "测试画面前提：暗像素占比 {dark} 应达到加载暗阈值"
    );
    assert!(
        dominant < LOADING_COLOR_CONSISTENCY_MIN,
        "测试画面前提：主色桶占比 {dominant} 应低于加载一致度阈值"
    );
    assert_eq!(
        loading_rule(&image),
        None,
        "暗色但色彩分散的场景不是加载界面"
    );
}

/// 世界判据（提瓦特 vs 千星）：左上角**青色条纹带计数**。
///
/// 与 `loading_screens_are_classified_as_loading` 同样的纪律：不只看结论，
/// 而是把**两侧的实测区间**直接算出来并断言它们不重叠，再断言阈值严格落在空档里。
/// 谁改坏了量法或阈值，这条测试立刻失败，而不是靠"结果刚好对"。
///
/// 判据形态与第 48 节一致：**区域上的标量统计**，不是 1:1 模板匹配，
/// 所以对位移与轻度缩放都不敏感。
#[test]
fn world_is_separated_by_cyan_band_count() {
    if crate::test_support::corpus::skip_without_corpus("world_is_separated_by_cyan_band_count") {
        return;
    }
    let count_of = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        world_cyan_band_count(&image, 1.0)
    };

    // 提瓦特侧：冷启动进世界后的 HUD 帧 + 两个不同地貌的大世界帧。
    let mut teyvat_ceiling = 0_usize;
    for name in [
        "cold-start/stage-6.jpg",
        "cold-start/stage-7.jpg",
        "cold-start/stage-9.jpg",
        "cold-start/enter-2.jpg",
        "cold-start/enter-3.jpg",
        "cold-start/enter-4.jpg",
        "cold-start/enter-5.jpg",
        "teyvat-regions-58/snow-01.jpg",
        "teyvat-regions-58/snow-02.jpg",
        "teyvat-regions-58/snow-03.jpg",
        "teyvat-regions-58/desert-01.jpg",
        "teyvat-regions-58/desert-02.jpg",
        "teyvat-regions-58/desert-03.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let count = count_of(&path).unwrap_or_else(|| panic!("{path} 应是整帧截图"));
        assert_eq!(
            guess_world_from_cyan_count(Some(count)),
            WorldGuess::Teyvat,
            "{path} 是提瓦特帧，青色条纹带计数 {count} 必须落在提瓦特侧"
        );
        teyvat_ceiling = teyvat_ceiling.max(count);
    }

    // 千星侧：三个**不同场景**的大厅 + 两个入口帧，验证判据与场景内容无关。
    let mut qianxing_floor = usize::MAX;
    for name in [
        "qianxing-entry-56/04-hall-entered.jpg",
        "qianxing-entry-56/06-hall-before-M.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "qianxing-halls-58/hall3-01.jpg",
        "live-hall-day-57/01-hall-day-chat-active.png",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let count = count_of(&path).unwrap_or_else(|| panic!("{path} 应是整帧截图"));
        assert_eq!(
            guess_world_from_cyan_count(Some(count)),
            WorldGuess::Qianxing,
            "{path} 是千星大厅帧，青色条纹带计数 {count} 必须落在千星侧"
        );
        qianxing_floor = qianxing_floor.min(count);
    }

    // 分离度自证：两侧区间必须**不重叠**。
    assert!(
        teyvat_ceiling < qianxing_floor,
        "提瓦特侧最高 {teyvat_ceiling} 必须严格小于千星侧最低 {qianxing_floor}"
    );
    // 阈值必须严格落在空档内，而不是压在某一侧的实测值上。
    assert!(
        WORLD_CYAN_TEYVAT_MAX <= qianxing_floor && WORLD_CYAN_TEYVAT_MAX > teyvat_ceiling,
        "提瓦特上限 {WORLD_CYAN_TEYVAT_MAX} 必须高于实测最高 {teyvat_ceiling} 且不超过千星最低 {qianxing_floor}"
    );
    assert!(
        WORLD_CYAN_QIANXING_MIN > teyvat_ceiling && WORLD_CYAN_QIANXING_MIN <= qianxing_floor,
        "千星下限 {WORLD_CYAN_QIANXING_MIN} 必须高于提瓦特最高 {teyvat_ceiling} 且覆盖千星最低 {qianxing_floor}"
    );

    // 小裁剪图必须拒判，而不是拿一小块去猜世界。
    let tiny = DynamicImage::new_rgb8(220, 230);
    assert_eq!(world_cyan_band_count(&tiny, 1.0), None);
    assert_eq!(guess_world_from_cyan_count(None), WorldGuess::Unknown);

    // 落在两侧空档之间的计数必须是 Unknown，不能倒向任何一侧。
    for count in [WORLD_CYAN_TEYVAT_MAX + 1, WORLD_CYAN_QIANXING_MIN - 1] {
        assert_eq!(
            guess_world_from_cyan_count(Some(count)),
            WorldGuess::Unknown,
            "计数 {count} 落在空档里，必须返回 Unknown 而不是猜一个世界"
        );
    }
    assert_eq!(guess_world_from_cyan_count(Some(0)), WorldGuess::Teyvat);
    assert_eq!(
        guess_world_from_cyan_count(Some(WORLD_CYAN_QIANXING_MIN)),
        WorldGuess::Qianxing
    );
}

/// 世界判据**必须只在 HUD 在场时给结论**。
///
/// 全屏菜单的左上角是菜单内容，青色计数恒为 0，与提瓦特的 0 无法区分。
/// 没有这道门时，判据会把主菜单大世界、奇域枢纽菜单、提瓦特地图一律**自信地**
/// 报成「提瓦特」—— 这正是目标禁止的行为：判不出来就返回 unknown，而不是猜一个。
///
/// 这条测试是补写的：第 58 节先把青色标量接进了证据链，实测立刻发现
/// 主菜单大世界被判成 teyvat，才回头补门。第一版门用的是界面 kind，实测又发现
/// 它同时挡住了提瓦特侧（overworld 只能靠连续 2 帧 unknown 的回退得到），
/// 于是改成 HUD 在场检测 —— 见下面两条正面测试。
#[test]
fn world_probe_never_guesses_a_world_outside_hud_states() {
    if crate::test_support::corpus::skip_without_corpus(
        "world_probe_never_guesses_a_world_outside_hud_states",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let mut checked = 0_usize;
    for name in [
        "sweep-49/01-paimon-menu.jpg",
        "sweep-49/10-F2.jpg",
        "qianxing-entry-56/05-hub-menu.jpg",
        "exit-to-teyvat-56/01-hub-menu.jpg",
        "sweep-49/02-map.jpg",
        "sweep-49/03-inventory.jpg",
        "sweep-49/06-wish.jpg",
        "qianxing-entry-56/07-qianxing-map.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(c.kind(), UiStateKind::Primary, "{path} 不该被判成千星大厅");
        let Some(world) = c.evidence().world_probe() else {
            // 落在 unknown 回退路径上的帧，证据被回退结果替换掉了，
            // 这里没有世界探针可查——但下面的用例覆盖了这条路径不缺判。
            continue;
        };
        checked += 1;
        assert_ne!(
            world.guess(),
            "qianxing",
            "{path} 不是千星大厅，绝不能报 qianxing"
        );
        assert_ne!(
            world.guess(),
            "teyvat",
            "{path} 的 kind 是 {}，左上角不是 HUD，世界判据必须拒判而不是猜一个世界（实测计数 {}）",
            c.kind().name(),
            world
                .count()
                .map_or_else(|| "null".to_string(), |c| c.to_string())
        );
    }
    assert!(
        checked >= 4,
        "应该至少有 4 帧真的走到了带世界探针的非 unknown 路径，实际只有 {checked}"
    );
}

/// 世界判据的**正面能力**：HUD 在场时，提瓦特与千星两侧都要能判出来。
///
/// 这是第 58 节第二版门的验收：把门从"界面 kind"换成"HUD 在场检测"之后，
/// 提瓦特的大世界帧（app 单帧只会判成 unknown）也必须拿到 teyvat。
#[test]
fn world_probe_decides_both_worlds_when_hud_is_present() {
    if crate::test_support::corpus::skip_without_corpus(
        "world_probe_decides_both_worlds_when_hud_is_present",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let world_of = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        // 一级/二级命中时分类器会跳过世界探针（性能优先），这里直接算一次，
        // 只验证世界判据本身的正反例能力，不依赖分类路径。
        let world = c
            .evidence()
            .world_probe()
            .cloned()
            .unwrap_or_else(|| world_probe_evidence(&image));
        (world.guess().to_string(), world.count())
    };

    // 提瓦特侧：冷启动进世界 + 两个不同地貌。
    for name in [
        "cold-start/stage-6.jpg",
        "cold-start/stage-9.jpg",
        "teyvat-regions-58/snow-01.jpg",
        "teyvat-regions-58/desert-01.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let (world, count) = world_of(&path);
        assert_eq!(
            world, "teyvat",
            "{path} 的 HUD 在场，世界判据必须给出 teyvat（实测青色计数 {count:?}）"
        );
    }

    // 千星侧：三个不同场景的大厅。
    for name in [
        "qianxing-halls-58/hall3-01.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "qianxing-entry-56/06-hall-before-M.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let (world, count) = world_of(&path);
        assert_eq!(
            world, "qianxing",
            "{path} 的 HUD 在场，世界判据必须给出 qianxing（实测青色计数 {count:?}）"
        );
    }
}

/// HUD 在场检测本身的分离度自证：在场与不在场两组必须**不重叠**。
#[test]
fn hud_presence_is_separated_by_reference_distance() {
    if crate::test_support::corpus::skip_without_corpus(
        "hud_presence_is_separated_by_reference_distance",
    ) {
        return;
    }
    let distance_of = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let (teyvat, qianxing, _scale) =
            world_hud_distances(&image).unwrap_or_else(|| panic!("{path} 应能算出距离"));
        teyvat.min(qianxing)
    };
    let mut present_ceiling = 0.0_f32;
    for name in [
        "qianxing-halls-58/hall3-01.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "qianxing-entry-56/06-hall-before-M.jpg",
        "cold-start/stage-6.jpg",
        "teyvat-regions-58/snow-01.jpg",
        "teyvat-regions-58/desert-01.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        present_ceiling = present_ceiling.max(distance_of(&path));
    }
    let mut absent_floor = f32::MAX;
    for name in [
        "sweep-49/01-paimon-menu.jpg",
        "qianxing-entry-56/05-hub-menu.jpg",
        "sweep-49/02-map.jpg",
        "sweep-49/03-inventory.jpg",
        "sweep-49/06-wish.jpg",
        "qianxing-entry-56/07-qianxing-map.jpg",
        "cold-start/stage-1.jpg",
        "cold-start/stage-2.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        absent_floor = absent_floor.min(distance_of(&path));
    }
    assert!(
        present_ceiling < absent_floor,
        "HUD 在场与不在场必须不重叠：在场最高 {present_ceiling} vs 不在场最低 {absent_floor}"
    );
    assert!(
        WORLD_HUD_PRESENT_MAX > present_ceiling && WORLD_HUD_PRESENT_MAX < absent_floor,
        "HUD 在场阈值 {WORLD_HUD_PRESENT_MAX} 必须严格落在空档 ({present_ceiling}, {absent_floor}) 内"
    );
}

/// 千星奇域地图判据：**两个轴各自的空档都必须存在**，且阈值落在空档内。
///
/// 与 `loading_screens_are_classified_as_loading` 同样的纪律：不只看结论，
/// 而是把正负两侧的实测区间算出来，断言**两个轴都不重叠**。
///

/// 千星地图**右下角锚点**：正负两侧必须不重叠，阈值落在空档内。
///
/// 与整屏色调判据的分工：这里量的是右下角那一小块 UI（「千星奇域·大厅」文字 +
/// 庙宇图标），对“整屏其它地方是不是暗青”不再敏感，所以亮底地图变体与深色虚空版
/// 都能命中；反过来，任何“整屏恰好偏暗青但没有这块标签”的页面也不会被它收进来。
#[test]
fn qianxing_map_anchor_separates_real_maps_from_every_other_screen() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_map_anchor_separates_real_maps_from_every_other_screen",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let score_of = |path: &str| -> f32 {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        qianxing_map_anchor_measure(&image, &templates.qianxing_map_label_template)
            .unwrap_or_else(|e| panic!("{path} 锚点计算失败: {e}"))
            .unwrap_or_else(|| panic!("{path} 应能算出锚点分数"))
            .score
    };

    // 正样本：9 帧真实千星地图（总览 / 放大 / 亮底变体 / 不同缩放）。
    let mut positive_floor = f32::MAX;
    for name in [
        "qianxing-entry-56/07-qianxing-map.jpg",
        "map-screen.png",
        "map-via-M.png",
        "qianxing-ui-59/qxmap-a.jpg",
        "qianxing-ui-59/qxmap-b.jpg",
        "qianxing-ui-59/qxmap-c.jpg",
        "qianxing-ui-59/r17-地图.jpg",
        "qianxing-ui-59/r17-cal-180.jpg",
        "qianxing-ui-59/r18sub/r2c0-map.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let score = score_of(&path);
        assert!(
            score >= QIANXING_MAP_ANCHOR_SCORE_MIN,
            "{path} 是千星地图，右下角锚点必须命中（实测 F1 {score:.4} < 阈值 {}）",
            QIANXING_MAP_ANCHOR_SCORE_MIN
        );
        positive_floor = positive_floor.min(score);
    }

    // 负样本：千星大厅 / 主菜单千星 / 主菜单大世界 / F2 大厅页 / 二级聊天 /
    // 覆盖层大厅卡·档案卡 / 提瓦特地图（含右下角同样是「文字+圆形图标+红点」的
    // 月之高地帧，是最难的近邻）/ Loading / Overworld。
    let mut negative_ceiling = 0.0_f32;
    for name in [
        "qianxing-entry-56/04-hall-entered.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "qianxing-entry-56/05-hub-menu.jpg",
        "qianxing-ui-59/r12-hub-live.jpg",
        "sweep-49/01-paimon-menu.jpg",
        "qianxing-ui-59/qxf2-from-hall.jpg",
        "qianxing-ui-59/r10-panel-live.jpg",
        "qianxing-ui-59/r17-大厅.jpg",
        "modF-11.png",
        "qianxing-ui-59/qxchat-open.jpg",
        "hall-chat-open.jpg",
        "cold-to-qianxing-59/c01-map.jpg",
        "cold-to-qianxing-59/c03-map-zoomedout.jpg",
        "cold-to-qianxing-59/001-t000.jpg",
        "cold-start/stage-6.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let score = score_of(&path);
        assert!(
            score < QIANXING_MAP_ANCHOR_SCORE_MIN,
            "{path} 不是千星地图，右下角锚点必须拒绝（实测 F1 {score:.4} >= 阈值 {}）",
            QIANXING_MAP_ANCHOR_SCORE_MIN
        );
        negative_ceiling = negative_ceiling.max(score);
    }

    assert!(
        negative_ceiling < QIANXING_MAP_ANCHOR_SCORE_MIN,
        "负样本最高 {negative_ceiling:.4} 必须低于阈值"
    );
    assert!(
        QIANXING_MAP_ANCHOR_SCORE_MIN < positive_floor,
        "正样本最低 {positive_floor:.4} 必须高于阈值"
    );
    assert!(
        positive_floor - negative_ceiling >= 0.20,
        "正负两侧至少留 0.20 的空档：正最低 {positive_floor:.4} / 负最高 {negative_ceiling:.4}"
    );

    // 小裁剪图必须拒判，不能拿右下角一小块去猜整屏。
    let tiny = DynamicImage::new_rgb8(120, 68);
    assert!(
        qianxing_map_anchor_measure(&tiny, &templates.qianxing_map_label_template)
            .unwrap()
            .is_none(),
        "小裁剪图必须拒判"
    );
}

/// 右下角锚点命中 → `Secondary(secondary:qianxing_map)`，且证据里带的是锚点探针。
#[test]
fn qianxing_map_anchor_classifies_as_secondary_hall_map() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_map_anchor_classifies_as_secondary_hall_map",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    for name in [
        "qianxing-entry-56/07-qianxing-map.jpg",
        "map-screen.png",
        "map-via-M.png",
        "qianxing-ui-59/r17-地图.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let mut classifier = TemplateUiStateClassifier::new(
            UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
            config.screen.clone(),
        );
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            UiStateKind::Secondary,
            "{path} 必须归到 Secondary，实际 {}",
            classification.kind().name()
        );
        assert_eq!(
            classification.label(),
            "secondary:qianxing_map",
            "{path} 的 label 必须是 secondary:qianxing_map"
        );
        assert_eq!(
            classification.evidence().final_rule(),
            "qianxing_map_label_template",
            "{path} 的最终判据必须是右下角锚点，而不是整屏色调"
        );
        let probe = classification
            .evidence()
            .template_probes()
            .iter()
            .find(|probe| probe.template().ends_with("qianxing-map-label.png"))
            .unwrap_or_else(|| panic!("{path} 证据里应带右下角锚点模板探针"));
        assert_eq!(probe.outcome(), "matched");
        assert!(
            probe
                .best_score()
                .is_some_and(|score| score >= QIANXING_MAP_ANCHOR_SCORE_MIN),
            "{path} 锚点探针分数应不低于阈值"
        );
        let region = probe.search_rect();
        assert_eq!(
            (region.x(), region.y(), region.width(), region.height()),
            (
                QIANXING_MAP_ANCHOR_SEARCH.x,
                QIANXING_MAP_ANCHOR_SEARCH.y,
                QIANXING_MAP_ANCHOR_SEARCH.width,
                QIANXING_MAP_ANCHOR_SEARCH.height,
            ),
            "{path} 锚点 ROI 必须就是右下角那一块"
        );
        // 有模板就够，不需要 OCR：这里没有注入任何 OCR handle。
        let _ = &templates;
    }
}

/// 缩放稳健性：真实帧缩到 1280×720 / 1600×900 / 2560×1440 后锚点仍要命中。
///
/// ROI 是按宽高比缩放后再归一到规范尺寸的，所以换分辨率不需要重新标定坐标；
/// 这条测试防的是"把 1920×1080 的绝对坐标当成所有画面的坐标"。
#[test]
fn qianxing_map_anchor_survives_frame_scaling() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_map_anchor_survives_frame_scaling",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let source =
        image::open("technical-docs/ui-state-evidence/qianxing-entry-56/07-qianxing-map.jpg")
            .unwrap();
    for (width, height) in [(1280_u32, 720_u32), (1600, 900), (2560, 1440)] {
        let scaled = source.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        let matched = qianxing_map_anchor_measure(&scaled, &templates.qianxing_map_label_template)
            .unwrap()
            .unwrap_or_else(|| panic!("{width}x{height} 应能算出锚点分数"));
        assert!(
            matched.score >= QIANXING_MAP_ANCHOR_SCORE_MIN,
            "{width}x{height} 缩放到该分辨率后锚点仍须命中（实测 F1 {:.4}）",
            matched.score
        );
        assert!(
            matched.x > QIANXING_MAP_ANCHOR_SEARCH.x - 32
                && matched.y > QIANXING_MAP_ANCHOR_SEARCH.y - 32,
            "{width}x{height} 的命中位置必须仍在右下角（实测 {},{}）",
            matched.x,
            matched.y
        );
    }
}

/// 可选 OCR 后备的**文本判据**（纯字符串，不需要模型）。
#[test]
fn qianxing_map_label_text_requires_both_words() {
    assert!(qianxing_map_label_text_matches("千星奇域·大厅"));
    assert!(qianxing_map_label_text_matches("  千 星 奇 域 · 大 厅  "));
    assert!(qianxing_map_label_text_matches(
        "UID: 10000001 千星奇域 · 大厅"
    ));
    assert!(!qianxing_map_label_text_matches("月之高地 探索度 91%"));
    assert!(!qianxing_map_label_text_matches("千星奇域"));
    assert!(!qianxing_map_label_text_matches("大厅列表"));
    assert!(!qianxing_map_label_text_matches(""));
}

/// 可选 OCR 后备：对右下角 ROI 调一次 OCR 再套文本判据。
/// 离线可测——OCR 设备是桩，不依赖模型文件；模板路径本身也不需要 OCR。
#[test]
fn qianxing_map_label_ocr_backup_reads_the_bottom_right_label() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_map_label_ocr_backup_reads_the_bottom_right_label",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let image =
        image::open("technical-docs/ui-state-evidence/qianxing-entry-56/07-qianxing-map.jpg")
            .unwrap();
    let region = qianxing_map_anchor_rect(&image).expect("整帧应能算出锚点区域");
    let text = Arc::new(Mutex::new("千星奇域 · 大厅".to_string()));
    let ocr = OcrRuntime::start(
        TestGateOcr {
            calls: Arc::new(AtomicUsize::new(0)),
            text: Arc::clone(&text),
        },
        1,
    )
    .unwrap();
    assert!(
        qianxing_map_label_via_ocr(&ocr.handle(), &image, region).unwrap(),
        "右下角 OCR 文本是「千星奇域·大厅」时必须命中"
    );
    *text.lock().unwrap() = "月之高地 探索度 91%".to_string();
    assert!(
        !qianxing_map_label_via_ocr(&ocr.handle(), &image, region).unwrap(),
        "提瓦特地图右下角的「月之高地」不能命中"
    );
    ocr.shutdown().unwrap();
    let _ = &config;
}

/// 反向保证：把上面那批真实画面逐帧过一遍分类器，**一个都不许**被判成 loading。
#[test]
fn no_real_screen_is_classified_as_loading() {
    if crate::test_support::corpus::skip_without_corpus("no_real_screen_is_classified_as_loading") {
        return;
    }
    let config = crate::config::bundled_config();
    for name in [
        "loading-screens/teyvat-map-with-wp-panel.jpg",
        "loading-screens/after-teleport-new-scene.jpg",
        "loading-screens/00-now.jpg",
        "transition-to-overworld/01-hall-entrance.jpg",
        "hall-chat-closed.jpg",
        "transition-to-overworld/05-overworld-teyvat.jpg",
        "cold-start/enter-2.jpg",
        "cold-start/stage-2.jpg",
        "cold-start/stage-3.jpg",
        "cold-start/stage-4.jpg",
        "login-screen.jpg",
        "overworld-hub.png",
        "hall-page.png",
        "map-screen.png",
        "wonderland-menu.png",
        "wonderland-collection.png",
        "secondary-full.png",
        "sweep-49/02-map.jpg",
        "sweep-49/03-inventory.jpg",
        "sweep-49/06-wish.jpg",
        "sweep-49/10-F2.jpg",
        "sweep-49/11-activity.jpg",
        "modS2-6.png",
        "modClear-2.png",
    ] {
        let path = if Path::new(&format!("technical-docs/ui-state-evidence/{name}")).exists() {
            format!("technical-docs/ui-state-evidence/{name}")
        } else {
            format!("dist/sandbox-run/{name}")
        };
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let kind = classifier
            .classify_at(&image, Instant::now())
            .unwrap()
            .kind();
        assert_ne!(
            kind,
            UiStateKind::Loading,
            "{path} 是真实游戏画面，不许被判成 loading"
        );
    }
}

#[test]
fn chat_markers_need_the_hall_bottom_bar_to_mean_the_hall() {
    if crate::test_support::corpus::skip_without_corpus(
        "chat_markers_need_the_hall_bottom_bar_to_mean_the_hall",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    // 提瓦特（世界聊天带气泡）与大世界：不许判成 primary
    for name in [
        "cold-start/enter-2.jpg",
        "transition-to-overworld/05-overworld-teyvat.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            c.kind(),
            UiStateKind::Primary,
            "{path} 是提瓦特，不许判成千星大厅"
        );
    }
    // 千星大厅帧必须仍然是 primary
    // overworld-hub.png 是真实千星大厅帧（底栏 👥 O 齐全），但底栏位置与模板源帧不同。
    // 把它钉进正样本：搜索区一旦收窄就会漏掉它——第 47 节就是这么栽的。
    for name in [
        "hall-chat-closed.jpg",
        "cat-frame-2.jpg",
        "round78-live2.png",
        "overworld-hub.png",
        "hall-chat-open.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let mut image = image::open(&path).unwrap();
        // 本地夹具是有损 JPEG：底栏没有纯白，吸附一次以匹配线上无损抓图的前提。
        snap_near_white_to_pure_white(&mut image, config.screen.hall_bar_rect.into(), 225);
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(c.kind(), UiStateKind::Primary, "{path} 仍是千星大厅");
    }
}

#[test]
fn hub_menu_never_falls_through_to_overworld_after_unknown_grace() {
    if crate::test_support::corpus::skip_without_corpus(
        "hub_menu_never_falls_through_to_overworld_after_unknown_grace",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    for name in [
        "wonderland-menu.png",
        "wonderland-menu-escape.png",
        "modR-5.png",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let now = Instant::now();
        for i in 0..5u64 {
            let c = classifier
                .classify_at(&image, now + Duration::from_secs(i * 40))
                .unwrap();
            assert_ne!(
                c.kind(),
                UiStateKind::Overworld,
                "{path} 第 {i} 帧被回退误报成 overworld"
            );
        }
    }
}

/// **第 59 节：回退探针的自信误判，以及世界判据怎么把它拦住。**
///
/// 实测经过：站在**千星大厅**里按 `F2` 开多人面板后，实时路径先连续两帧 `Unknown`，
/// 于是走 `probe_game_state` 回退；回退只看右上角**祈愿顶栏**一个模板，而那个模板在
/// 千星面板上**也会命中** ⇒ 实时接口回了 `kind=overworld / rule=world_wish_template`，
/// 而**同一块画面**走离线单帧路径（不跑回退）却是 `Unknown`。
///
/// 这正是仓库里早就记下的风险（见 `hub_menu_never_falls_through_to_overworld_after_unknown_grace`：
/// world-wish 在语料里命中过 60% 的帧）。修法是叠一道**正交**的世界判据：
/// 祈愿顶栏命中**并且**「有 HUD 且判为提瓦特」才算大世界。
#[test]
fn probe_game_state_rejects_world_wish_hits_outside_teyvat() {
    if crate::test_support::corpus::skip_without_corpus(
        "probe_game_state_rejects_world_wish_hits_outside_teyvat",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    // 负样本：千星大厅的多人面板与聊天框三帧。
    for name in [
        "qianxing-ui-59/qxf2-from-hall.jpg",
        "qianxing-ui-59/qxchat-from-hall.jpg",
        "qianxing-ui-59/qxchat-closed2.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        assert!(!world_is_teyvat(&image), "{path} 不是提瓦特大世界");
        let probed = probe_game_state_without_entry(&image, &templates, &config.screen).unwrap();
        assert_ne!(
            probed.kind(),
            UiStateKind::Overworld,
            "{path} 站在千星大厅里，回退探针不该把它判成大世界"
        );
        assert_ne!(
            probed.evidence().final_rule(),
            "world_wish_template",
            "{path} 被世界判据否掉时应报 world_wish_without_hud_bottom_bar"
        );
    }
    // 正样本：真实的提瓦特大世界帧仍须通过世界判据，别把门关死。
    // 证据是 JPEG（白色被压到 240–254），按仓库既有口径先 snap 成纯白再判——
    // 等价于线上的无损抓图；Overworld 判据是「底栏白键 1~4」，0 个不猜。
    for name in [
        "teyvat-regions-58/snow-01.jpg",
        "teyvat-regions-58/desert-01.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let mut image = image::open(&path).unwrap();
        snap_near_white_to_pure_white(&mut image, config.screen.hall_bar_rect.into(), 225);
        assert!(
            world_is_teyvat(&image),
            "{path} 是提瓦特大世界，必须判为 true"
        );
    }
}

/// 千星大厅多人面板（F2）判据：**两侧区间必须不重叠**，阈值落在空档内。
///
/// 用的是与世界判据同一套方法——区域归一化描述子 + 冻结参考比距离。
/// 三帧正样本的**玩家列表各不相同**（公共大厅人数 36/40、37/40 等），
/// 所以这条判据证明的是"界面骨架"而不是"某一次的内容"。
#[test]
fn qianxing_player_panel_is_separated_by_reference_distance() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_player_panel_is_separated_by_reference_distance",
    ) {
        return;
    }
    let distance_of = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        qianxing_player_panel_distance(&image).unwrap_or_else(|| panic!("{path} 应能量出距离"))
    };
    // 正样本：三帧真实 F2 面板
    let mut positive_ceiling = 0.0_f32;
    for name in [
        "qianxing-ui-59/qxf2-from-hall.jpg",
        "qianxing-ui-59/after-fix-f2.jpg",
        "qianxing-ui-59/r10-now.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let distance = distance_of(&path);
        assert!(
            is_qianxing_player_panel(&image::open(&path).unwrap()),
            "{path} 是千星大厅多人面板，必须判为真（距离 {distance:.4}）"
        );
        positive_ceiling = positive_ceiling.max(distance);
    }
    // 负样本：其余界面，含**千星大厅本身**（必须与"大厅里的 F2 面板"分开）
    let mut negative_floor = f32::MAX;
    for name in [
        "qianxing-ui-59/start-hall.jpg",
        "qianxing-ui-59/back-hall-1.jpg",
        "qianxing-ui-59/hall-again.jpg",
        "qianxing-ui-59/qxmenu-hub.jpg",
        "qianxing-ui-59/qxmap-a.jpg",
        "qianxing-entry-56/05-hub-menu.jpg",
        "exit-to-teyvat-56/01-hub-menu.jpg",
        "transition-to-overworld/02-hub-menu.jpg",
        "qianxing-halls-58/hall3-01.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "qianxing-entry-56/07-qianxing-map.jpg",
        "sweep-49/02-map.jpg",
        "sweep-49/01-paimon-menu.jpg",
        "sweep-49/03-inventory.jpg",
        "sweep-49/10-F2.jpg",
        "sweep-49/11-activity.jpg",
        "sweep-49/06-wish.jpg",
        "sweep-49/07-friends.jpg",
        "teyvat-regions-58/snow-01.jpg",
        "teyvat-regions-58/desert-01.jpg",
        "loading-screens/01-map.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let distance = distance_of(&path);
        assert!(
            !is_qianxing_player_panel(&image::open(&path).unwrap()),
            "{path} 不是千星大厅多人面板，不该被判为真（距离 {distance:.4}）"
        );
        negative_floor = negative_floor.min(distance);
    }
    // 分离度自证：两侧区间必须不重叠。
    assert!(
        positive_ceiling < negative_floor,
        "正样本最大 {positive_ceiling:.4} 必须严格小于负样本最小 {negative_floor:.4}"
    );
    // 阈值必须严格落在空档内，而不是压在某一侧的实测值上。
    assert!(
        QIANXING_PANEL_MAX > positive_ceiling && QIANXING_PANEL_MAX < negative_floor,
        "阈值 {QIANXING_PANEL_MAX} 必须落在空档 ({positive_ceiling:.4}, {negative_floor:.4}) 内"
    );
    // 小裁剪图必须返回 None 拒判，而不是拿一小块去比。
    let tiny = DynamicImage::new_rgb8(120, 68);
    assert_eq!(qianxing_player_panel_distance(&tiny), None);
    assert!(!is_qianxing_player_panel(&tiny));
}

/// **第 61 节：盖革式弱点——位移。**
///
/// 把真实 F2 面板整体平移 ±1/±2/±3 像素，判据必须仍然命中。
///
/// 这条用例锁死的是 `QIANXING_PANEL_SEARCH = 2` 这个搜索窗口：
/// 没有搜索时位移 ±3 的距离是 **0.1969**、阈值 **0.22**，只有 10% 余量，
/// 等于站在悬崖边上。加了 ±2 搜索之后，±3 的最优对齐只剩 1 像素残差，
/// 距离掉回 ≈±1 的水平。**阈值本身一个字没动**——这是原理性修法，不是调阈值。
#[test]
fn qianxing_player_panel_survives_small_pixel_shifts() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_player_panel_survives_small_pixel_shifts",
    ) {
        return;
    }
    let base = image::open("technical-docs/ui-state-evidence/qianxing-ui-59/r10-now.jpg").unwrap();
    let (width, height) = (base.width(), base.height());
    for (dx, dy) in [(1, 0), (-1, 0), (2, 0), (-2, 0), (3, 0), (-3, 0)] {
        // 与扰动生成脚本同样的做法：画到同尺寸画布上，空出来的边角是黑的。
        let mut canvas = image::RgbaImage::new(width, height);
        image::imageops::replace(&mut canvas, &base.to_rgba8(), i64::from(dx), i64::from(dy));
        let shifted = DynamicImage::ImageRgba8(canvas);
        let distance = qianxing_player_panel_distance(&shifted)
            .unwrap_or_else(|| panic!("位移 ({dx},{dy}) 后应能量出距离"));
        assert!(
            is_qianxing_player_panel(&shifted),
            "位移 ({dx},{dy}) 后仍应判为千星多人面板（距离 {distance:.4}，阈值 {QIANXING_PANEL_MAX}）"
        );
    }
    // 负样本也要能平移：千星大厅整体平移后仍然不能命中。
    let hall =
        image::open("technical-docs/ui-state-evidence/qianxing-ui-59/start-hall.jpg").unwrap();
    for dx in [0_i64, 1, -1, 3, -3] {
        let mut canvas = image::RgbaImage::new(width, height);
        image::imageops::replace(&mut canvas, &hall.to_rgba8(), dx, 0);
        let shifted = DynamicImage::ImageRgba8(canvas);
        assert!(
            !is_qianxing_player_panel(&shifted),
            "千星大厅本身平移 {dx} 像素后也不该被判成 F2 面板"
        );
    }
}

/// F2 大厅页（千星多人面板）首选判据 = **左下「大厅列表」+ 右下「邀请玩家」**
/// 两个底部按钮锚点同时命中；任一不中就不判 F2，交给描述子兜底。
///
/// 这条用例同时钉住：
/// - 正样本（13 帧真实 F2，含公共/私人大厅、不同玩家列表）两个锚点都 >= 0.95，
///   分类输出必须是 `Secondary` / `secondary:qianxing_panel`（surface
///   `Secondary(MultiplayerPanel)`），不能再落成 `panel:hall_card` 或 `main_menu`；
/// - 负样本（大厅/主菜单/派蒙/二级聊天/覆盖层大厅卡/好友面板/地图/加载/大世界）
///   锚点分数都低于 0.95，分类不得是 F2 多人面板。
#[test]
fn f2_hall_page_bottom_buttons_classify_secondary_multiplayer_panel() {
    if crate::test_support::corpus::skip_without_corpus(
        "f2_hall_page_bottom_buttons_classify_secondary_multiplayer_panel",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let threshold = templates
        .chat_templates
        .marker_threshold
        .max(QIANXING_F2_BUTTON_MIN);
    let left_region: Rect = config.screen.hall_bar_rect.into();
    let right_region: Rect = config.screen.qianxing_f2_invite_rect.into();
    let score_of = |image: &DynamicImage, region: Rect, template: &PathBuf| {
        best_template_candidate_downscaled(image, Some(region), template, QIANXING_F2_BUTTON_SCALE)
            .ok()
            .flatten()
            .map(|hit| hit.score)
            .unwrap_or(0.0)
    };
    let classify = |image: &DynamicImage| {
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panel_regions_from(&config));
        classifier.classify_at(image, Instant::now()).unwrap()
    };

    let mut positive_floor = f32::MAX;
    for path in [
        "technical-docs/ui-state-evidence/qianxing-ui-59/after-fix-f2.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxf2-from-hall.jpg",
        "technical-docs/ui-state-evidence/hall-page.png",
        "tests/fixtures/ui/unknown-panel-1920x1080.png",
        "technical-docs/ui-state-evidence/qianxing-ui-59/probe-a.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/probe-b.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxchat-closed2.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxchat-from-hall.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r10-now.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r10-panel-live.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r10-set.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-大厅.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r1c0-hall.jpg",
    ] {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let left = score_of(
            &image,
            left_region,
            &templates.qianxing_f2_hall_list_template,
        );
        let right = score_of(
            &image,
            right_region,
            &templates.qianxing_f2_invite_player_template,
        );
        println!("F2POS {path} hall_list={left:.4} invite={right:.4}");
        assert!(
            left >= threshold && right >= threshold,
            "{path} 两个按钮锚点应同时 >= {threshold}（大厅列表 {left:.4}、邀请玩家 {right:.4}）"
        );
        positive_floor = positive_floor.min(left.min(right));
        let classification = classify(&image);
        assert_eq!(
            classification.kind(),
            UiStateKind::Secondary,
            "{path} 应判 Secondary，实际 {}/{}",
            classification.kind().name(),
            classification.label()
        );
        assert_eq!(
            classification.label(),
            "secondary:qianxing_panel",
            "{path} 应判 F2 多人面板，实际 {}/{}",
            classification.kind().name(),
            classification.label()
        );
        assert_eq!(
            classification.evidence().final_rule(),
            "secondary:qianxing_panel_buttons",
            "{path} 应走按钮首选判据"
        );
        let surface = crate::ui::routines::test_surface_of(
            classification.kind(),
            classification.label(),
            classification.evidence().final_rule(),
        );
        assert_eq!(surface.chinese_name(), "二级界面·多人面板", "{path}");
        assert!(
            surface.is_secondary_interface(),
            "{path} surface 应为二级界面子型"
        );
    }

    // 负样本覆盖：千星大厅、主菜单千星、主菜单大世界、二级聊天（大厅/好友）、
    // 覆盖层大厅卡/好友面板、千星地图、Loading、Overworld。
    let mut negative_ceiling = 0.0_f32;
    for path in [
        "tests/fixtures/ui/hall-primary-1920x1080.jpg",
        "tests/fixtures/ui/main-menu-wonderland-1920x1080.png",
        "technical-docs/ui-state-evidence/wonderland-menu.png",
        "tests/fixtures/ui/menu-paimon-1920x1080.jpg",
        "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxchat-open.jpg",
        "tests/fixtures/ui/panel-hall-card-1920x1080.jpg",
        "tests/fixtures/ui/panel-friend-list-1920x1080.jpg",
        "technical-docs/ui-state-evidence/friend-list.png",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxmap-a.jpg",
        "tests/fixtures/ui/map-teyvat-1920x1080.png",
        "tests/fixtures/ui/loading-dark-1920x1080.jpg",
        "tests/fixtures/ui/overworld-teyvat-1920x1080.jpg",
    ] {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let left = score_of(
            &image,
            left_region,
            &templates.qianxing_f2_hall_list_template,
        );
        let right = score_of(
            &image,
            right_region,
            &templates.qianxing_f2_invite_player_template,
        );
        let classification = classify(&image);
        println!(
            "F2NEG {path} hall_list={left:.4} invite={right:.4} label={}",
            classification.label()
        );
        assert!(
            left < threshold && right < threshold,
            "{path} 不得同时命中 F2 按钮锚点（大厅列表 {left:.4}、邀请玩家 {right:.4}）"
        );
        negative_ceiling = negative_ceiling.max(left.max(right));
        assert_ne!(
            classification.label(),
            "secondary:qianxing_panel",
            "{path} 被误判成 F2 多人面板"
        );
    }
    assert!(
        QIANXING_F2_BUTTON_MIN > negative_ceiling && QIANXING_F2_BUTTON_MIN < positive_floor,
        "阈值 {QIANXING_F2_BUTTON_MIN} 必须落在空档 ({negative_ceiling:.4}, {positive_floor:.4}) 内"
    );
}

#[test]
fn classifier_keeps_chat_priority_and_probes_game_states_only_after_unknown_grace() {
    if crate::test_support::corpus::skip_without_corpus(
        "classifier_keeps_chat_priority_and_probes_game_states_only_after_unknown_grace",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
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
    let secondary = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg").unwrap();
    // 真实祈愿页截图：天然带祈愿顶栏、但没有提瓦特 HUD——正是本用例要钉住的
    // "回退探针被世界判据门拦下"场景的真值来源（tests/fixtures/ui/README.md）。
    let world = image::open("tests/fixtures/ui/world-wish-page-1920x1080.png").unwrap();
    let now = Instant::now();
    for elapsed in [0, 10] {
        assert_eq!(
            classifier
                .classify_at(&secondary, now + Duration::from_secs(elapsed))
                .unwrap()
                .kind(),
            UiStateKind::Secondary
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        classifier
            .classify_at(&world, now + Duration::from_secs(20))
            .unwrap()
            .kind(),
        UiStateKind::Unknown
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
        UiStateKind::GameGate,
        "gate OCR has priority over the wish template"
    );
    assert_eq!(
        classifier
            .classify_at(&world, now + Duration::from_secs(27))
            .unwrap()
            .kind(),
        UiStateKind::GameGate
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    *text.lock().unwrap() = String::new();
    // 第 59 节给回退探针加了**世界判据门**之后，这里不再是 `Overworld`。
    // 真实祈愿页有祈愿顶栏、但**没有提瓦特 HUD**，所以门必须把它拦下。
    //
    // 注意 `label()` 与判据名是**两个字段**：`probe_game_state` 传给
    // `with_evidence` 的第二个参数是 `"unknown"`，而原因写在 **evidence 的
    // `final_rule()`** 里。所以这里两样都断言，才能证明「探针确实跑了、且被门拦下」——
    // 如果探头根本没跑，`final_rule()` 会是 `no_game_anchor` 而不是门的规则名。
    // 第 31 秒：探针跑了，命中祈愿顶栏，但被世界判据拦下 ⇒ 判据名是门的那一个。
    let probed = classifier
        .classify_at(&world, now + Duration::from_secs(31))
        .unwrap();
    assert_eq!(probed.label(), "unknown");
    assert_eq!(
        probed.evidence().final_rule(),
        "world_wish_without_hud_bottom_bar",
        "祈愿顶栏命中但没有提瓦特 HUD，回退探针必须被世界判据拦下"
    );
    // 第 32 秒：还在探针冷却期（5 秒）里，返回的是基础分类 ⇒ 判据名回到 no_reliable_anchor。
    // 这一对断言把"探针何时跑、跑完门如何裁决"两个时刻都钉住了。
    let cooled = classifier
        .classify_at(&world, now + Duration::from_secs(32))
        .unwrap();
    assert_eq!(cooled.label(), "unknown");
    assert_eq!(cooled.evidence().final_rule(), "no_reliable_anchor");
    // 第 31 秒那次探针会产生两次 OCR：一次「点击进入」门禁文本，一次
    // 千星地图右下角标签的可选后备（`qianxing_map_label_via_ocr`）。
    // 第 32 秒在冷却期里，探针根本没跑，因此总数停在 4。
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        classifier
            .classify_at(&secondary, now + Duration::from_secs(33))
            .unwrap()
            .kind(),
        UiStateKind::Secondary
    );
    assert!(classifier.game_fallback.confirmed.is_none());
    assert_eq!(
        classifier
            .classify_at(&world, now + Duration::from_secs(34))
            .unwrap()
            .kind(),
        UiStateKind::Unknown
    );
    // 仍然是 4：第 34 秒还在同一次冷却期内，第 33 秒那帧是 secondary 直接返回。
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    // 好友判据已并入大厅；现在链首扫的是聊天标记，所以用聊天区的非法矩形验证"坏区域必须报错"。
    classifier.screen.chat_rect.width = 3000;
    assert!(
        classifier
            .classify_at(&world, now + Duration::from_secs(40))
            .is_err()
    );
    assert!(classifier.game_fallback.unknown_since.is_none());
    ocr.shutdown().unwrap();
}

#[test]
fn pixel_identical_unknown_cache_hit_advances_fallback_time() {
    if crate::test_support::corpus::skip_without_corpus(
        "pixel_identical_unknown_cache_hit_advances_fallback_time",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let calls = Arc::new(AtomicUsize::new(0));
    let ocr = OcrRuntime::start(
        TestGateOcr {
            calls: calls.clone(),
            text: Arc::new(Mutex::new("点击进入".to_string())),
        },
        1,
    )
    .unwrap();
    let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone())
        .with_game_entry_detection(ocr.handle(), Rect::new(500, 800, 500, 100), 2);
    let image = image::open("tests/fixtures/ui/world-wish-page-1920x1080.png").unwrap();
    let cached = classifier
        .classify_at(&image, Instant::now() - GAME_STATE_PROBE_INTERVAL)
        .unwrap();
    assert_eq!(cached.kind(), UiStateKind::Unknown);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let candidate = classifier.classify_cached(&image, &cached).unwrap();
    assert_eq!(candidate.label(), "unknown:game_probe");
    let confirmed = classifier.classify_cached(&image, &candidate).unwrap();
    assert_eq!(confirmed.kind(), UiStateKind::GameGate);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // A known fallback result must still be revalidated after its cooldown.
    classifier.game_fallback.retry_after = Some(Instant::now() - Duration::from_secs(1));
    classifier.classify_cached(&image, &confirmed).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
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
    if crate::test_support::corpus::skip_without_corpus(
        "fixed_scrolled_friend_list_uses_the_back_anchor_for_secondary_state",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
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
        assert_eq!(state.kind, UiStateKind::Secondary);
    }

    let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
    let classification = classifier
        .classify(&image)
        .expect("classify fixed secondary-chat screenshot");
    let evidence = classification.evidence();
    assert_eq!(evidence.final_rule(), "secondary_chat_anchor");
    // 探针顺序：secondary_back → main_menu（返回提瓦特）→ secondary_chat_submit
    // （发送圆钮）。派蒙的双锚点探针排在聊天锚点之后：聊天界面先判二级，
    // "不是聊天"才继续查派蒙。
    assert_eq!(evidence.template_probes().len(), 3);
    assert!(
        evidence
            .template_probes()
            .iter()
            .all(|probe| probe.best_score().is_some())
    );
    assert!(evidence.template_probes()[0].hit_rect().is_some());
    assert!(
        evidence.template_probes()[1].hit_rect().is_none(),
        "二级界面上不该出现主菜单的返回提瓦特按钮"
    );
    assert!(
        evidence.template_probes()[2].hit_rect().is_some(),
        "二级聊天必须命中「发送」锚点才能判二级"
    );
}

/// 底栏结构判据：大厅 = 7–14 块**同高、同一条水平线的小圆角白键**
/// （白 ≥225、块高 26–28 px、顶部 y 极差 0）；主菜单 / 设置宫格是整块白底（60–64 px）
/// 或跨两行，提瓦特只有一个 Enter，都不满足。
///
/// 2026-10-17 第三轮：白色判据收紧到 **225**（纯 255 只覆盖 89/182 大厅帧：
/// JPEG/回放/DPI 缩放帧的白色只有 240–254）；再按**实际像素高度 + 同一水平线**判形状：
/// 主菜单白卡高 60–64 px、设置宫格还跨两行（顶部 985/1045），都不满足。
/// 白色判据只认纯白 255，且**不回退**到近白。
///
/// 实测依据（2026-11-06，.119 一级值守连拍 5 帧）：线上无损帧在 255 与 225 下
/// 签名完全一致，所以要严格；而 225 会把近白噪声算成按键，污染状态数据，
/// 因此刻意不兜底。
#[test]
fn hall_bar_white_threshold_prefers_exact_white_and_falls_back_when_scaled() {
    let rect = Rect {
        x: 0,
        y: 985,
        width: 600,
        height: 95,
    };
    let build = |value: u8| {
        let mut canvas = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([0, 0, 0, 255]));
        for index in 0..7u32 {
            let left = 20 + index * 60;
            for y in 995..1021 {
                for x in left..left + 30 {
                    canvas.put_pixel(x, y, image::Rgba([value, value, value, 255]));
                }
            }
        }
        image::DynamicImage::ImageRgba8(canvas)
    };

    // 原样纯白：直接命中，几何量与旧阈值相同。
    assert_eq!(hall_bar_signature(&build(255), rect), (7, 26, 26, 0));
    // 近白（模拟 resize 插值/有损压缩后的白色）：**不认**。
    // 用户明确要求不回退到 225——那会把 240–254 的噪声算成按键、污染状态数据。
    assert_eq!(hall_bar_signature(&build(250), rect), (0, 0, 0, 0));
}

/// 底栏结构判据要把大厅、提瓦特、主菜单、设置宫格分开。
///
/// 2026-11-06 起白色判据只认纯白 255（用户要求：225 会把近白噪声算成按键、污染状态）。
/// 因此这条用例改用**合成帧**：原来的 technical-docs JPEG 夹具本身不含纯白
/// （有损压缩把白色压到 240–254），而且 technical-docs 已 gitignore、CI 里没有这些文件。
/// 真实帧验证改为手工：2026-11-06 在 .119 一级值守连拍 5 帧，
/// 255 与 225 签名完全一致（7 块 / 高 26 / 极差 0）。
#[test]
fn hall_bar_structure_separates_hall_from_menu_and_teyvat() {
    let config = crate::config::bundled_config();
    let rect: Rect = config.screen.hall_bar_rect.into();
    let (origin_x, origin_y) = (rect.x, rect.y);

    // 在底栏区域内画 count 个纯白块。
    let keys = |count: i32, width: i32, height: i32, gap: i32, top_offset: i32| {
        let mut canvas = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([0, 0, 0, 255]));
        for index in 0..count {
            let left = origin_x + 20 + index * (width + gap);
            for y in (origin_y + top_offset)..(origin_y + top_offset + height) {
                for x in left..left + width {
                    canvas.put_pixel(x as u32, y as u32, image::Rgba([255, 255, 255, 255]));
                }
            }
        }
        image::DynamicImage::ImageRgba8(canvas)
    };
    let is_hall = |image: &image::DynamicImage| {
        let (tiles, min_height, max_height, top_spread) = hall_bar_signature(image, rect);
        (HALL_BAR_MIN_TILES..=HALL_BAR_MAX_TILES).contains(&tiles)
            && min_height >= HALL_BAR_MIN_TILE_HEIGHT_PX
            && max_height <= HALL_BAR_MAX_TILE_HEIGHT_PX
            && top_spread <= HALL_BAR_MAX_TILE_TOP_SPREAD_PX
    };

    // 大厅：7 个同高、同一水平线的小白键。
    assert!(is_hall(&keys(7, 30, 26, 30, 10)), "大厅底栏应被判成大厅");

    // 提瓦特：只有一个 Enter。
    assert!(!is_hall(&keys(2, 30, 26, 30, 10)), "提瓦特底栏不应判成大厅");

    // 主菜单：整块白卡（高 60–64 px），块高超过上限。
    let menu = keys(3, 200, 62, 20, 10);
    let (_, menu_min_height, _, _) = hall_bar_signature(&menu, rect);
    assert!(!is_hall(&menu), "主菜单白卡不得判成大厅");
    assert!(
        menu_min_height > HALL_BAR_MAX_TILE_HEIGHT_PX,
        "主菜单白卡的块高应超过键高上限"
    );

    // 设置宫格：分两行，顶部极差远超 4 px。
    let mut grid = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([0, 0, 0, 255]));
    for row in 0..2i32 {
        for index in 0..4i32 {
            let left = origin_x + 20 + index * 60;
            let top = origin_y + 5 + row * 40;
            for y in top..top + 30 {
                for x in left..left + 40 {
                    grid.put_pixel(x as u32, y as u32, image::Rgba([255, 255, 255, 255]));
                }
            }
        }
    }
    let grid = image::DynamicImage::ImageRgba8(grid);
    let (_, _, _, grid_spread) = hall_bar_signature(&grid, rect);
    assert!(!is_hall(&grid), "设置宫格不得判成大厅");
    assert!(
        grid_spread > HALL_BAR_MAX_TILE_TOP_SPREAD_PX,
        "设置宫格应跨两行（顶部极差应大于 4 px）"
    );
}
/// 用户 2026-11-10 明确：千星大厅底栏只有 7~8 个白键（聊天框关 8、开 7），
/// 旧上限 14 把更宽的白色结构也算了进来。合成帧钉住边界 7/8 通过、6/9/10/14 拒绝。
#[test]
fn hall_bottom_bar_accepts_seven_or_eight_keys_only() {
    let config = crate::config::bundled_config();
    let rect: Rect = config.screen.hall_bar_rect.into();
    let (origin_x, origin_y) = (rect.x, rect.y);
    let keys = |count: i32| {
        let mut canvas = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([0, 0, 0, 255]));
        for index in 0..count {
            let left = origin_x + 20 + index * 60;
            for y in (origin_y + 10)..(origin_y + 36) {
                for x in left..left + 30 {
                    canvas.put_pixel(x as u32, y as u32, image::Rgba([255, 255, 255, 255]));
                }
            }
        }
        image::DynamicImage::ImageRgba8(canvas)
    };
    for count in [7, 8] {
        let image = keys(count);
        let (tiles, min_height, max_height, top_spread) = hall_bar_signature(&image, rect);
        assert!(
            hall_bar_looks_like_hall_keys(tiles, min_height, max_height, top_spread),
            "{count} 个白键应判成千星大厅底栏（实测聊天关=8、开=7）"
        );
    }
    for count in [6, 9, 10, 14] {
        let image = keys(count);
        let (tiles, min_height, max_height, top_spread) = hall_bar_signature(&image, rect);
        assert!(
            !hall_bar_looks_like_hall_keys(tiles, min_height, max_height, top_spread),
            "{count} 个白键不得判成千星大厅底栏"
        );
    }
}

/// Overworld 判据（用户 2026-11-10 第二次修正）：**HUD 在场 + 底栏白键 1~4 个**
/// （实测主世界通常 2 个）。旧阈值 `<7` 会把 5–6 个键的帧也放进来，这次收紧。
///
/// 有损 JPEG 帧的白色被压到 240–254，严格 255 会数出 0 个；测试沿用仓库既有口径，
/// 先用 `snap_near_white_to_pure_white` 把白色还原成纯白（等价于线上的无损抓图）再跑判据。
#[test]
fn overworld_requires_hud_and_one_to_four_bottom_keys() {
    if crate::test_support::corpus::skip_without_corpus(
        "overworld_requires_hud_and_one_to_four_bottom_keys",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let hall_rect: Rect = config.screen.hall_bar_rect.into();

    // ── 边界样本：真实提瓦特帧（HUD 在场）上清空底栏后精确画 0..=8 个白键。
    // 1~4 必须通过；0（没证据）与 5/6/7/8（大厅是 7–8）必须拒绝。
    let base = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg").unwrap();
    let (origin_x, origin_y) = (hall_rect.x.max(0) as u32, hall_rect.y.max(0) as u32);
    let frame_with_keys = |count: usize| {
        let mut rgb = base.to_rgb8();
        for y in origin_y..origin_y + hall_rect.height {
            for x in origin_x..origin_x + hall_rect.width {
                rgb.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
        for index in 0..count {
            let left = origin_x + 20 + index as u32 * 60;
            for y in origin_y + 10..origin_y + 36 {
                for x in left..left + 30 {
                    rgb.put_pixel(x, y, image::Rgb([255, 255, 255]));
                }
            }
        }
        image::DynamicImage::ImageRgb8(rgb)
    };
    for count in 1..=4usize {
        let image = frame_with_keys(count);
        let (tiles, _, _, _) = hall_bar_signature(&image, hall_rect);
        assert_eq!(tiles, count, "边界样本应有 {count} 个白键");
        assert!(
            world_is_teyvat_in(&image, hall_rect),
            "{count} 个白键落在 1~4，应判提瓦特大世界"
        );
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        assert_eq!(
            classifier
                .classify_at(&image, Instant::now())
                .unwrap()
                .kind(),
            UiStateKind::Overworld,
            "{count} 个白键的提瓦特帧应判 Overworld"
        );
    }
    for count in [0usize, 5, 6, 7, 8] {
        let image = frame_with_keys(count);
        let (tiles, _, _, _) = hall_bar_signature(&image, hall_rect);
        assert_eq!(tiles, count);
        assert!(
            !world_is_teyvat_in(&image, hall_rect),
            "{count} 个白键不在 1~4，不得判成提瓦特大世界"
        );
    }

    // ── 真实提瓦特帧（含旧青带判据漏掉的 teleport-53 生活帧）：
    // 有损帧 snap 成纯白后底栏应为 1~2 个键 → Overworld。
    for name in [
        "teyvat-regions-58/desert-01.jpg",
        "teyvat-regions-58/snow-01.jpg",
        "teleport-53/live-220.jpg",
        "transition-to-overworld/05-overworld-teyvat.jpg",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let mut image = image::open(&path).unwrap();
        snap_near_white_to_pure_white(&mut image, hall_rect, 225);
        let (tiles, _, _, _) = hall_bar_signature(&image, hall_rect);
        assert!(
            (1..=4).contains(&tiles),
            "{path} 提瓦特底栏白键实测 {tiles} 个，应落在 1~4"
        );
        assert!(
            world_is_teyvat_in(&image, hall_rect),
            "{path} 是提瓦特大世界（HUD 在场 + 底栏 {tiles} 键），必须判 true"
        );
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        assert_eq!(
            classifier
                .classify_at(&image, Instant::now())
                .unwrap()
                .kind(),
            UiStateKind::Overworld,
            "{path} 应判成 Overworld"
        );
    }

    // ── 负样本：千星大厅 7–8 键绝不判 Overworld。
    // hb-01..05 是有损帧：严格 255 只数出 2–4 块（落在 1~4 内！），必须靠
    // 「近白结构 7 块 = 大厅底栏」否决；snap 成纯白后是 7 块，直接超出 1~4。
    for name in [
        "hallbar-56/hb-01.jpg",
        "hallbar-56/hb-02.jpg",
        "hallbar-56/hb-03.jpg",
        "hallbar-56/hb-04.jpg",
        "hallbar-56/hb-05.jpg",
        "qianxing-halls-58/hall2-01.jpg",
        "round78-live2.png",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let raw = image::open(&path).unwrap();
        assert!(
            !world_is_teyvat_in(&raw, hall_rect),
            "{path} 是千星大厅（有损帧），世界判据不得放行"
        );
        let mut snapped = raw.clone();
        snap_near_white_to_pure_white(&mut snapped, hall_rect, 225);
        let (tiles, _, _, _) = hall_bar_signature(&snapped, hall_rect);
        assert!(
            tiles > HALL_BAR_OVERWORLD_MAX_TILES,
            "{path} snap 后应数出大厅的 7–8 键（实际 {tiles}）"
        );
        assert!(
            !world_is_teyvat_in(&snapped, hall_rect),
            "{path} 千星大厅 {tiles} 键不得判 Overworld"
        );
    }
    // 菜单 / F2 面板 / 千星地图 / 二级聊天：HUD 不在场，一律不是 Overworld。
    for name in [
        "wonderland-menu.png",
        "qianxing-ui-59/after-fix-f2.jpg",
        "qianxing-ui-59/qxmap-a.jpg",
        "secondary-full.png",
    ] {
        let path = format!("technical-docs/ui-state-evidence/{name}");
        let image = image::open(&path).unwrap();
        assert!(
            !world_is_teyvat_in(&image, hall_rect),
            "{path} HUD 不在场或非大世界，世界判据不得放行"
        );
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            classification.kind(),
            UiStateKind::Overworld,
            "{path} 不得判成 Overworld（{}）",
            classification.label()
        );
    }
}

/// 二级判据 = 左上圆形返回按钮 **且** 底栏「发送」圆钮。主菜单淡出过渡帧只有
/// 返回按钮（旧实现会判成 Secondary），现在必须落 Unknown；真实大厅二级聊天
/// 与好友会话仍然判 Secondary。
#[test]
fn secondary_chat_requires_the_send_anchor() {
    if crate::test_support::corpus::skip_without_corpus("secondary_chat_requires_the_send_anchor") {
        return;
    }
    let config = crate::config::bundled_config();
    let secondary = [
        "technical-docs/ui-state-evidence/round78-live.png",
        "technical-docs/ui-state-evidence/secondary-full.png",
        "technical-docs/ui-state-evidence/sec-fixture.jpg",
        "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg",
    ];
    for path in secondary {
        let image = image::open(path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            UiStateKind::Secondary,
            "{path} 是真实的二级聊天，必须判 Secondary（实际 {}）",
            classification.label()
        );
        assert_eq!(
            classification.evidence().final_rule(),
            "secondary_chat_anchor",
            "{path} 应报返回按钮 + 聊天锚点规则"
        );
    }
    for path in [
        "technical-docs/ui-state-evidence/step-5.png",
        "technical-docs/ui-state-evidence/step-last.png",
    ] {
        let image = image::open(path).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            classification.kind(),
            UiStateKind::Secondary,
            "{path} 是主菜单淡出帧，只有返回按钮，不得判成二级聊天"
        );
    }
}

fn panel_regions_from(config: &AppConfig) -> PanelRegions {
    PanelRegions {
        friend_panel: config.moderation.friend_panel_region.into(),
        friend_search_panel: config.moderation.search_panel_region.into(),
        friend_block_chat: config.moderation.block_chat_region.into(),
        confirm_button: config.moderation.confirm_button_region.into(),
        confirm_message: config.moderation.confirm_message_region.into(),
        invite_view_star: config.invite.view_star_region.into(),
        invite_goto_hall: config.invite.goto_hall_region.into(),
        invite_enter_hall: config.invite.enter_hall_region.into(),
    }
}

/// 每个真实面板夹具必须被判成**它自己**的标签——判据锚点必须专属该面板。
///
/// 这条用例钉住 2026-10-08 修掉的一个真实缺陷：`friend-more-settings`（⋯）是
/// **共用 chrome**，在档案卡与⋯菜单上落在同一坐标（实测档案卡 0.992 甚至高于菜单
/// 0.959），于是档案卡被误判成"更多设置菜单"。修法是把更多设置菜单的锚点换成
/// **菜单展开时才存在**的「屏蔽聊天」，并按"进入该大厅(大厅卡) → 前往其大厅(档案卡)"
/// 排序，避免大厅卡被档案卡那条暗按钮误命中。
#[test]
fn panel_fixtures_are_labelled_by_their_own_anchor() {
    if crate::test_support::corpus::skip_without_corpus(
        "panel_fixtures_are_labelled_by_their_own_anchor",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let panels = panel_regions_from(&config);
    for (name, label) in [
        ("panel-friend-list-1920x1080.jpg", "panel:friend_list"),
        ("panel-add-friend-1920x1080.jpg", "panel:add_friend"),
        // 逐帧目检：这张是⋯菜单展开；菜单优先级高于底层资料卡。
        ("panel-more-settings-1920x1080.jpg", "panel:more_settings"),
        (
            "panel-confirm-dialog-1920x1080.jpg",
            "panel:confirm_block_player",
        ),
        // 逐帧目检：这张是千星资料卡（奇域等级 + 查看资料），不是提瓦特档案卡。
        (
            "panel-archive-card-1920x1080.jpg",
            "panel:qianxing_profile_friend",
        ),
        ("panel-hall-card-1920x1080.jpg", "panel:hall_card"),
        ("panel-avatar-popup-1920x1080.jpg", "panel:avatar_popup"),
    ] {
        let path = format!("tests/fixtures/ui/{name}");
        let image = image::open(&path).unwrap();
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panels);
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            UiStateKind::Panel,
            "{path} 应判为 panel，实际是 {}",
            classification.kind().name()
        );
        assert_eq!(
            classification.label(),
            label,
            "{path} 的面板标签错了——锚点不专属该面板"
        );
    }
}

/// 确认弹窗的正样本：按钮 + 对应关键词文案必须判成对应子类型。
#[test]
fn confirm_dialogs_are_classified_by_button_and_message() {
    if crate::test_support::corpus::skip_without_corpus(
        "confirm_dialogs_are_classified_by_button_and_message",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let panels = panel_regions_from(&config);
    for (path, label) in [
        (
            "technical-docs/ui-state-evidence/cold-to-qianxing-59/c05-region-confirm.jpg",
            "panel:confirm_enter_hall",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-entry-56/03-confirm-dialog.jpg",
            "panel:confirm_enter_hall",
        ),
        (
            "technical-docs/ui-state-evidence/exit-to-teyvat-56/02-leave-confirm.jpg",
            "panel:confirm_leave_hall",
        ),
        (
            "technical-docs/ui-state-evidence/leave-hall-confirm.jpg",
            "panel:confirm_leave_hall",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r12-ctrl-return.jpg",
            "panel:confirm_leave_hall",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r12-ctrl-team.jpg",
            "panel:confirm_leave_hall",
        ),
        (
            "technical-docs/ui-state-evidence/transition-to-overworld/03-leave-confirm.jpg",
            "panel:confirm_leave_hall",
        ),
        (
            "tests/fixtures/ui/panel-confirm-dialog-1920x1080.jpg",
            "panel:confirm_block_player",
        ),
    ] {
        let image = image::open(path).unwrap();
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panels);
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            UiStateKind::Panel,
            "{path} 应判为 panel，实际是 {}",
            classification.kind().name()
        );
        assert_eq!(classification.label(), label, "{path} 的确认弹窗子类型错了");
    }
}

fn fill_rgb_rect(image: &mut image::RgbImage, rect: Rect, color: image::Rgb<u8>) {
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = (rect.x + rect.width as i32).min(image.width() as i32);
    let y1 = (rect.y + rect.height as i32).min(image.height() as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            image.put_pixel(x as u32, y as u32, color);
        }
    }
}

/// 负样本：确认按钮在但文案被去掉 → 不判任何确认子类型（绝不回退单按钮标签）。
/// 文案在但确认按钮被去掉 → 同样不判。文案换成「屏蔽聊天」也不得判确认。
#[test]
fn confirm_dialog_requires_both_button_and_message_anchors() {
    if crate::test_support::corpus::skip_without_corpus(
        "confirm_dialog_requires_both_button_and_message_anchors",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let panels = panel_regions_from(&config);
    let source = image::open("tests/fixtures/ui/panel-confirm-dialog-1920x1080.jpg")
        .unwrap()
        .to_rgb8();
    let background = *source.get_pixel(700, 650);
    let message_region = Rect::new(650, 460, 620, 90);
    let classify = |image: &image::RgbImage| {
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panels);
        classifier
            .classify_at(&DynamicImage::ImageRgb8(image.clone()), Instant::now())
            .unwrap()
            .label()
            .to_string()
    };

    // 只有确认按钮：抹掉整行文案。
    let mut only_button = source.clone();
    fill_rgb_rect(&mut only_button, message_region, background);
    let label = classify(&only_button);
    assert!(
        !label.starts_with("panel:confirm_"),
        "只有确认按钮、没有文案锚点，不得判确认弹窗，实际 {label}"
    );

    // 只有文案：抹掉确认按钮。
    let mut only_message = source.clone();
    fill_rgb_rect(&mut only_message, panels.confirm_button, background);
    let label = classify(&only_message);
    assert!(
        !label.starts_with("panel:confirm_"),
        "只有文案、没有确认按钮，不得判确认弹窗，实际 {label}"
    );

    // 文案换成其它文字（「屏蔽聊天」）：不得判确认。
    let other = image::open("tests/fixtures/ui/panel-more-settings-1920x1080.jpg")
        .unwrap()
        .to_rgb8();
    let patch = image::imageops::crop_imm(&other, 1380, 360, 170, 70).to_image();
    let mut replaced = source;
    fill_rgb_rect(&mut replaced, message_region, background);
    image::imageops::overlay(&mut replaced, &patch, 900, 480);
    let label = classify(&replaced);
    assert!(
        !label.starts_with("panel:confirm_"),
        "文案换成「屏蔽聊天」后不得判确认弹窗，实际 {label}"
    );
}

/// 主菜单大世界**布局无关**判定：返回按钮命中 + 无「返回提瓦特」+ 无聊天发送锚点 +
/// 左侧「多瓦片宫格结构」（downscale 颜色 + 连通域）。不再依赖「队伍配置/成就」
/// 固定坐标、固定列距、商城图标或 OCR。
#[test]
fn paimon_menu_is_detected_by_layout_independent_grid() {
    if crate::test_support::corpus::skip_without_corpus(
        "paimon_menu_is_detected_by_layout_independent_grid",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    for path in [
        "tests/fixtures/ui/menu-paimon-1920x1080.jpg",
        "technical-docs/ui-state-evidence/sweep-49/01-paimon-menu.jpg",
        "technical-docs/ui-state-evidence/sweep-49/10-F2.jpg",
        "technical-docs/ui-state-evidence/sweep-49/10-F2b.jpg",
        "technical-docs/ui-state-evidence/sweep-50/01-map.jpg",
        "technical-docs/ui-state-evidence/sweep-50/02-waypoint-selected.jpg",
        "technical-docs/ui-state-evidence/sweep-50/06-F2-real.jpg",
    ] {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            UiStateKind::MainMenu,
            "{path} 被误判成 {}",
            classification.label()
        );
        assert_eq!(
            classification.evidence().final_rule(),
            "paimon_menu_grid",
            "{path} 应报宫格结构规则"
        );
    }
    // 负样本覆盖：主菜单千星/淡出帧/hub 过渡/二级·当前大厅/二级·好友私聊/
    // F2 多人面板/千星地图/覆盖层/Loading/Unknown。
    for path in [
        "technical-docs/ui-state-evidence/wonderland-menu.png",
        "technical-docs/ui-state-evidence/settled-3.png",
        "technical-docs/ui-state-evidence/final.png",
        "technical-docs/ui-state-evidence/restored.png",
        "technical-docs/ui-state-evidence/modR-5.png",
        "technical-docs/ui-state-evidence/step-5.png",
        "technical-docs/ui-state-evidence/step-last.png",
        "technical-docs/ui-state-evidence/transition-to-overworld/02-hub-menu.jpg",
        "technical-docs/ui-state-evidence/qianxing-entry-56/05-hub-menu.jpg",
        "technical-docs/ui-state-evidence/round78-live.png",
        "technical-docs/ui-state-evidence/secondary-full.png",
        "technical-docs/ui-state-evidence/sec-fixture.jpg",
        "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/after-fix-f2.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/qxmap-a.jpg",
        "technical-docs/ui-state-evidence/map-screen.png",
        "technical-docs/ui-state-evidence/map-via-M.png",
        "technical-docs/ui-state-evidence/more-settings.png",
        "technical-docs/ui-state-evidence/friend-panel-via-O.png",
        "technical-docs/ui-state-evidence/journey-page.png",
        "technical-docs/ui-state-evidence/loading-screen.jpg",
        "technical-docs/ui-state-evidence/loading-screens/00-now.jpg",
        "technical-docs/ui-state-evidence/blank-frame.jpg",
        "technical-docs/ui-state-evidence/hud-hidden.png",
        "technical-docs/ui-state-evidence/cat-paimon-menu.png",
        "technical-docs/ui-state-evidence/hall-page.png",
    ] {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            classification.evidence().final_rule(),
            "paimon_menu_grid",
            "{path} 不是主菜单大世界，不得被宫格结构判成 MainMenu"
        );
    }
}

/// 布局无关性：菜单整体**平移**、菜单项**增删**后仍能命中。
/// 旧方案（固定「队伍配置/成就」坐标 + 列距 325）在这些变换下会失效。
#[test]
fn paimon_grid_survives_translation_and_item_changes() {
    if crate::test_support::corpus::skip_without_corpus(
        "paimon_grid_survives_translation_and_item_changes",
    ) {
        return;
    }
    let base = image::open("tests/fixtures/ui/menu-paimon-1920x1080.jpg")
        .unwrap()
        .to_rgb8();
    let (width, height) = base.dimensions();
    let as_image = |rgb: image::RgbImage| DynamicImage::ImageRgb8(rgb);
    let baseline = paimon_menu_grid_structure(&as_image(base.clone()))
        .expect("真实主菜单大世界帧应命中宫格结构");

    // 整体平移：整帧内容移动 (+37,+23) / (-41,-29)，结构不变。
    for (dx, dy) in [(37_i64, 23_i64), (-41, -29)] {
        let mut canvas = image::RgbImage::from_pixel(width, height, image::Rgb([0, 0, 0]));
        image::imageops::overlay(&mut canvas, &base, dx, dy);
        let shifted = paimon_menu_grid_structure(&as_image(canvas))
            .unwrap_or_else(|| panic!("平移 ({dx},{dy}) 后应仍命中"));
        assert!(shifted.0 >= PAIMON_GRID_MIN_TILES);
    }

    // 菜单项减少：抹掉整行前两块瓦片（用面板浅底色覆盖），仍应命中且块数减少。
    let mut removed = base.clone();
    let panel = image::Rgb([237, 229, 216]);
    for y in 340..500 {
        for x in 270..600 {
            removed.put_pixel(x, y, panel);
        }
    }
    let removed_hit = paimon_menu_grid_structure(&as_image(removed))
        .expect("删除两块菜单项后仍应命中（剩余宫格 ≥8 块）");
    assert!(
        removed_hit.0 < baseline.0,
        "删除菜单项后块数应减少：{} vs {}",
        removed_hit.0,
        baseline.0
    );

    // 菜单项增加：把一块真实瓦片（含四周留白，保证独立连通域）复制到玩家卡片区，
    // 块数应增加。旧方案靠固定坐标找指定瓦片，这一步无从谈起。
    let mut added = base.clone();
    for y in 0..160 {
        for x in 0..160 {
            let pixel = base.get_pixel(272 + x, 344 + y);
            added.put_pixel(272 + x, 150 + y, *pixel);
        }
    }
    let added_hit = paimon_menu_grid_structure(&as_image(added)).expect("增加菜单项后仍应命中");
    assert!(
        added_hit.0 > baseline.0,
        "增加菜单项后块数应增加：{} vs {}",
        added_hit.0,
        baseline.0
    );
}

/// 检测必须**快**：1920×1080 真实 Rgb8 帧上多次取均值。
/// debug 构建给宽松上限（CI 的 debug 几倍慢），release 断言目标 <5ms。
#[test]
fn paimon_grid_detection_is_fast() {
    if crate::test_support::corpus::skip_without_corpus("paimon_grid_detection_is_fast") {
        return;
    }
    let rgb = image::open("tests/fixtures/ui/menu-paimon-1920x1080.jpg")
        .unwrap()
        .to_rgb8();
    let image = DynamicImage::ImageRgb8(rgb);
    let runs = 200;
    let started = Instant::now();
    for _ in 0..runs {
        assert!(paimon_menu_grid_structure(&image).is_some());
    }
    let mean_ms = started.elapsed().as_secs_f64() * 1000.0 / runs as f64;
    println!("paimon_menu_grid_structure: {mean_ms:.3} ms/frame @1920x1080 (runs={runs})");
    let bound_ms = if cfg!(debug_assertions) { 50.0 } else { 5.0 };
    assert!(
        mean_ms < bound_ms,
        "宫格检测太慢：{mean_ms:.3} ms/frame（上限 {bound_ms}ms）"
    );
}

/// task-119：好友列表新变体（申请加入/前往其大厅）判 Panel·好友面板；
/// 旧版式（互动按钮）继续靠 friend_panel 模板命中；hall 页/主菜单/加载/大厅不得误判。
#[test]
fn friend_list_new_variant_is_panel_and_old_layout_still_works() {
    if crate::test_support::corpus::skip_without_corpus(
        "friend_list_new_variant_is_panel_and_old_layout_still_works",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let classify = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panel_regions_from(&config));
        classifier.classify_at(&image, Instant::now()).unwrap()
    };
    // 新变体与旧版式都必须是 Panel·好友面板。
    for path in [
        "technical-docs/ui-state-evidence/friend-list-goto-hall.jpg",
        "technical-docs/ui-state-evidence/friend-list.png",
        "technical-docs/ui-state-evidence/friend-panel-via-O.png",
        "tests/fixtures/ui/panel-friend-list-1920x1080.jpg",
    ] {
        let classification = classify(path);
        assert_eq!(
            classification.kind(),
            UiStateKind::Panel,
            "{path} 应为 Panel"
        );
        assert_eq!(
            classification.label(),
            "panel:friend_list",
            "{path} 应为好友面板"
        );
    }
    // 负样本：不能被好友列表锚点吃掉。
    for (path, kind) in [
        (
            "technical-docs/ui-state-evidence/hall-page.png",
            UiStateKind::Secondary,
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/after-fix-f2.jpg",
            UiStateKind::Secondary,
        ),
        (
            "technical-docs/ui-state-evidence/wonderland-menu.png",
            UiStateKind::MainMenu,
        ),
        (
            "technical-docs/ui-state-evidence/round78-live.png",
            UiStateKind::Secondary,
        ),
        (
            "technical-docs/ui-state-evidence/full-hall.jpg",
            UiStateKind::Primary,
        ),
        (
            "technical-docs/ui-state-evidence/loading-screen.jpg",
            UiStateKind::Loading,
        ),
        (
            "technical-docs/ui-state-evidence/loading-screens/after-x.jpg",
            UiStateKind::Unknown,
        ),
        (
            "tests/fixtures/ui/menu-paimon-1920x1080.jpg",
            UiStateKind::MainMenu,
        ),
    ] {
        let classification = classify(path);
        assert_eq!(
            classification.kind(),
            kind,
            "{path} kind 变了：{}",
            classification.label()
        );
        assert_ne!(
            classification.label(),
            "panel:friend_list",
            "{path} 被好友列表锚点误判"
        );
    }
}

// ===== task-113：全量语料提交门禁（默认 #[ignore]，本地提交前用 scripts/corpus-gate.sh 跑）=====

/// transitional 的显式白名单：只能这 4 张，且 `label_source` 必须带理由。
const TRANSITIONAL_WHITELIST: [&str; 4] = [
    "technical-docs/ui-state-evidence/cold-to-qianxing-59/020-t398.jpg",
    "technical-docs/ui-state-evidence/step-5.png",
    "technical-docs/ui-state-evidence/step-last.png",
    "technical-docs/ui-state-evidence/transition-to-overworld/04-after-confirm.jpg",
];

/// 门禁总耗时预算（debug 构建，含解码；task-112 release 基线 mean≈48ms/张全量）。
/// 超出即失败——防止分类器悄悄变慢导致提交前门禁不可用。
const CORPUS_GATE_MAX_TOTAL_MS: u128 = 900_000;

fn expected_kind_for_manifest_label(label: &str) -> Option<UiStateKind> {
    if label.starts_with("覆盖层") {
        return Some(UiStateKind::Panel);
    }
    let table: [(&str, UiStateKind); 8] = [
        ("千星大厅", UiStateKind::Primary),
        ("二级聊天", UiStateKind::Secondary),
        ("二级界面", UiStateKind::Secondary),
        ("主菜单千星", UiStateKind::MainMenu),
        ("主菜单大世界", UiStateKind::MainMenu),
        ("加载界面", UiStateKind::Loading),
        ("提瓦特大世界", UiStateKind::Overworld),
        ("登录入口", UiStateKind::GameGate),
    ];
    table
        .iter()
        .find(|(prefix, _)| label.contains(prefix))
        .map(|(_, kind)| *kind)
}

/// task-125 严格 surface 门禁：kind 之外还必须比较具体界面子型。
/// 说明：`ui::routines::state_observation::surface_of` 是唯一映射，但该模块对
/// `ui::state` 私有（改 mod.rs 属 task-125 写域之外），故在门禁测试内镜像同一映射；
/// 若那边映射变更必须同步这里。`expected` 来自 manifest 的人工标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GateSurface {
    PrimaryHall,
    SecondaryChat,
    SecondaryMultiplayerPanel,
    SecondaryHallMap,
    OverlayFriendPanel,
    OverlaySearchPage,
    OverlayMoreSettings,
    OverlayBlockList,
    OverlayConfirmEnterHall,
    OverlayConfirmLeaveHall,
    OverlayConfirmBlockPlayer,
    OverlayProfileCard,
    OverlayTeyvatProfileCardNonFriend,
    OverlayTeyvatProfileCardFriend,
    OverlayQianxingProfileCardNonFriend,
    OverlayQianxingProfileCardFriend,
    OverlayProfilePage,
    OverlayHallCard,
    OverlayAvatarPopup,
    OverlayOther,
    WonderlandMainMenu,
    PaimonMenu,
    Loading,
    Overworld,
    GameGate,
    Unknown,
}

impl GateSurface {
    fn name(self) -> &'static str {
        match self {
            Self::PrimaryHall => "PrimaryHall",
            Self::SecondaryChat => "Secondary(Chat)",
            Self::SecondaryMultiplayerPanel => "Secondary(MultiplayerPanel)",
            Self::SecondaryHallMap => "Secondary(HallMap)",
            Self::OverlayFriendPanel => "Overlay(FriendPanel)",
            Self::OverlaySearchPage => "Overlay(SearchPage)",
            Self::OverlayMoreSettings => "Overlay(MoreSettings)",
            Self::OverlayBlockList => "Overlay(BlockList)",
            Self::OverlayConfirmEnterHall => "Overlay(ConfirmEnterHall)",
            Self::OverlayConfirmLeaveHall => "Overlay(ConfirmLeaveHall)",
            Self::OverlayConfirmBlockPlayer => "Overlay(ConfirmBlockPlayer)",
            Self::OverlayProfileCard => "Overlay(ProfileCard)",
            Self::OverlayTeyvatProfileCardNonFriend => "Overlay(TeyvatProfileCardNonFriend)",
            Self::OverlayTeyvatProfileCardFriend => "Overlay(TeyvatProfileCardFriend)",
            Self::OverlayQianxingProfileCardNonFriend => "Overlay(QianxingProfileCardNonFriend)",
            Self::OverlayQianxingProfileCardFriend => "Overlay(QianxingProfileCardFriend)",
            Self::OverlayProfilePage => "Overlay(ProfilePage)",
            Self::OverlayHallCard => "Overlay(HallCard)",
            Self::OverlayAvatarPopup => "Overlay(AvatarPopup)",
            Self::OverlayOther => "Overlay(Other)",
            Self::WonderlandMainMenu => "WonderlandMainMenu",
            Self::PaimonMenu => "PaimonMenu",
            Self::Loading => "Loading",
            Self::Overworld => "Overworld",
            Self::GameGate => "GameGate",
            Self::Unknown => "Unknown",
        }
    }
}

fn gate_secondary_from_label(label: &str) -> GateSurface {
    let tail = label.rsplit(':').next().unwrap_or(label);
    if tail == "panel" || tail.ends_with("_panel") {
        GateSurface::SecondaryMultiplayerPanel
    } else if tail == "map" || tail.ends_with("_map") {
        GateSurface::SecondaryHallMap
    } else {
        GateSurface::SecondaryChat
    }
}

fn gate_overlay_from_label(label: &str) -> GateSurface {
    match label {
        "panel:friend_list" => GateSurface::OverlayFriendPanel,
        "panel:add_friend" | "panel:search" => GateSurface::OverlaySearchPage,
        "panel:more_settings" => GateSurface::OverlayMoreSettings,
        "panel:block_list" | "panel:block_chat" => GateSurface::OverlayBlockList,
        "panel:confirm_enter_hall" => GateSurface::OverlayConfirmEnterHall,
        "panel:confirm_leave_hall" => GateSurface::OverlayConfirmLeaveHall,
        "panel:confirm_block_player" => GateSurface::OverlayConfirmBlockPlayer,
        "panel:archive_card" => GateSurface::OverlayProfileCard,
        "panel:teyvat_profile_nonfriend" => GateSurface::OverlayTeyvatProfileCardNonFriend,
        "panel:teyvat_profile_friend" => GateSurface::OverlayTeyvatProfileCardFriend,
        "panel:qianxing_profile_nonfriend" => GateSurface::OverlayQianxingProfileCardNonFriend,
        "panel:qianxing_profile_friend" => GateSurface::OverlayQianxingProfileCardFriend,
        "panel:profile_page" => GateSurface::OverlayProfilePage,
        "panel:hall_card" => GateSurface::OverlayHallCard,
        "panel:avatar_popup" => GateSurface::OverlayAvatarPopup,
        _ => GateSurface::OverlayOther,
    }
}

/// 镜像 `state_observation::surface_of`（kind/label/final_rule → surface）。
fn gate_predicted_surface(kind: UiStateKind, label: &str, final_rule: &str) -> GateSurface {
    match kind {
        UiStateKind::Primary => GateSurface::PrimaryHall,
        UiStateKind::Secondary => gate_secondary_from_label(label),
        UiStateKind::Panel => gate_overlay_from_label(label),
        UiStateKind::MainMenu => {
            if final_rule == "paimon_menu_grid" {
                GateSurface::PaimonMenu
            } else {
                GateSurface::WonderlandMainMenu
            }
        }
        UiStateKind::Loading => GateSurface::Loading,
        UiStateKind::Overworld => GateSurface::Overworld,
        UiStateKind::GameGate => GateSurface::GameGate,
        UiStateKind::Unknown => GateSurface::Unknown,
    }
}

fn gate_expected_surface(label: &str) -> Option<GateSurface> {
    if label.starts_with("覆盖层") {
        let surface = if label.contains("好友面板") {
            GateSurface::OverlayFriendPanel
        } else if label.contains("搜索页") {
            GateSurface::OverlaySearchPage
        } else if label.contains("更多设置") {
            GateSurface::OverlayMoreSettings
        } else if label.contains("拉黑") {
            GateSurface::OverlayBlockList
        } else if label.contains("确认弹窗·进入随机大厅") {
            GateSurface::OverlayConfirmEnterHall
        } else if label.contains("确认弹窗·离开大厅") {
            GateSurface::OverlayConfirmLeaveHall
        } else if label.contains("确认弹窗·屏蔽玩家") {
            GateSurface::OverlayConfirmBlockPlayer
        } else if label.contains("提瓦特资料卡·非好友") {
            GateSurface::OverlayTeyvatProfileCardNonFriend
        } else if label.contains("提瓦特资料卡·好友") {
            GateSurface::OverlayTeyvatProfileCardFriend
        } else if label.contains("千星资料卡·非好友") {
            GateSurface::OverlayQianxingProfileCardNonFriend
        } else if label.contains("千星资料卡·好友") {
            GateSurface::OverlayQianxingProfileCardFriend
        } else if label.contains("档案卡") {
            GateSurface::OverlayProfileCard
        } else if label.contains("个人资料页") {
            GateSurface::OverlayProfilePage
        } else if label.contains("大厅卡") {
            GateSurface::OverlayHallCard
        } else if label.contains("头像弹窗") {
            GateSurface::OverlayAvatarPopup
        } else {
            // 分类器没有专属判据的覆盖层：strict 只要求 Panel/Other，不猜具体页。
            GateSurface::OverlayOther
        };
        return Some(surface);
    }
    let table: [(&str, GateSurface); 10] = [
        ("千星大厅", GateSurface::PrimaryHall),
        ("二级聊天", GateSurface::SecondaryChat),
        (
            "二级界面·千星多人面板",
            GateSurface::SecondaryMultiplayerPanel,
        ),
        ("二级界面·千星地图", GateSurface::SecondaryHallMap),
        ("主菜单千星", GateSurface::WonderlandMainMenu),
        ("主菜单大世界", GateSurface::PaimonMenu),
        ("加载界面", GateSurface::Loading),
        ("提瓦特大世界", GateSurface::Overworld),
        ("登录入口", GateSurface::GameGate),
        ("过渡帧", GateSurface::Unknown),
    ];
    table
        .iter()
        .find(|(prefix, _)| label.contains(prefix))
        .map(|(_, surface)| *surface)
}

#[test]
#[ignore = "全量图像分类提交门禁：需 technical-docs/ui-state-corpus；用 scripts/corpus-gate.sh 一键运行"]
fn corpus_manifest_confirmed_classification_gate() {
    // 允许用 env 指向 manifest 副本（诊断/预演）；默认就是仓库内最终清单。
    let manifest_path = std::env::var_os("MILIASTRA_CORPUS_MANIFEST")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new("technical-docs/ui-state-corpus/manifest.tsv").to_path_buf());
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
        panic!(
            "不可提交：门禁失败：读不到 manifest {}（不允许跳过）：{error}",
            manifest_path.display()
        )
    });
    // 不可提交：门禁失败：测试配置加载失败
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    // 与生产同一条构造路径（composition/application.rs）：面板判据必须注入区域，否则
    // 覆盖层会全部落回 Unknown，门禁会把"没接线"误报成"分类错误"。
    let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone())
        .with_panel_regions(PanelRegions {
            friend_panel: config.moderation.friend_panel_region.into(),
            friend_search_panel: config.moderation.search_panel_region.into(),
            friend_block_chat: config.moderation.block_chat_region.into(),
            confirm_button: config.moderation.confirm_button_region.into(),
            confirm_message: config.moderation.confirm_message_region.into(),
            invite_view_star: config.invite.view_star_region.into(),
            invite_goto_hall: config.invite.goto_hall_region.into(),
            invite_enter_hall: config.invite.enter_hall_region.into(),
        });

    let mut confirmed = 0usize;
    let mut not_state = 0usize;
    let mut transitional = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    let mut surface_mismatches: Vec<String> = Vec::new();
    let mut transitional_seen: Vec<String> = Vec::new();
    let mut invalid_status: Vec<String> = Vec::new();
    let mut decode_failures: Vec<String> = Vec::new();
    // GameGate 判据依赖 Windows OCR（enter_game 文本识别），离线 Linux 测试无法复现；
    // 显式单列、计数并如实失败/告警，绝不当成"分类通过"。
    let mut ocr_dependent_gamegate: Vec<String> = Vec::new();
    let started = Instant::now();
    let mut classify_ms = 0u128;

    for (line_number, line) in manifest.lines().enumerate() {
        if line_number == 0 || line.is_empty() {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            columns.len(),
            13,
            "不可提交：门禁失败：manifest 第 {} 行列数不是 13: {:?}",
            line_number + 1,
            line
        );
        let path = columns[0];
        let expected_label = columns[9];
        let status = columns[10];
        let label_source = columns[11];
        match status {
            "confirmed" => {
                confirmed += 1;
                assert!(
                    !expected_label.trim().is_empty(),
                    "不可提交：门禁失败：confirmed 行 expected 为空: {path}"
                );
                let expected_kind =
                        expected_kind_for_manifest_label(expected_label).unwrap_or_else(|| {
                            panic!(
                                "不可提交：门禁失败：confirmed 标签无法映射到运行时 kind: {path} -> {expected_label}"
                            )
                        });
                let image = match image::open(path) {
                    Ok(image) => image,
                    Err(error) => {
                        // 不静默跳过：记入清单并在末尾失败；anomaly 帮助区分"损坏"还是"格式不符"。
                        decode_failures.push(format!("{path} | anomaly={} | {error}", columns[12]));
                        continue;
                    }
                };
                let classify_started = Instant::now();
                let classification = classifier
                    .classify_at(&image, Instant::now())
                    .unwrap_or_else(|error| {
                        panic!("不可提交：门禁失败：confirmed 图片分类出错 {path}: {error}")
                    });
                classify_ms = classify_ms.saturating_add(classify_started.elapsed().as_millis());
                let expected_surface = gate_expected_surface(expected_label).unwrap_or_else(|| {
                    panic!("门禁失败：confirmed 标签无法映射到 surface: {path} -> {expected_label}")
                });
                let predicted_surface = gate_predicted_surface(
                    classification.kind(),
                    classification.label(),
                    classification.evidence().final_rule(),
                );
                if expected_kind == UiStateKind::GameGate {
                    // GameGate 判据依赖 Windows OpenVINO OCR（enter_game 文本）；
                    // Linux 离线不能给可信 predicted：单列"OCR 依赖"清单，逐条打印+计数，
                    // 不计为通过、也不混进 Linux mismatch 断言。Windows/带 OCR 环境另行核验。
                    ocr_dependent_gamegate.push(format!(
                            "{path} | predicted={}({}) | expected=game_gate({expected_label}) | deferred=windows_ocr",
                            classification.kind().name(),
                            classification.label()
                        ));
                    continue;
                }
                if predicted_surface != expected_surface {
                    surface_mismatches.push(format!(
                        "{path} | predicted={}(label={},rule={}) | expected={} | label={}",
                        predicted_surface.name(),
                        classification.label(),
                        classification.evidence().final_rule(),
                        expected_surface.name(),
                        expected_label
                    ));
                }
                if classification.kind() != expected_kind {
                    mismatches.push(format!(
                        "{path} | predicted={}({}) | expected={}({})",
                        classification.kind().name(),
                        classification.label(),
                        expected_kind.name(),
                        expected_label
                    ));
                }
            }
            "not_state" => not_state += 1,
            "transitional" => {
                transitional += 1;
                assert!(
                    !label_source.trim().is_empty(),
                    "不可提交：门禁失败：transitional 必须带理由（label_source 为空）: {path}"
                );
                assert!(
                    TRANSITIONAL_WHITELIST.contains(&path),
                    "不可提交：门禁失败：transitional 不在显式白名单: {path}"
                );
                transitional_seen.push(path.to_string());
            }
            other => invalid_status.push(format!("{path} | status={other}")),
        }
    }
    let total_ms = started.elapsed().as_millis();

    // 先把关键结果落盘，便于失败时把 mismatch 清单发给 lead（即使后续统计断言失败也可取）。
    let report = format!(
        "confirmed={confirmed} not_state={not_state} transitional={transitional}\n\
             mismatches={}\n{}\n\
             surface_mismatches={}\n{}\n\
             decode_failures={}\n{}\n\
             OCR_DEPENDENT_GAMEGATE={}\n{}\n\
             invalid_status={}\n{}",
        mismatches.len(),
        mismatches.join("\n"),
        surface_mismatches.len(),
        surface_mismatches.join("\n"),
        decode_failures.len(),
        decode_failures.join("\n"),
        ocr_dependent_gamegate.len(),
        ocr_dependent_gamegate.join("\n"),
        invalid_status.len(),
        invalid_status.join("\n"),
    );
    let _ = std::fs::write("target/corpus-gate-report.txt", &report);
    assert!(
        decode_failures.is_empty(),
        "不可提交：门禁失败：{} 张 confirmed 图片无法解码（manifest 需修正或移出 confirmed，禁止静默跳过）：\n{}",
        decode_failures.len(),
        decode_failures.join("\n")
    );
    assert!(
        mismatches.is_empty(),
        "不可提交：门禁失败：{}/{} 张 confirmed 分类错误（predicted != expected）：\n{}",
        mismatches.len(),
        confirmed,
        mismatches.join("\n")
    );
    assert!(
        surface_mismatches.is_empty(),
        "不可提交：strict surface 门禁失败：{}/{} 张 confirmed 的 predicted surface != expected surface：\n{}",
        surface_mismatches.len(),
        confirmed,
        surface_mismatches.join("\n")
    );
    assert!(
        invalid_status.is_empty(),
        "不可提交：门禁失败：manifest 出现非 confirmed/not_state/transitional 状态（unlabeled/whitelisted/uncertain 一律失败）：\n{}",
        invalid_status.join("\n")
    );
    assert_eq!(
        transitional,
        TRANSITIONAL_WHITELIST.len(),
        "不可提交：门禁失败：transitional 数量与白名单不一致（manifest={transitional}，白名单={}）",
        TRANSITIONAL_WHITELIST.len()
    );
    assert_eq!(
        ocr_dependent_gamegate.len(),
        22,
        "不可提交：门禁失败：GameGate(OCR 依赖) 数量应为 22，实际 {}；清单变化时需同步门禁基线：\n{}",
        ocr_dependent_gamegate.len(),
        ocr_dependent_gamegate.join("\n")
    );
    assert!(
        confirmed >= 750,
        "不可提交：门禁失败：confirmed 数量低于预期基线（≥750），疑似 manifest 被截断：{confirmed}"
    );
    assert!(
        total_ms <= CORPUS_GATE_MAX_TOTAL_MS,
        "不可提交：门禁失败：耗时 {total_ms}ms 超过预算 {CORPUS_GATE_MAX_TOTAL_MS}ms（classify={classify_ms}ms）"
    );
    println!(
        "corpus-gate PASS: confirmed={confirmed} not_state={not_state} transitional={transitional} kind_mismatches=0 surface_mismatches=0 OCR_DEPENDENT_GAMEGATE={} total={total_ms}ms classify={classify_ms}ms mean={:.2}ms",
        ocr_dependent_gamegate.len(),
        classify_ms as f64 / confirmed.max(1) as f64
    );
    for deferred in &ocr_dependent_gamegate {
        println!("corpus-gate OCR_DEPENDENT_GAMEGATE (Windows 另行核验): {deferred}");
    }
}

/// task-121：21 张非 OCR mismatch 的回归（千星覆盖页/亮底加载/亮底千星地图/派蒙误标）。
#[test]
fn task121_overlay_loading_and_bright_map_regressions() {
    if crate::test_support::corpus::skip_without_corpus(
        "task121_overlay_loading_and_bright_map_regressions",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let classify = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panel_regions_from(&config));
        classifier.classify_at(&image, Instant::now()).unwrap()
    };
    // 千星全屏覆盖页：右上白色关闭按钮（不依赖单个页面文案）。
    for path in [
        "technical-docs/ui-state-evidence/modF-11.png",
        "technical-docs/ui-state-evidence/modF-14.png",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-cal-300.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-cal-420.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-cal-540.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-好友.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r17-纪游.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r0c0-qianxing-shop.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r1c1-dressup.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r1c2-friend.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r2c1-change-char.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r2c2-songyuan.jpg",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r2c3-jiyou.jpg",
        "technical-docs/ui-state-evidence/wonderland-collection.png",
    ] {
        let classification = classify(path);
        assert_eq!(
            classification.kind(),
            UiStateKind::Panel,
            "{path} 应是覆盖层，实际 {}",
            classification.label()
        );
    }
    // 亮底加载变体（正中 spinner）。
    assert_eq!(
        classify("technical-docs/ui-state-evidence/cold-to-qianxing-59/001-t000.jpg").kind(),
        UiStateKind::Loading
    );
    // 千星大厅内漫游 HUD：底栏 8 个混合高度白键（Enter/Alt+N 长条 + 图标方键），
    // 中心线对齐 → Primary（而不是靠改标签绕过）。
    assert_eq!(
        classify("technical-docs/ui-state-evidence/final-frame.jpg").kind(),
        UiStateKind::Primary
    );
    // HUD 完全隐藏的千星大厅场景：没有任何界面判据，保持 Unknown（manifest not_state）。
    assert_eq!(
        classify("technical-docs/ui-state-evidence/hud-hidden.png").kind(),
        UiStateKind::Unknown
    );
    // 亮底千星地图变体。
    assert_eq!(
        classify("technical-docs/ui-state-evidence/map-screen.png").kind(),
        UiStateKind::Secondary
    );
    // manifest 误标（实为主菜单大世界）。
    for path in [
        "technical-docs/mic-primary-residency-shots/03-before-failure-2338-live.png",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r16-settings-grid.png",
        "technical-docs/ui-state-evidence/qianxing-ui-59/r16-settings.jpg",
    ] {
        assert_eq!(
            classify(path).kind(),
            UiStateKind::MainMenu,
            "{path} 实为主菜单大世界"
        );
    }
    // 负样本：提瓦特帧/主菜单千星/大厅/覆盖层不得被新判据抢走。
    for (path, kind) in [
        (
            "technical-docs/ui-state-evidence/loading-screens/ac-012.jpg",
            UiStateKind::Overworld,
        ),
        (
            "technical-docs/ui-state-evidence/exit-to-teyvat-56/01-hub-menu.jpg",
            UiStateKind::MainMenu,
        ),
        (
            "technical-docs/ui-state-evidence/transition-to-overworld/02-hub-menu.jpg",
            UiStateKind::MainMenu,
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r20sub/r1c3-c3-team.jpg",
            UiStateKind::MainMenu,
        ),
        (
            "technical-docs/ui-state-evidence/team-finder.png",
            UiStateKind::Panel,
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r0c2-qiyu-collect.jpg",
            UiStateKind::Panel,
        ),
        (
            "technical-docs/ui-state-evidence/full-hall.jpg",
            UiStateKind::Primary,
        ),
        (
            "technical-docs/ui-state-evidence/round78-live.png",
            UiStateKind::Secondary,
        ),
        (
            "technical-docs/ui-state-evidence/loading-screen.jpg",
            UiStateKind::Loading,
        ),
        (
            "tests/fixtures/ui/menu-paimon-1920x1080.jpg",
            UiStateKind::MainMenu,
        ),
    ] {
        assert_eq!(classify(path).kind(), kind, "{path} kind 回归");
    }
}

// ── 证据画廊诊断（只读，#[ignore]） ──────────────────────────────────
//
// 供 technical-docs/ui-state-evidence/evidence-gallery 的画图脚本使用：
// 用**生产分类器**跑代表帧，把 `kind/label/final_rule`、证据探针原样、
// 以及结构判据（底栏白键 / 派蒙宫格 / 整屏色调 / 千星地图色调 /
// F2 参考距离 / 覆盖页关闭按钮）的真实测量值写成 JSONL。
//
// 不复制匹配器：模板分数直接来自 `UiStateEvidence::template_probes()`；
// 结构量直接调用生产函数。`gallery_panel_probe_scores` 只是把 refine_panel
// 里那张固定表再跑一遍（同模板/同区域/同降采样倍数），用于展示**每个子型**
// 各得多少分，不参与最终判定，也不会写回分类结果。

#[cfg(test)]
fn gallery_rect_json(rect: Rect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x, "y": rect.y, "w": rect.width, "h": rect.height,
    })
}

#[cfg(test)]
fn gallery_evidence_rect_json(rect: UiEvidenceRect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x(), "y": rect.y(), "w": rect.width(), "h": rect.height(),
    })
}

/// 千星地图右下角锚点的测量结果（标注图直接用它画 ROI / 模板框 / 分数阈值）。
#[cfg(test)]
fn gallery_qianxing_map_anchor(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
) -> serde_json::Value {
    let measured = qianxing_map_anchor_measure(image, &templates.qianxing_map_label_template)
        .unwrap_or_else(|error| panic!("锚点测量失败: {error}"));
    let rect = qianxing_map_anchor_rect(image).unwrap_or(QIANXING_MAP_ANCHOR_SEARCH);
    serde_json::json!({
        "template": templates.qianxing_map_label_template.display().to_string(),
        "template_width": QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH,
        "template_height": QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT,
        "bright_min": QIANXING_MAP_ANCHOR_BRIGHT_MIN,
        "threshold": QIANXING_MAP_ANCHOR_SCORE_MIN,
        "score": measured.map(|matched| matched.score),
        "rect": {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height},
        "hit_rect": measured.map(|matched| serde_json::json!({
            "x": matched.x, "y": matched.y,
            "width": QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH,
            "height": QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT,
        })),
    })
}

/// 逐模板独立测量（与 `refine_panel` 同一模板、同一区域、同一降采样倍数）。
#[cfg(test)]
fn gallery_panel_probe_scores(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    panels: PanelRegions,
) -> Vec<serde_json::Value> {
    let threshold = templates.chat_templates.marker_threshold;
    let cases: [(&str, &PathBuf, Rect, u32); 6] = [
        (
            "panel:friend_list",
            &templates.friend_panel_template,
            panels.friend_panel,
            1,
        ),
        (
            "panel:more_settings",
            &templates.friend_block_chat_template,
            panels.friend_block_chat,
            4,
        ),
        (
            "panel:add_friend",
            &templates.friend_search_panel_template,
            panels.friend_search_panel,
            1,
        ),
        (
            "panel:hall_card",
            &templates.invite_enter_hall_template,
            panels.invite_enter_hall,
            // 与 refine_panel 默认表一致：去掉按钮底色后 1/4 会掉到阈值下。
            2,
        ),
        (
            "panel:archive_card",
            &templates.invite_goto_hall_template,
            panels.invite_goto_hall,
            // 与 refine_panel 默认表一致。
            4,
        ),
        (
            "panel:avatar_popup",
            &templates.invite_view_star_template,
            panels.invite_view_star,
            // 与 refine_panel 默认表一致。
            2,
        ),
    ];
    let mut values = Vec::new();
    for (label, path, region, scale) in cases {
        let key = match label {
            "panel:friend_list" => "friend_panel",
            "panel:more_settings" => "friend_block_chat",
            "panel:add_friend" => "friend_search_panel",
            "panel:hall_card" => "invite_enter_hall",
            "panel:archive_card" => "invite_goto_hall",
            "panel:avatar_popup" => "invite_view_star",
            _ => unreachable!("fixed panel table"),
        };
        let scale = templates
            .panel_downsample_factors
            .get(key)
            .copied()
            .unwrap_or(scale);
        let score = best_template_candidate_downscaled(image, Some(region), path, scale)
            .ok()
            .flatten()
            .map(|hit| hit.score);
        values.push(serde_json::json!({
            "label": label,
            "template": path.display().to_string(),
            "region": gallery_rect_json(region),
            "downscale": scale,
            "score": score,
            "threshold": threshold,
            "pass": score.is_some_and(|score| score >= threshold),
        }));
    }
    // 确认弹窗双锚点：确认按钮 + 三个文案关键词各量一次。
    let confirm_button_scale = templates
        .panel_downsample_factors
        .get("confirm_button")
        .copied()
        .unwrap_or(1);
    let confirm_button = best_template_candidate_downscaled(
        image,
        Some(panels.confirm_button),
        &templates.confirm_button_template,
        confirm_button_scale,
    )
    .ok()
    .flatten();
    values.push(serde_json::json!({
        "label": "panel:confirm(button)",
        "template": templates.confirm_button_template.display().to_string(),
        "region": gallery_rect_json(panels.confirm_button),
        "downscale": confirm_button_scale,
        "score": confirm_button.as_ref().map(|hit| hit.score),
        "threshold": threshold,
        "pass": confirm_button.as_ref().is_some_and(|hit| hit.quality_ok && hit.score >= threshold),
    }));
    for (label, path) in [
        (
            "panel:confirm_block_player",
            &templates.confirm_message_block_template,
        ),
        (
            "panel:confirm_enter_hall",
            &templates.confirm_message_random_hall_template,
        ),
        (
            "panel:confirm_leave_hall",
            &templates.confirm_message_return_teyvat_template,
        ),
    ] {
        let hit = best_template_candidate_downscaled(
            image,
            Some(panels.confirm_message),
            path,
            CONFIRM_MESSAGE_SCALE,
        )
        .ok()
        .flatten();
        values.push(serde_json::json!({
                "label": label,
                "template": path.display().to_string(),
                "region": gallery_rect_json(panels.confirm_message),
                "downscale": CONFIRM_MESSAGE_SCALE,
                "score": hit.as_ref().map(|hit| hit.score),
                "threshold": CONFIRM_MESSAGE_MIN,
                "pass": hit.as_ref().is_some_and(|hit| hit.quality_ok && hit.score >= CONFIRM_MESSAGE_MIN),
            }));
    }
    // 千星地图右下角锚点：专用 F1 形状匹配（不是灰度 SAD）。
    let mut anchor = gallery_qianxing_map_anchor(image, templates);
    anchor["label"] = serde_json::json!("secondary:qianxing_map(anchor)");
    anchor["source"] = serde_json::json!("diagnostic");
    anchor["search_rect"] = anchor["rect"].clone();
    anchor["downscale"] = serde_json::json!(1);
    anchor["pass"] = serde_json::json!(
        anchor["score"]
            .as_f64()
            .is_some_and(|score| score as f32 >= QIANXING_MAP_ANCHOR_SCORE_MIN)
    );
    values.push(anchor);
    values
}

/// 单帧检测报告导出。环境变量：
/// - `MILIASTRA_REPORT_IMAGE`：输入截图（必填）
/// - `MILIASTRA_REPORT_OUT`：输出 JSON（必填）
/// - `MILIASTRA_REPORT_EXPECT`：可选期望标签，仅原样记录
/// - `MILIASTRA_REPORT_CONFIG`：可选配置；未给出时用内置完整配置（`bundled_config`）
#[test]
#[ignore = "单帧 UI 检测报告导出：需要 MILIASTRA_REPORT_IMAGE / MILIASTRA_REPORT_OUT"]
#[cfg(test)]
fn dump_ui_detect_report() {
    let Ok(image_path) = std::env::var("MILIASTRA_REPORT_IMAGE") else {
        return;
    };
    let out_path = std::env::var("MILIASTRA_REPORT_OUT").expect("MILIASTRA_REPORT_OUT 必须给出");
    let config_path = std::env::var("MILIASTRA_REPORT_CONFIG").ok();
    let expect = std::env::var("MILIASTRA_REPORT_EXPECT").ok();
    let config = match config_path.as_deref() {
        Some(path) => AppConfig::load(Path::new(path)).unwrap(),
        None => crate::config::bundled_config(),
    };
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let panels = panel_regions_from(&config);
    let canvas = Canvas {
        width: config.screen.expected_width,
        height: config.screen.expected_height,
        resize: true,
    };
    let started = Instant::now();
    let source = match image::open(&image_path) {
        Ok(source) => source,
        Err(error) => {
            let record = serde_json::json!({
                "schema": "miliastra-ui-report/1",
                "image": {"path": image_path, "decode_error": error.to_string()},
                "error": format!("图片解码失败: {error}"),
            });
            std::fs::write(&out_path, serde_json::to_string_pretty(&record).unwrap())
                .unwrap_or_else(|error| panic!("写不到 {out_path}: {error}"));
            return;
        }
    };
    let (source_width, source_height) = (source.width(), source.height());
    let normalized = resize_to_canvas(&source, &canvas, None);
    let resized = normalized.is_some();
    let image = normalized.as_ref().unwrap_or(&source);
    let normalize_ms = started.elapsed().as_millis();
    let classify_started = Instant::now();
    // 每帧独立分类器：避免 game_fallback 的跨帧状态污染单帧报告。
    let mut classifier = TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
        .with_panel_regions(panels);
    let classification = match classifier.classify_at(image, Instant::now()) {
        Ok(classification) => classification,
        Err(error) => {
            let record = serde_json::json!({
                "schema": "miliastra-ui-report/1",
                "image": {
                    "path": image_path,
                    "width": source_width,
                    "height": source_height,
                },
                "error": format!("分类器失败: {error}"),
                "elapsed_ms": started.elapsed().as_millis(),
            });
            std::fs::write(&out_path, serde_json::to_string_pretty(&record).unwrap())
                .unwrap_or_else(|error| panic!("写不到 {out_path}: {error}"));
            return;
        }
    };
    let classify_ms = classify_started.elapsed().as_millis();
    let evidence = classification.evidence();
    let surface = crate::ui::routines::test_surface_of(
        classification.kind(),
        classification.label(),
        evidence.final_rule(),
    );
    let probes: Vec<serde_json::Value> = evidence
        .template_probes()
        .iter()
        .map(|probe| {
            serde_json::json!({
                "label": probe
                    .template()
                    .rsplit('/')
                    .next()
                    .unwrap_or(probe.template()),
                "template": probe.template(),
                "search_rect": gallery_evidence_rect_json(probe.search_rect()),
                "score": probe.best_score(),
                "threshold": probe.threshold(),
                "hit_rect": probe.hit_rect().map(gallery_evidence_rect_json),
                "pass": probe
                    .best_score()
                    .is_some_and(|score| score >= probe.threshold()),
                "outcome": probe.outcome(),
                "source": "production",
            })
        })
        .collect();
    let marker = evidence.marker_probe().map(|marker| {
        serde_json::json!({
            "search_rect": gallery_evidence_rect_json(marker.search_rect()),
            "blue": marker.blue_count(),
            "yellow": marker.yellow_count(),
            "pink": marker.pink_count(),
            "threshold": templates.chat_templates.marker_threshold,
            "source": "production",
        })
    });
    let world = evidence.world_probe().map(|world| {
        serde_json::json!({
            "region": gallery_evidence_rect_json(world.region()),
            "count": world.count(),
            "teyvat_max": world.teyvat_max(),
            "qianxing_min": world.qianxing_min(),
            "guess": world.guess(),
        })
    });
    let hall_region: Rect = config.screen.hall_bar_rect.into();
    let exact = hall_bar_signature(image, hall_region);
    let near = hall_bar_signature_with_threshold(image, hall_region, HALL_BAR_WHITE_NEAR);
    let (dark, white, dominant) = screen_tone(image);
    let (center160, center480) = loading_center_dark_ratios(image);
    let grid = paimon_menu_grid_structure(image);
    let panel_distance = qianxing_player_panel_distance(image);
    let close_ratios = gallery_overlay_close_ratios(image);
    let paimon_region = Rect {
        x: 0,
        y: 0,
        width: image.width() * PAIMON_GRID_LEFT_PERCENT / 100,
        height: image.height(),
    };
    let world_region = world.as_ref().map_or_else(
        || gallery_rect_json(WORLD_CYAN_BAND_RECT),
        |value| value["region"].clone(),
    );
    let structural_regions = serde_json::json!([
        {"name": "底栏白键判据", "key": "screen.hall_bar_rect", "rect": gallery_rect_json(hall_region)},
        {"name": "主菜单返回提瓦特锚点", "key": "screen.main_menu_rect", "rect": gallery_rect_json(config.screen.main_menu_rect.into())},
        {"name": "左上圆形返回按钮", "key": "screen.secondary_back_rect", "rect": gallery_rect_json(config.screen.secondary_back_rect.into())},
        {"name": "二级聊天发送按钮", "key": "screen.secondary_chat_submit_rect", "rect": gallery_rect_json(config.screen.secondary_chat_submit_rect.into())},
        {"name": "F2 邀请玩家按钮", "key": "screen.qianxing_f2_invite_rect", "rect": gallery_rect_json(config.screen.qianxing_f2_invite_rect.into())},
        {"name": "大世界祈愿顶栏", "key": "screen.world_wish_rect", "rect": gallery_rect_json(config.screen.world_wish_rect.into())},
        {"name": "派蒙宫格（左 45% 全高）", "key": "paimon_grid", "rect": gallery_rect_json(paimon_region)},
        {"name": "千星地图右下锚点", "key": "qianxing_map_anchor", "rect": gallery_rect_json(qianxing_map_anchor_rect(image).unwrap_or(QIANXING_MAP_ANCHOR_SEARCH))},
        {"name": "世界判据青色条纹带", "key": "world_probe", "rect": world_region},
        {"name": "好友面板锚点", "key": "moderation.friend_panel_region", "rect": gallery_rect_json(panels.friend_panel)},
        {"name": "添加好友页锚点", "key": "moderation.search_panel_region", "rect": gallery_rect_json(panels.friend_search_panel)},
        {"name": "更多设置锚点", "key": "moderation.block_chat_region", "rect": gallery_rect_json(panels.friend_block_chat)},
        {"name": "确认弹窗按钮锚点", "key": "moderation.confirm_button_region", "rect": gallery_rect_json(panels.confirm_button)},
        {"name": "确认弹窗文案锚点", "key": "moderation.confirm_message_region", "rect": gallery_rect_json(panels.confirm_message)},
        {"name": "头像弹窗锚点", "key": "invite.view_star_region", "rect": gallery_rect_json(panels.invite_view_star)},
        {"name": "档案卡锚点", "key": "invite.goto_hall_region", "rect": gallery_rect_json(panels.invite_goto_hall)},
        {"name": "大厅卡锚点", "key": "invite.enter_hall_region", "rect": gallery_rect_json(panels.invite_enter_hall)},
    ]);
    let ocr_regions = serde_json::json!([
        {"name": "聊天正文 OCR", "source": "screen.chat_rect", "purpose": "chat", "rect": gallery_rect_json(config.screen.chat_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "大厅名称 OCR", "source": "screen.hall_name_rect", "purpose": "hall_name", "rect": gallery_rect_json(config.screen.hall_name_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "大厅人数 OCR", "source": "screen.hall_member_count_rect", "purpose": "hall_member_count", "rect": gallery_rect_json(config.screen.hall_member_count_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "大厅时间 OCR", "source": "screen.hall_time_rect", "purpose": "hall_time", "rect": gallery_rect_json(config.screen.hall_time_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "大厅成员列表 OCR", "source": "screen.hall_member_list_rect", "purpose": "hall_member_list", "rect": gallery_rect_json(config.screen.hall_member_list_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "二级聊天左侧「大厅」入口 OCR", "source": "screen.secondary_hall_rect", "purpose": "secondary_hall", "rect": gallery_rect_json(config.screen.secondary_hall_rect.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "好友行 OCR", "source": "invite.friend_list_region", "purpose": "friend_rows", "rect": gallery_rect_json(config.invite.friend_list_region.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "好友私聊正文 OCR", "source": "invite.friend_chat_region", "purpose": "friend_chat", "rect": gallery_rect_json(config.invite.friend_chat_region.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
        {"name": "GameGate「点击进入」OCR（来自配置区域）", "source": "startup.enter_game_text_region", "purpose": "game_gate", "rect": gallery_rect_json(config.startup.enter_game_text_region.into()), "text": null, "status": "UNVERIFIABLE (offline)"},
    ]);
    let record = serde_json::json!({
        "schema": "miliastra-ui-report/1",
        "image": {
            "path": image_path,
            "width": source_width,
            "height": source_height,
            "canvas_width": image.width(),
            "canvas_height": image.height(),
            "resized": resized,
            "scale_x": image.width() as f64 / source_width as f64,
            "scale_y": image.height() as f64 / source_height as f64,
        },
        "config": config_path.as_deref().unwrap_or("<bundled>"),
        "expect": expect,
        "kind": classification.kind().name(),
        "label": classification.label(),
        "final_rule": evidence.final_rule(),
        "surface": {
            "debug": format!("{surface:?}"),
            "chinese": surface.chinese_name(),
        },
        "elapsed_ms": {
            "normalize": normalize_ms,
            "classify": classify_ms,
            "total": started.elapsed().as_millis(),
        },
        "template_probes": probes,
        "diagnostic_probes": report_diagnostic_probes(
            image,
            &templates,
            panels,
            &config.screen,
            classification.kind(),
        ),
        "marker": marker,
        "world": world,
        "structural": {
            "hall_bar_exact": {
                "tiles": exact.0,
                "min_height_px": exact.1,
                "max_height_px": exact.2,
                "top_spread_px": exact.3,
                "looks_like_hall_keys": hall_bar_looks_like_hall_keys(exact.0, exact.1, exact.2, exact.3),
            },
            "hall_bar_near_white": {
                "tiles": near.0,
                "min_height_px": near.1,
                "max_height_px": near.2,
                "top_spread_px": near.3,
                "looks_like_hall_keys": hall_bar_looks_like_hall_keys(near.0, near.1, near.2, near.3),
            },
            "paimon_grid": grid.map(|(tiles, rows, cols)| serde_json::json!({
                "tiles": tiles, "rows": rows, "cols": cols,
                "min_tiles": PAIMON_GRID_MIN_TILES,
                "min_cols": PAIMON_GRID_MIN_COLS,
                "min_rows": PAIMON_GRID_MIN_ROWS,
            })),
            "screen_tone": {"dark": dark, "white": white, "dominant": dominant},
            "loading_center": {"center160_dark": center160, "center480_dark": center480},
            "loading_rule": loading_rule(image),
            "qianxing_map_anchor": gallery_qianxing_map_anchor(image, &templates),
            "qianxing_panel_distance": panel_distance,
            "qianxing_panel_max": QIANXING_PANEL_MAX,
            "overlay_close": close_ratios.map(|(blue, white)| serde_json::json!({
                "blue_ratio": blue,
                "white_ratio": white,
                "blue_gate": 0.20,
                "white_threshold": 0.35,
                "hit": is_qianxing_overlay_close(image),
            })),
            "world_is_teyvat": world_is_teyvat_in(image, hall_region),
            "beige_panel_fraction": beige_panel_fraction(image),
            "anchor_threshold": templates.chat_templates.marker_threshold.max(0.95),
            "marker_threshold": templates.chat_templates.marker_threshold,
        },
        "structural_regions": structural_regions,
        "ocr_regions": ocr_regions,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&record).unwrap())
        .unwrap_or_else(|error| panic!("写不到 {out_path}: {error}"));
    eprintln!(
        "UI 检测报告：{} -> {}（kind={} label={}）",
        record["image"]["path"].as_str().unwrap_or("?"),
        out_path,
        classification.kind().name(),
        classification.label()
    );
}

/// task-126 验证集：每个 surface 正/负样本，先验证再改模板。
#[test]
fn task126_surface_validation_sets() {
    if crate::test_support::corpus::skip_without_corpus("task126_surface_validation_sets") {
        return;
    }
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let classify = |path: &str| {
        let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
        let mut classifier =
            TemplateUiStateClassifier::new(templates.clone(), config.screen.clone())
                .with_panel_regions(panel_regions_from(&config));
        classifier.classify_at(&image, Instant::now()).unwrap()
    };
    let ev = |p: &str| format!("technical-docs/ui-state-evidence/{p}");
    // F2 多人面板（大厅页）→ Secondary·多人面板；绝不能再落成覆盖层·大厅卡。
    for p in [
        "qianxing-ui-59/after-fix-f2.jpg",
        "qianxing-ui-59/qxf2-from-hall.jpg",
        "qianxing-ui-59/r10-now.jpg",
        "qianxing-ui-59/r10-panel-live.jpg",
        "qianxing-ui-59/r10-set.jpg",
        "qianxing-ui-59/r17-大厅.jpg",
        "qianxing-ui-59/r18sub/r1c0-hall.jpg",
        "hall-page.png",
    ] {
        let c = classify(&ev(p));
        assert_eq!(
            c.kind(),
            UiStateKind::Secondary,
            "{p} 应判 Secondary，实际 {}/{}",
            c.kind().name(),
            c.label()
        );
        assert_eq!(
            c.label(),
            "secondary:qianxing_panel",
            "{p} 应判 F2 多人面板，实际 {}/{}",
            c.kind().name(),
            c.label()
        );
    }
    // 真正的覆盖层·大厅卡（叠在二级聊天之上的进入大厅弹窗）仍是 panel:hall_card。
    let c = classify("tests/fixtures/ui/panel-hall-card-1920x1080.jpg");
    assert_eq!(
        c.label(),
        "panel:hall_card",
        "覆盖层大厅卡正样本被误判成 {}",
        c.label()
    );
    // modF-11 / modF-14 是**千星个人资料页**（右下角同样有「心愿单」+「衣橱」），
    // 不是提瓦特好友档案卡；旧语料曾把它标成档案卡，双按钮判据接入后统一改判。
    for p in ["modF-11.png", "modF-14.png"] {
        let c = classify(&ev(p));
        assert_eq!(
            c.label(),
            "panel:profile_page",
            "{p} 应判个人资料页，实际 {}",
            c.label()
        );
    }
    // 真正的提瓦特好友档案卡（角色展柜 / 添加好友版式）绝不能被个人资料页判据抢走。
    // 这里只断言"不落个人资料页"：档案卡自身的 panel:archive_card 判据属于另一条
    // 模板链，其回归由档案卡自己的测试负责，不在这里耦合。
    for p in ["modI-2.png", "recov-3.png"] {
        let c = classify(&ev(p));
        assert_ne!(c.label(), "panel:profile_page", "{p} 被个人资料页判据误判");
    }
    // Other 正样本（千星子页）
    for p in [
        "qianxing-ui-59/r18sub/r0c1-pop-qiyu.jpg",
        "qianxing-ui-59/r18sub/r0c2-qiyu-collect.jpg",
        "qianxing-ui-59/r18sub/r0c3-my-qiyu.jpg",
        "qianxing-ui-59/r18sub/r3c0-guide.jpg",
        "team-finder.png",
        "wonderland-level-page.png",
        "journey-page.png",
        "qianxing-ui-59/r18sub/r1c2-friend.jpg",
    ] {
        let c = classify(&ev(p));
        assert_eq!(
            c.label(),
            "panel:qianxing_page",
            "{p} 应为 Other，实际 {}",
            c.label()
        );
    }
    // 非常规入口的「特典活动」页：用户确认它不是常规 UI 状态，直接从状态模型
    // 丢弃（不再有专属探针）。这三帧必须落 `Unknown`，也不得再被其它千星
    // 覆盖页判据回收成 `panel:qianxing_page`。
    for p in [
        "qianxing-ui-59/r18sub/r3c1-special-event.jpg",
        "qianxing-ui-59/r18sub/r3c2-community.jpg",
        "qianxing-ui-59/r20sub/r3c1-c1-event.jpg",
    ] {
        let c = classify(&ev(p));
        assert_eq!(
            c.kind(),
            UiStateKind::Unknown,
            "{p} 是非常规入口的特典活动页，应丢弃为 Unknown，实际 {}/{}",
            c.kind().name(),
            c.label()
        );
    }
    // 退出菜单正样本
    for p in [
        "qianxing-ui-59/r12-ctrl-gear.jpg",
        "qianxing-ui-59/r16-y1000-a.jpg",
    ] {
        let c = classify(&ev(p));
        assert_eq!(
            c.label(),
            "panel:qianxing_page",
            "{p} 应判退出菜单(Other)，实际 {}",
            c.label()
        );
    }
    // 负样本：提瓦特面板/大厅/主菜单不得被千星锚点抢判
    for (p, label) in [
        ("friend-list.png", "panel:friend_list"),
        ("friend-list-goto-hall.jpg", "panel:friend_list"),
        ("friend-panel-via-O.png", "panel:friend_list"),
        ("modC-4.png", "panel:add_friend"),
        // 逐帧目检：这张是⋯菜单展开；菜单优先级高于底层资料卡。
        (
            "tests/fixtures/ui/panel-more-settings-1920x1080.jpg",
            "panel:more_settings",
        ),
        (
            "tests/fixtures/ui/panel-friend-list-1920x1080.jpg",
            "panel:friend_list",
        ),
        (
            "tests/fixtures/ui/panel-avatar-popup-1920x1080.jpg",
            "panel:avatar_popup",
        ),
    ] {
        let path = if p.starts_with("tests/") {
            p.to_string()
        } else {
            ev(p)
        };
        let c = classify(&path);
        assert_eq!(c.label(), label, "{path} 负样本被误判为 {}", c.label());
    }
    // 提瓦特好友详情卡（more-settings.png / looknow.png，与 friend-detail-actions.png
    // 字节相同）：⋯下拉弹窗没打开，画面只是好友详情卡上的同名按钮列。
    // 更多设置判据的区域收窄后，它们**不得**再被判成更多设置弹窗；落 archive_card
    // 是档案卡「添加好友」兜底锚点的行为，这里只钉住"不是更多设置、不是千星页"。
    for p in ["more-settings.png", "looknow.png"] {
        let c = classify(&ev(p));
        assert_ne!(
            c.label(),
            "panel:more_settings",
            "{p} 不是⋯下拉弹窗（区域外文字块），不得判更多设置"
        );
        assert_ne!(
            c.label(),
            "panel:qianxing_page",
            "{p} 是提瓦特帧，不得判千星页"
        );
    }
    // 大厅/二级不得被千星锚点抢判（kind 级）
    assert_eq!(classify(&ev("full-hall.jpg")).kind(), UiStateKind::Primary);
    assert_eq!(
        classify(&ev("round78-live.png")).kind(),
        UiStateKind::Secondary
    );
}

// ── 个人资料页双模板：分数分布诊断（#[ignore]） ──────────────────────
//
// 用生产匹配路径（同模板/同搜索区/同 1/2 下采样）量每一帧的两个按钮分数，
// 供选择阈值与记录负样本上界。`MILIASTRA_PROFILE_PAGE_SCAN=1` 时扫描整棵
// technical-docs/ui-state-evidence（约 1000 帧），否则只量固定代表帧。

#[cfg(test)]
fn profile_page_button_scores(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
) -> (Option<f32>, Option<f32>) {
    let Some((wishlist_region, wardrobe_region)) = profile_page_button_rects(image) else {
        return (None, None);
    };
    let score = |path: &PathBuf, region: Rect| {
        best_template_candidate_downscaled(image, Some(region), path, PROFILE_PAGE_BUTTON_SCALE)
            .ok()
            .flatten()
            .map(|hit| hit.score)
    };
    (
        score(
            &templates.qianxing_profile_page_wishlist_template,
            wishlist_region,
        ),
        score(
            &templates.qianxing_profile_page_wardrobe_template,
            wardrobe_region,
        ),
    )
}

#[cfg(test)]
fn profile_page_scan_paths(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            profile_page_scan_paths(&path, out);
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg"
                )
            })
        {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "诊断：个人资料页双模板在证据帧上的分数分布；MILIASTRA_PROFILE_PAGE_SCAN=1 扫描全量"]
fn profile_page_button_score_diagnostic() {
    let config = crate::config::bundled_config();
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let mut paths: Vec<PathBuf> = Vec::new();
    if std::env::var_os("MILIASTRA_PROFILE_PAGE_SCAN").is_some() {
        profile_page_scan_paths(Path::new("technical-docs/ui-state-evidence"), &mut paths);
        profile_page_scan_paths(Path::new("tests/fixtures/ui"), &mut paths);
    } else {
        for p in [
            "technical-docs/ui-state-evidence/qianxing-profile-page.png",
            "tests/fixtures/ui/panel-archive-card-1920x1080.jpg",
            "tests/fixtures/ui/panel-hall-card-1920x1080.jpg",
            "tests/fixtures/ui/panel-more-settings-1920x1080.jpg",
            "tests/fixtures/ui/panel-friend-list-1920x1080.jpg",
            "tests/fixtures/ui/panel-add-friend-1920x1080.jpg",
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxf2-from-hall.jpg",
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxmap-a.jpg",
            "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r0c3-my-qiyu.jpg",
            "tests/fixtures/ui/main-menu-wonderland-1920x1080.png",
            "technical-docs/ui-state-evidence/qianxing-ui-59/r12-ctrl-gear.jpg",
            "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r3c1-special-event.jpg",
            "technical-docs/ui-state-evidence/full-hall.jpg",
            "technical-docs/ui-state-evidence/round78-live.png",
        ] {
            paths.push(PathBuf::from(p));
        }
    }
    let mut rows: Vec<(f32, String, Option<f32>, Option<f32>)> = Vec::new();
    for path in paths {
        let Ok(image) = image::open(&path) else {
            continue;
        };
        if image.width() != 1920 || image.height() != 1080 {
            continue;
        }
        let (wishlist, wardrobe) = profile_page_button_scores(&image, &templates);
        let max = wishlist.unwrap_or(0.0).max(wardrobe.unwrap_or(0.0));
        rows.push((max, path.display().to_string(), wishlist, wardrobe));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!("=== 个人资料页双模板分数（降序，含正样本） ===");
    for (max, path, wishlist, wardrobe) in rows.iter().take(40) {
        eprintln!(
            "max={max:.4} wishlist={:.4} wardrobe={:.4} {path}",
            wishlist.unwrap_or(0.0),
            wardrobe.unwrap_or(0.0),
        );
    }
}

// ── 个人资料页（右下角「心愿单」+「衣橱」双按钮） ────────────────────

fn profile_page_classifier(config: &AppConfig) -> TemplateUiStateClassifier {
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    TemplateUiStateClassifier::new(templates, config.screen.clone())
        .with_panel_regions(panel_regions_from(config))
}

const PROFILE_PAGE_FRAME: &str = "technical-docs/ui-state-evidence/qianxing-profile-page.png";

#[test]
fn profile_page_frame_is_panel_profile_page() {
    if crate::test_support::corpus::skip_without_corpus("profile_page_frame_is_panel_profile_page")
    {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    let image = image::open(PROFILE_PAGE_FRAME)
        .unwrap_or_else(|error| panic!("打不开 {PROFILE_PAGE_FRAME}: {error}"));
    let classification = classifier.classify_at(&image, Instant::now()).unwrap();
    assert_eq!(classification.kind(), UiStateKind::Panel);
    assert_eq!(classification.label(), "panel:profile_page");
    assert_eq!(classification.evidence().final_rule(), "panel:profile_page");
    // 两个按钮模板都必须作为证据出现并达到阈值（分数量自生产探针，不写死）。
    let probes = classification.evidence().template_probes();
    assert_eq!(probes.len(), 2, "双按钮判据必须记录两个模板探针");
    for probe in probes {
        assert_eq!(probe.outcome(), "matched", "{} 未命中", probe.template());
        assert!(
            probe
                .best_score()
                .is_some_and(|score| score >= probe.threshold()),
            "{} 分数 {:?} 低于阈值 {}",
            probe.template(),
            probe.best_score(),
            probe.threshold(),
        );
        // 透明化模板的原点就是「按钮原点 + 字形裁剪原点」；真实帧上必须精确
        // 落在各自按钮位置，否则位置门控就形同虚设。
        let expected = if probe
            .template()
            .ends_with("qianxing-profile-page-wishlist.png")
        {
            PROFILE_PAGE_WISHLIST_ORIGIN
        } else if probe
            .template()
            .ends_with("qianxing-profile-page-wardrobe.png")
        {
            PROFILE_PAGE_WARDROBE_ORIGIN
        } else {
            panic!("意外的按钮模板：{}", probe.template());
        };
        let hit = probe
            .hit_rect()
            .unwrap_or_else(|| panic!("{} 没有命中矩形", probe.template()));
        assert_eq!(
            (hit.x(), hit.y()),
            expected,
            "{} 命中原点与位置门控期望不一致",
            probe.template()
        );
    }
    let surface = crate::ui::routines::test_surface_of(
        classification.kind(),
        classification.label(),
        classification.evidence().final_rule(),
    );
    assert_eq!(surface.chinese_name(), "覆盖层·个人资料页");
    assert_eq!(format!("{surface:?}"), "Overlay(ProfilePage)");
}

/// 负样本：现有面板/二级/主菜单不得被个人资料页双按钮判据抢走。
#[test]
fn profile_page_does_not_steal_existing_panels() {
    if crate::test_support::corpus::skip_without_corpus(
        "profile_page_does_not_steal_existing_panels",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    let cases = [
        // 档案卡 / 大厅卡 / 更多设置 / 好友面板 / 搜索页 / 头像弹窗（提瓦特面板）
        (
            "tests/fixtures/ui/panel-archive-card-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:qianxing_profile_friend",
        ),
        // 同位置、同尺寸的「另存为 / 保存」对话框（最难的负样本，实测 0.9552）
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r17-好友.jpg",
            UiStateKind::Panel,
            "panel:qianxing_page",
        ),
        (
            "tests/fixtures/ui/panel-hall-card-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:hall_card",
        ),
        (
            "tests/fixtures/ui/panel-more-settings-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:more_settings",
        ),
        (
            "tests/fixtures/ui/panel-friend-list-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:friend_list",
        ),
        (
            "tests/fixtures/ui/panel-add-friend-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:add_friend",
        ),
        (
            "tests/fixtures/ui/panel-avatar-popup-1920x1080.jpg",
            UiStateKind::Panel,
            "panel:avatar_popup",
        ),
        // 主菜单千星（返回提瓦特）
        (
            "tests/fixtures/ui/main-menu-wonderland-1920x1080.png",
            UiStateKind::MainMenu,
            "",
        ),
        // 千星 F2 多人面板 / 千星地图（底栏区域同样有白色胶囊按钮）
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxf2-from-hall.jpg",
            UiStateKind::Secondary,
            "secondary:qianxing_panel",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxmap-a.jpg",
            UiStateKind::Secondary,
            "secondary:qianxing_map",
        ),
        // 千星 Other 覆盖页（个人资料页的最近邻：同版式、同关闭按钮）
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r0c3-my-qiyu.jpg",
            UiStateKind::Panel,
            "panel:qianxing_page",
        ),
        // 非常规入口的特典活动页：已从 UI 状态模型丢弃，必须是 Unknown，
        // 也不能被个人资料页判据误判。
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r18sub/r3c1-special-event.jpg",
            UiStateKind::Unknown,
            "",
        ),
        // 大厅 + 二级聊天
        (
            "technical-docs/ui-state-evidence/full-hall.jpg",
            UiStateKind::Primary,
            "",
        ),
        (
            "technical-docs/ui-state-evidence/round78-live.png",
            UiStateKind::Secondary,
            "",
        ),
    ];
    for (path, kind, label) in cases {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开 {path}: {error}"));
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            classification.label(),
            "panel:profile_page",
            "{path} 被个人资料页判据误判"
        );
        assert_eq!(classification.kind(), kind, "{path} kind 回归");
        if !label.is_empty() {
            assert_eq!(classification.label(), label, "{path} label 回归");
        }
    }
    // 提瓦特好友档案卡：只保证不被个人资料页判据抢走；档案卡自身的 label 归
    // `invite_goto_hall` 模板链，属于另一条判据，不在本测试里锁死。
    for path in [
        "technical-docs/ui-state-evidence/modI-2.png",
        "technical-docs/ui-state-evidence/recov-3.png",
    ] {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开 {path}: {error}"));
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_ne!(
            classification.label(),
            "panel:profile_page",
            "{path} 被个人资料页判据误判"
        );
    }
}

/// 两个按钮只命中一个时不得判个人资料页：抹掉任一个按钮后，
/// 另一个模板仍达到阈值（证明是「双命中」要求把它拦下来的，不是分数掉下去）。
#[test]
fn profile_page_requires_both_buttons() {
    if crate::test_support::corpus::skip_without_corpus("profile_page_requires_both_buttons") {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    let base = image::open(PROFILE_PAGE_FRAME)
        .unwrap_or_else(|error| panic!("打不开 {PROFILE_PAGE_FRAME}: {error}"))
        .to_rgba8();
    // 按钮实测像素边界（与模板同源）：心愿单 x1464–1655、衣橱 x1680–1871，y996–1043。
    let erase = |mut image: image::RgbaImage, x: u32| {
        // 用按钮左侧背景色填掉整块按钮，保证不是“按钮还在但被压暗”。
        let background = image.get_pixel(x - 20, 1020).0;
        for y in 996..1044 {
            for px in x..x + 192 {
                image.put_pixel(px, y, image::Rgba(background));
            }
        }
        DynamicImage::ImageRgba8(image)
    };
    for (missing, remaining_is_wardrobe) in [(1464u32, true), (1680u32, false)] {
        let single = erase(base.clone(), missing);
        let (wishlist, wardrobe) = profile_page_button_scores(&single, &templates);
        let remaining = if remaining_is_wardrobe {
            wardrobe
        } else {
            wishlist
        };
        assert!(
            remaining.is_some_and(|score| score >= PROFILE_PAGE_BUTTON_SCORE_MIN),
            "抹掉 x={missing} 后另一按钮仍应命中，实际 wishlist={wishlist:?} wardrobe={wardrobe:?}"
        );
        assert!(
            classifier.profile_page_by_buttons(&single).is_none(),
            "只剩一个按钮时不得判个人资料页（抹掉 x={missing}）"
        );
        let classification = classifier.classify_at(&single, Instant::now()).unwrap();
        assert_ne!(classification.label(), "panel:profile_page");
    }
}

// ── 资料卡四分类（提瓦特 / 千星 × 好友 / 非好友） ────────────────────

/// 四张用户实拍帧 + 三张旧档案卡帧必须逐帧判出具体四分类。
#[test]
fn profile_card_four_way_classification() {
    if crate::test_support::corpus::skip_without_corpus("profile_card_four_way_classification") {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    for (path, label) in [
        (
            "technical-docs/ui-state-evidence/teyvat-profile-card-nonfriend.png",
            "panel:teyvat_profile_nonfriend",
        ),
        (
            "technical-docs/ui-state-evidence/teyvat-profile-card-friend.png",
            "panel:teyvat_profile_friend",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-profile-card-nonfriend.png",
            "panel:qianxing_profile_nonfriend",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-profile-card-friend.png",
            "panel:qianxing_profile_friend",
        ),
        // 旧「档案卡」3 帧逐帧目检后归入具体类型。
        (
            "technical-docs/ui-state-evidence/modI-2.png",
            "panel:teyvat_profile_nonfriend",
        ),
        (
            "technical-docs/ui-state-evidence/recov-3.png",
            "panel:teyvat_profile_nonfriend",
        ),
        (
            "tests/fixtures/ui/panel-archive-card-1920x1080.jpg",
            "panel:qianxing_profile_friend",
        ),
        // ⋯菜单叠在提瓦特非好友资料卡上，仍应判非好友。
        (
            "technical-docs/ui-state-evidence/friend-detail-actions.png",
            "panel:teyvat_profile_nonfriend",
        ),
        (
            "technical-docs/ui-state-evidence/looknow.png",
            "panel:teyvat_profile_nonfriend",
        ),
    ] {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开 {path}: {error}"));
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(classification.kind(), UiStateKind::Panel, "{path}");
        assert_eq!(
            classification.label(),
            label,
            "{path} 的资料卡四分类错了（final_rule={}）",
            classification.evidence().final_rule()
        );
    }
}

/// 把 `source` 的 `source_rect` 贴到 `image` 的 `dest`；只用于合成负样本
/// （抹掉「添加好友」），不改变真实语料。
fn paste_rect(
    image: &DynamicImage,
    dest: Rect,
    source: &DynamicImage,
    source_rect: Rect,
) -> DynamicImage {
    let mut canvas = image.to_rgba8();
    let patch = source.to_rgba8();
    for y in 0..dest.height as i32 {
        for x in 0..dest.width as i32 {
            let sx = (source_rect.x + x).max(0) as u32;
            let sy = (source_rect.y + y).max(0) as u32;
            canvas.put_pixel(
                (dest.x + x).max(0) as u32,
                (dest.y + y).max(0) as u32,
                *patch.get_pixel(sx, sy),
            );
        }
    }
    DynamicImage::ImageRgba8(canvas)
}

/// 好友/非好友只差「添加好友」：抹掉按钮后必须从非好友翻成好友。
#[test]
fn profile_card_friend_flag_flips_when_add_friend_is_erased() {
    if crate::test_support::corpus::skip_without_corpus(
        "profile_card_friend_flag_flips_when_add_friend_is_erased",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    // 负样本背景来自同版式的好友/空白帧，只覆盖按钮本身，不手绘内容。
    let teyvat_blank =
        image::open("technical-docs/ui-state-evidence/teyvat-profile-card-friend.png").unwrap();
    let qianxing_blank =
        image::open("technical-docs/ui-state-evidence/qianxing-profile-card-nonfriend.png")
            .unwrap();
    for (path, expected_nonfriend, dest, source, source_rect) in [
        (
            "technical-docs/ui-state-evidence/teyvat-profile-card-nonfriend.png",
            "panel:teyvat_profile_nonfriend",
            Rect::new(1130, 795, 270, 65),
            &teyvat_blank,
            Rect::new(1130, 795, 270, 65),
        ),
        (
            "technical-docs/ui-state-evidence/modI-2.png",
            "panel:teyvat_profile_nonfriend",
            Rect::new(1130, 795, 270, 65),
            &teyvat_blank,
            Rect::new(1130, 795, 270, 65),
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-profile-card-nonfriend.png",
            "panel:qianxing_profile_nonfriend",
            Rect::new(720, 555, 250, 55),
            &qianxing_blank,
            Rect::new(720, 690, 250, 55),
        ),
    ] {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开 {path}: {error}"));
        let original = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            original.label(),
            expected_nonfriend,
            "{path} 原图应判非好友"
        );
        let erased = paste_rect(&image, dest, source, source_rect);
        let classification = classifier.classify_at(&erased, Instant::now()).unwrap();
        let expected_friend = expected_nonfriend.replace("nonfriend", "friend");
        assert_eq!(
            classification.label(),
            expected_friend,
            "{path} 抹掉「添加好友」后应翻成好友"
        );
    }
}

/// 千星好友帧：没有「添加好友」，但「查看资料」必须命中。
#[test]
fn qianxing_profile_friend_uses_view_profile_without_add_friend() {
    if crate::test_support::corpus::skip_without_corpus(
        "qianxing_profile_friend_uses_view_profile_without_add_friend",
    ) {
        return;
    }
    let config = crate::config::bundled_config();
    let mut classifier = profile_page_classifier(&config);
    for path in [
        "technical-docs/ui-state-evidence/qianxing-profile-card-friend.png",
        "tests/fixtures/ui/panel-archive-card-1920x1080.jpg",
    ] {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开 {path}: {error}"));
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.label(),
            "panel:qianxing_profile_friend",
            "{path}"
        );
        let mut add_seen = false;
        let mut view_matched = false;
        let mut domain_matched = false;
        for probe in classification.evidence().template_probes() {
            let name = probe.template();
            if name.contains("qianxing-profile-add-friend") {
                add_seen = true;
                assert_ne!(
                    probe.outcome(),
                    "matched",
                    "{path} 好友帧不应命中「添加好友」"
                );
            }
            if name.contains("invite-view-profile") {
                view_matched = probe.outcome() == "matched";
            }
            if name.contains("qianxing-profile-domain-level") {
                domain_matched = probe.outcome() == "matched";
            }
        }
        assert!(add_seen, "{path} 证据里缺少「添加好友」负样本探针");
        assert!(view_matched, "{path} 好友帧必须由「查看资料」命中");
        assert!(domain_matched, "{path} 千星资料卡必须由「奇域等级」命中");
    }
}

/// 二级聊天停在**不可发言页签**（如「已屏蔽公开频道」）时的判据：
/// 底部没有发送圆钮，靠左上返回按钮 + 左侧「当前大厅」入口按钮确认为二级聊天。
///
/// 理由是否定式的：没有它时，这类帧会一路落到千星地图判据，在千星大厅里被
/// 「整屏偏暗青」抢成 `secondary:qianxing_map`；驻留引擎据此以为自己在千星地图页，
/// 反复按 Esc 却退不出去（实测死循环）。
///
/// 用合成画布而不是真实截图：判据只取决于两个模板是否出现在各自搜索区里，
/// 与语料无关，因此干净检出也能跑（不 `skip_without_corpus`）。
#[test]
fn secondary_chat_without_submit_button_matches_via_hall_anchor() {
    let config = crate::config::bundled_config();
    let back = image::open(&config.templates.secondary_back).expect("load secondary_back");
    let hall = image::open(&config.templates.secondary_hall).expect("load secondary_hall");

    let width = config.screen.expected_width;
    let height = config.screen.expected_height;
    let back_rect: Rect = config.screen.secondary_back_rect.into();
    let hall_rect: Rect = config.screen.secondary_hall_rect.into();

    let paint = |with_hall: bool| {
        // 底色必须**按大块**变化：`screen_tone` 在 120x68 粗网格上取块均值，
        // 逐像素噪声会被平均回同一颜色，反而更像「整屏一色」的加载界面；
        // 而纯色/平滑渐变也都会被 `loading_rule` 抢先判成 loading。
        let mut canvas = image::RgbaImage::new(width, height);
        let mut seed: u32 = 0x9E37_79B9;
        for block_y in (0..height).step_by(24) {
            for block_x in (0..width).step_by(24) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let color = image::Rgba([
                    ((seed >> 8) & 0xFF) as u8,
                    ((seed >> 16) & 0xFF) as u8,
                    ((seed >> 24) & 0xFF) as u8,
                    255,
                ]);
                for y in block_y..(block_y + 24).min(height) {
                    for x in block_x..(block_x + 24).min(width) {
                        canvas.put_pixel(x, y, color);
                    }
                }
            }
        }
        image::imageops::overlay(
            &mut canvas,
            &back.to_rgba8(),
            i64::from(back_rect.x),
            i64::from(back_rect.y),
        );
        if with_hall {
            image::imageops::overlay(
                &mut canvas,
                &hall.to_rgba8(),
                i64::from(hall_rect.x),
                i64::from(hall_rect.y),
            );
        }
        image::DynamicImage::ImageRgba8(canvas)
    };

    let classify = |image: &image::DynamicImage| {
        let mut classifier = TemplateUiStateClassifier::new(
            UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
            config.screen.clone(),
        );
        classifier
            .classify_at(image, Instant::now())
            .expect("classify")
    };

    // 正样本：返回按钮 +「当前大厅」入口，没有发送圆钮 —— 必须判二级聊天。
    let matched = classify(&paint(true));
    assert_eq!(
        matched.kind(),
        UiStateKind::Secondary,
        "返回按钮 +「当前大厅」入口必须判二级聊天，实际 {}",
        matched.kind().name()
    );
    assert_eq!(
        matched.evidence().final_rule(),
        "secondary_hall_anchor",
        "判据名必须是 secondary_hall_anchor，实际 {}",
        matched.evidence().final_rule()
    );

    // 负样本：抹掉「当前大厅」入口后，不得再靠这条判据成立。
    // 否则说明判据退化成「只要有返回按钮就算二级聊天」，会把奇域主菜单吞进来。
    let unmatched = classify(&paint(false));
    assert_ne!(
        unmatched.evidence().final_rule(),
        "secondary_hall_anchor",
        "没有「当前大厅」入口时不得判成 secondary_hall_anchor"
    );
}
