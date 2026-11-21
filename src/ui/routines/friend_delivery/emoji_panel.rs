//! 二级聊天表情：面板打开、页签定位、卡片网格识别与表情点击输入。
//!
//! 也包含网页端「解析全部页签」的全量表情目录扫描（`EmojiCatalogUi`）。
//! 依赖只走 `crate::text::emoji`（Wave 0-A 下沉成果）：表情的引擎能力属于文本层，
//! 这里不反向引用任何 `features` 下的模块。

use std::time::Duration;

use enigo::Key;
use image::{DynamicImage, GenericImageView};

use crate::geometry::{Point, Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::recovery::GameRecoveryCoordinator;
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::text::emoji::{EMOJI_TAB_TOTAL, EmojiTag, normalize_emoji_label};

use super::super::residency::{ConversationRef, ResidencyTarget, settle_to};
use super::super::transaction::{FinishPolicy, UiTask, UiTaskSpec};
use super::{FriendDeliveryRoutineConfig, after_input_failure, settle_environment, wait_ms};

/// 表情按钮在 1920×1080 设计分辨率下的位置。
pub(crate) const EMOJI_BUTTON_DESIGN: Point = Point::new(907, 980);
/// 表情栏页签行 ROI（设计分辨率）。红心/齿轮模板在这里整块匹配，
/// 高度必须覆盖图标完整高度：红心实测 y≈921..949，原来 840..940 会把它裁掉一半。
pub(crate) const EMOJI_TRAY_DESIGN: Rect = Rect::new(280, 820, 920, 150);
/// 表情网格 ROI（设计分辨率）。只对这块做一次 OCR。
pub(crate) const EMOJI_GRID_DESIGN: Rect = Rect::new(332, 336, 832, 588);
/// 第 1 个角色页签相对红心的 x 偏移（设计分辨率）。
pub(crate) const EMOJI_TAB_FIRST_OFFSET_DESIGN: f32 = 110.0;
/// 相邻角色页签的间距（设计分辨率）。
pub(crate) const EMOJI_TAB_PITCH_DESIGN: f32 = 110.5;
/// 打开表情栏后等待动画的时长。
pub(crate) const EMOJI_PANEL_OPEN_WAIT_MS: u64 = 700;
/// 点击页签后等待网格出现的时长。
pub(crate) const EMOJI_GRID_WAIT_MS: u64 = 700;
/// 点击表情后等待面板关闭的时长。
pub(crate) const EMOJI_CLOSE_WAIT_MS: u64 = 1200;
pub(crate) fn emoji_failure(stage: &'static str, code: &str) -> UiRoutineFailure {
    UiRoutineFailure::new(InputCertainty::AfterInputUnknown, stage, code)
}

pub(crate) fn design_scale(image: &DynamicImage, expected_width: u32) -> f32 {
    if expected_width == 0 {
        return 1.0;
    }
    image.width() as f32 / expected_width as f32
}

pub(crate) fn scale_rect(rect: Rect, scale: f32) -> Rect {
    Rect::new(
        (rect.x as f32 * scale).round() as i32,
        (rect.y as f32 * scale).round() as i32,
        (rect.width as f32 * scale).round().max(1.0) as u32,
        (rect.height as f32 * scale).round().max(1.0) as u32,
    )
}

pub(crate) fn scale_point(point: Point, scale: f32) -> Point {
    Point::new(
        (point.x as f32 * scale).round() as i32,
        (point.y as f32 * scale).round() as i32,
    )
}

pub(crate) fn to_design(value: i32, scale: f32) -> i32 {
    if scale <= 0.0 {
        value
    } else {
        (value as f32 / scale).round() as i32
    }
}
pub(crate) fn capture_emoji_frame(
    context: &mut UiRoutineContext<'_>,
    stage: &'static str,
) -> Result<DynamicImage, UiRoutineFailure> {
    let frame = context
        .capture_frame()
        .map_err(|error| after_input_failure(stage, error))?;
    Ok(frame.image().clone())
}

pub(crate) fn save_emoji_debug(
    image: &DynamicImage,
    config: &FriendDeliveryRoutineConfig,
    label: &str,
) {
    if !config.emoji_capture_debug {
        return;
    }
    let directory = std::path::Path::new("deps/emoji-debug");
    if let Err(error) = std::fs::create_dir_all(directory) {
        log::warn!("创建表情调试截图目录失败：{error:#}");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let path = directory.join(format!("{stamp}-{label}.png"));
    match image.save(&path) {
        Ok(()) => log::info!("表情调试截图已保存：{}", path.display()),
        Err(error) => log::warn!("保存表情调试截图失败：{error:#}"),
    }
}
/// 表情栏固定锚点模板：左侧齿轮与收藏页红心（1920 基线裁图，随画面等比缩放）。
pub(crate) const EMOJI_GEAR_TEMPLATE: &[u8] =
    include_bytes!("../../../../assets/emoji-tray-gear.png");
pub(crate) const EMOJI_HEART_TEMPLATE: &[u8] =
    include_bytes!("../../../../assets/emoji-tray-heart.png");
/// 红心中心相对齿轮中心的偏移（1920 基线实测）。
pub(crate) const EMOJI_GEAR_TO_HEART_DX_DESIGN: i32 = 94;
pub(crate) const EMOJI_GEAR_TO_HEART_DY_DESIGN: i32 = -2;
/// 锚点模板匹配阈值。
pub(crate) const EMOJI_ANCHOR_THRESHOLD: f32 = 0.9;

pub(crate) fn load_scaled_template(bytes: &[u8], scale: f32) -> Option<DynamicImage> {
    let template = image::load_from_memory(bytes).ok()?;
    if (scale - 1.0).abs() <= 0.01 {
        return Some(template);
    }
    let width = (template.width() as f32 * scale).round().max(1.0) as u32;
    let height = (template.height() as f32 * scale).round().max(1.0) as u32;
    Some(template.resize_exact(width, height, image::imageops::FilterType::Triangle))
}

/// 用模板匹配找表情栏锚点：优先红心，找不到再退回齿轮 + 固定偏移。
///
/// 两种模板都是 UI 结构图（不是颜色阈值），匹配失败就是失败，不做坐标兜底。
pub(crate) fn find_emoji_anchor(image: &DynamicImage, tray: Rect, scale: f32) -> Option<Point> {
    if let Some(template) = load_scaled_template(EMOJI_HEART_TEMPLATE, scale)
        && let Ok(Some(hit)) =
            crate::ui::template::best_embedded_template_candidate(image, tray, &template)
        && hit.score >= EMOJI_ANCHOR_THRESHOLD
    {
        return Some(hit.rect().center());
    }
    if let Some(template) = load_scaled_template(EMOJI_GEAR_TEMPLATE, scale)
        && let Ok(Some(hit)) =
            crate::ui::template::best_embedded_template_candidate(image, tray, &template)
        && hit.score >= EMOJI_ANCHOR_THRESHOLD
    {
        let gear = hit.rect().center();
        return Some(Point::new(
            gear.x + (EMOJI_GEAR_TO_HEART_DX_DESIGN as f32 * scale).round() as i32,
            gear.y + (EMOJI_GEAR_TO_HEART_DY_DESIGN as f32 * scale).round() as i32,
        ));
    }
    None
}
/// 以红心为原点推算页签中心；页签总数固定为 `EMOJI_TAB_TOTAL`。
pub(crate) fn emoji_tab_center(heart: Point, pack: u32, scale: f32) -> Option<Point> {
    if pack == 0 || pack > EMOJI_TAB_TOTAL {
        return None;
    }
    if pack == 1 {
        return Some(heart);
    }
    let offset =
        (EMOJI_TAB_FIRST_OFFSET_DESIGN + (pack as f32 - 2.0) * EMOJI_TAB_PITCH_DESIGN) * scale;
    Some(Point::new(heart.x + offset.round() as i32, heart.y))
}

/// 在 OCR 结果里精确匹配候选；多命中视为歧义，拒绝点击。
pub(crate) fn select_emoji_candidate(
    lines: &[crate::runtime::ocr::OcrLine],
    candidates: &[String],
    offset: Point,
    scale: f32,
) -> Result<Option<Point>, &'static str> {
    let mut matches: Vec<(String, Point)> = Vec::new();
    for candidate in candidates {
        // 符号不参与比对：OCR 吃掉 `～ · ！` 时仍应命中同一张卡片。
        let target = normalize_emoji_label(candidate);
        if target.is_empty() {
            continue;
        }
        let hits: Vec<&crate::runtime::ocr::OcrLine> = lines
            .iter()
            .filter(|line| normalize_emoji_label(&line.text) == target)
            .collect();
        if hits.len() > 1 {
            return Err("emoji_ocr_ambiguous");
        }
        if let Some(hit) = hits.first() {
            let center = hit.bbox.center();
            matches.push((
                candidate.clone(),
                Point::new(
                    to_design(offset.x + center.x, scale),
                    to_design(offset.y + center.y, scale),
                ),
            ));
        }
    }
    if matches.is_empty() {
        return Ok(None);
    }
    let index = if matches.len() == 1 {
        0
    } else {
        rand::random::<usize>() % matches.len()
    };
    Ok(Some(matches[index].1))
}
pub(crate) fn close_emoji_panel(context: &mut UiRoutineContext<'_>, stage: &'static str) {
    if let Err(error) = context.device().press_key(Key::Escape) {
        log::warn!("关闭表情面板失败（阶段 {stage}）：{error:#}");
    }
}

/// 打开表情面板并校验红心锚点。
///
/// 失败常见于两点：面板没打开（点到聊天里的红色元素）或面板本来开着、第一次点按钮
/// 又把它关掉。这里最多重试一次，仍不对就明确失败，不做坐标兜底。
///
/// 每次尝试都按当帧的缩放重算锚点（分辨率不是设计基线时，模板与坐标都必须等比缩放）；
/// 未命中时把该帧的缩放写进 debug 日志——尺寸异常导致的连续失败因此能一眼看出来。
pub(crate) fn open_emoji_panel_anchored(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    stage: &'static str,
) -> Result<(Point, f32), UiRoutineFailure> {
    let expected_width = config.screen.expected_width;
    for attempt in 0..2 {
        let frame = capture_emoji_frame(context, stage)?;
        let scale = design_scale(&frame, expected_width);
        let button = scale_point(EMOJI_BUTTON_DESIGN, scale);
        context
            .device()
            .click_point(button.x, button.y)
            .map_err(|error| after_input_failure(stage, error))?;
        wait_ms(
            context,
            EMOJI_PANEL_OPEN_WAIT_MS,
            stage,
            InputCertainty::AfterInputUnknown,
        )?;
        let panel_frame = capture_emoji_frame(context, stage)?;
        save_emoji_debug(&panel_frame, config, "panel-open");
        let tray = scale_rect(EMOJI_TRAY_DESIGN, scale);
        let Some(heart) = find_emoji_anchor(&panel_frame, tray, scale) else {
            log::debug!(
                "表情面板打开后未找到红心锚点（第 {} 次尝试，当帧缩放 {scale:.3}，预期画布宽 {expected_width}）",
                attempt + 1
            );
            continue;
        };
        return Ok((heart, scale));
    }
    Err(emoji_failure(stage, "emoji_anchor_not_found"))
}
pub(crate) fn send_emoji_segment(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    tag: &EmojiTag,
    stage: &'static str,
) -> Result<(), UiRoutineFailure> {
    let (heart, scale) = match open_emoji_panel_anchored(context, config, stage) {
        Ok(value) => value,
        Err(failure) => {
            close_emoji_panel(context, stage);
            return Err(failure);
        }
    };
    let tray = scale_rect(EMOJI_TRAY_DESIGN, scale);

    // 页签 1（收藏）也要点一次：面板会记住上次停留的页签，不能假设默认在收藏。
    let Some(tab) = emoji_tab_center(heart, tag.pack, scale) else {
        close_emoji_panel(context, stage);
        return Err(emoji_failure(stage, "emoji_pack_not_found"));
    };
    if tab.x < tray.x || tab.x > tray.right() || tab.y < tray.y || tab.y > tray.bottom() {
        close_emoji_panel(context, stage);
        return Err(emoji_failure(stage, "emoji_pack_not_found"));
    }
    context
        .device()
        .click_point(tab.x, tab.y)
        .map_err(|error| after_input_failure(stage, error))?;
    wait_ms(
        context,
        EMOJI_GRID_WAIT_MS,
        stage,
        InputCertainty::AfterInputUnknown,
    )?;

    let grid_frame = capture_emoji_frame(context, stage)?;
    save_emoji_debug(&grid_frame, config, "pack-grid");
    let grid = scale_rect(EMOJI_GRID_DESIGN, scale);
    let crop = crop_canvas(&grid_frame, grid)
        .map_err(|error| emoji_failure(stage, &format!("emoji_crop_failed: {error:#}")))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| emoji_failure(stage, &format!("emoji_ocr_failed: {error:#}")))?;
    let pick =
        match select_emoji_candidate(&lines, &tag.candidates, Point::new(grid.x, grid.y), scale) {
            Ok(pick) => pick,
            Err(code) => {
                close_emoji_panel(context, stage);
                return Err(emoji_failure(stage, code));
            }
        };
    let Some(point) = pick else {
        close_emoji_panel(context, stage);
        return Err(emoji_failure(stage, "emoji_not_visible"));
    };
    context
        .device()
        .click_point(point.x, point.y)
        .map_err(|error| after_input_failure(stage, error))?;
    wait_ms(
        context,
        EMOJI_CLOSE_WAIT_MS,
        stage,
        InputCertainty::AfterInputUnknown,
    )?;
    let closed_frame = capture_emoji_frame(context, stage)?;
    save_emoji_debug(&closed_frame, config, "after-send");
    if find_emoji_anchor(&closed_frame, tray, scale).is_some() {
        close_emoji_panel(context, stage);
        return Err(emoji_failure(stage, "emoji_panel_not_closed"));
    }
    Ok(())
}
// ===================================================================
// 网页端一次性解析全部页签（全量表情目录）
// ===================================================================

/// 单个页签的扫描结果。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct EmojiTabScan {
    pub pack: u32,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EmojiCatalogScanOutcome {
    pub tabs: Vec<EmojiTabScan>,
    pub failure: Option<String>,
}

/// 网页端“解析全部页签”入口（只在 HTTP 层使用）。
pub(crate) struct EmojiCatalogUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl EmojiCatalogUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例（装配层只 `default()` 一次）。
    ///
    /// 不注入时行为与注入前逐条等价：默认实例就是「恢复观测只活在本对象里」的旧语义。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit(
        &self,
    ) -> std::result::Result<UiOperation<EmojiCatalogScanOutcome>, UiSubmitError> {
        self.runtime.submit(EmojiCatalogRoutine {
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
        })
    }
}

pub(crate) struct EmojiCatalogRoutine {
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for EmojiCatalogRoutine {}

impl UiRoutine for EmojiCatalogRoutine {
    type Output = EmojiCatalogScanOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let environment = settle_environment(&self.ocr, &self.config, &self.recovery);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::AnySecondary),
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Secondary(ConversationRef::AnySecondary),
            },
            environment,
        )
        .with_total_budget(Duration::from_secs(60));
        let task = UiTask::new(
            spec,
            EmojiCatalogExecution {
                ocr: self.ocr,
                config: self.config,
                recovery: self.recovery,
            },
        );
        let outcome = task.execute(context);
        match outcome.output {
            Some(inner) => EmojiCatalogScanOutcome {
                tabs: inner.tabs,
                failure: inner.failure,
            },
            None => EmojiCatalogScanOutcome {
                tabs: Vec::new(),
                failure: Some(
                    outcome
                        .entry_failure
                        .map(|failure| failure.reason().to_string())
                        .unwrap_or_else(|| "表情目录扫描没有开始：入口导航失败".to_string()),
                ),
            },
        }
    }
}

pub(crate) struct EmojiCatalogExecution {
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

#[derive(Default)]
pub(crate) struct EmojiCatalogInner {
    tabs: Vec<EmojiTabScan>,
    failure: Option<String>,
}

impl sealed::UiRoutineSealed for EmojiCatalogExecution {}

impl UiRoutine for EmojiCatalogExecution {
    type Output = EmojiCatalogInner;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        match scan_emoji_tabs(context, &self.ocr, &self.config, &self.recovery) {
            Ok(tabs) => EmojiCatalogInner {
                tabs,
                failure: None,
            },
            Err(failure) => EmojiCatalogInner {
                tabs: Vec::new(),
                failure: Some(failure.reason().to_string()),
            },
        }
    }
}
/// 网格是否真的打开了：只看网格 ROI 里白色卡片像素占比。
///
/// 收藏页为空时点页签只会停在页签行，上方仍然是聊天画面；这时 OCR 会把聊天正文
/// 当成表情标签，因此必须先判定网格是否存在。
pub(crate) fn emoji_grid_visible(image: &DynamicImage, grid: Rect) -> bool {
    const WHITE_THRESHOLD: u8 = 200;
    const MIN_WHITE_RATIO: f32 = 0.2;
    let (width, height) = image.dimensions();
    let x0 = grid.x.max(0) as u32;
    let y0 = grid.y.max(0) as u32;
    let x1 = grid.right().max(0) as u32;
    let y1 = grid.bottom().max(0) as u32;
    let x1 = x1.min(width);
    let y1 = y1.min(height);
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    let mut white = 0_u64;
    let mut total = 0_u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let [red, green, blue, _] = image.get_pixel(x, y).0;
            let luma =
                (u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000;
            if luma >= u32::from(WHITE_THRESHOLD) {
                white += 1;
            }
            total += 1;
        }
    }
    total > 0 && (white as f32 / total as f32) >= MIN_WHITE_RATIO
}

pub(crate) fn scan_emoji_tabs(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> Result<Vec<EmojiTabScan>, UiRoutineFailure> {
    const STAGE: &str = "scan_emoji_catalog";
    // 扫描必须在二级当前聊天里进行：入口导航可能被过渡界面跳过，
    // 这里再显式落一次驻留，避免在一级大厅里点空。
    settle_to(
        context,
        &settle_environment(ocr, config, recovery),
        ResidencyTarget::Secondary(ConversationRef::AnySecondary),
    )?;
    let (heart, scale) = match open_emoji_panel_anchored(context, config, STAGE) {
        Ok(value) => value,
        Err(failure) => {
            close_emoji_panel(context, STAGE);
            return Err(failure);
        }
    };
    let tray = scale_rect(EMOJI_TRAY_DESIGN, scale);
    let grid = scale_rect(EMOJI_GRID_DESIGN, scale);
    let mut tabs = Vec::with_capacity(EMOJI_TAB_TOTAL as usize);
    for pack in 1..=EMOJI_TAB_TOTAL {
        let Some(tab) = emoji_tab_center(heart, pack, scale) else {
            close_emoji_panel(context, STAGE);
            return Err(emoji_failure(STAGE, "emoji_pack_not_found"));
        };
        if tab.x < tray.x || tab.x > tray.right() || tab.y < tray.y || tab.y > tray.bottom() {
            close_emoji_panel(context, STAGE);
            return Err(emoji_failure(STAGE, "emoji_pack_not_found"));
        }
        context
            .device()
            .click_point(tab.x, tab.y)
            .map_err(|error| after_input_failure(STAGE, error))?;
        wait_ms(
            context,
            EMOJI_GRID_WAIT_MS,
            STAGE,
            InputCertainty::AfterInputUnknown,
        )?;
        let frame = capture_emoji_frame(context, STAGE)?;
        save_emoji_debug(&frame, config, &format!("scan-pack-{pack}"));
        if !emoji_grid_visible(&frame, grid) {
            // 收藏为空时只有页签行，没有网格：不 OCR，直接记空。
            tabs.push(EmojiTabScan {
                pack,
                labels: Vec::new(),
            });
            continue;
        }
        let crop = crop_canvas(&frame, grid)
            .map_err(|error| emoji_failure(STAGE, &format!("emoji_crop_failed: {error:#}")))?;
        let lines = ocr
            .recognize_lines(crop, OcrPriority::UiConfirmation)
            .map_err(|error| emoji_failure(STAGE, &format!("emoji_ocr_failed: {error:#}")))?;
        tabs.push(EmojiTabScan {
            pack,
            labels: ordered_emoji_labels(&lines),
        });
    }
    close_emoji_panel(context, STAGE);
    Ok(tabs)
}

/// 网格里的非表情文字：收藏为空时的「编辑」卡片等。
pub(crate) const EMOJI_GRID_NON_LABELS: &[&str] = &["编辑"];

/// OCR 行按“先上后下、同一行从左到右”排序，并对完全相同的文字去重。
pub(crate) fn ordered_emoji_labels(lines: &[crate::runtime::ocr::OcrLine]) -> Vec<String> {
    let mut ordered: Vec<&crate::runtime::ocr::OcrLine> = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .collect();
    ordered.sort_by_key(|line| (line.bbox.y / 24, line.bbox.x));
    let mut labels: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in ordered {
        let text = line.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if EMOJI_GRID_NON_LABELS.contains(&text.as_str()) {
            continue;
        }
        // 去重按去符号后的键：`哼哼` 与 `哼哼～` 视为同一个表情。
        let key = normalize_emoji_label(&text);
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        labels.push(text);
    }
    labels
}
