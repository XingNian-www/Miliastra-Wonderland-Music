//! 好友列表定位、搜索、滚动匹配与会话切换。
//!
//! 把「要投递给谁」变成可点击的好友行，并在点击后确认会话确实切到目标好友。
//! 不含发送、不含表情、不含驻留收尾——那些分别属于 `delivery` / `emoji_panel` /
//! 事务包装器（模块根 `friend_delivery.rs`）。

use image::{DynamicImage, GenericImageView};

use crate::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::geometry::{Point, Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::recovery::GameRecoveryCoordinator;
use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiRoutineProgressStage,
};
use crate::text::normalize_comparison_text as normalize_lock_text;

use super::super::residency::{ConversationRef, RecoveryReport, ResidencyTarget, settle_to};
use super::{
    FRIEND_AVATAR_TEXT_OFFSET_X, FRIEND_DRAG_MAX_EDGE_INSET, FRIEND_ROW_Y_TOLERANCE,
    FriendDeliveryRoutineConfig, MAX_FRIEND_LIST_DRAGS, after_input_failure, before_input_failure,
    capture_normalized, settle_environment, settle_environment_without_ocr, wait_ms,
};

pub(crate) enum PageSearch {
    Found(Point),
    Missing,
}
#[derive(Clone, Debug)]
pub(crate) struct FriendOcrRow {
    pub(crate) text: String,
    pub(crate) point: Point,
}
pub(crate) const FUZZY_FRIEND_MIN_SCORE: f32 = 0.55;
pub(crate) const FUZZY_FRIEND_MIN_MARGIN: f32 = 0.12;
/// 打开（并确认）目标好友的会话（既有入口，调用方零修改）。
///
/// 内部使用一份**本对象自建**的恢复协调器：没有注入点时，语义与拆分前逐条等价
/// （恢复观测只活在这一次调用里）。需要与收尾共享同一份状态的路径
/// 请走 [`open_friend_conversation_with_recovery`]。
pub(crate) fn open_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipients: &[String],
) -> std::result::Result<(), UiRoutineFailure> {
    open_friend_conversation_with_recovery(
        context,
        ocr,
        config,
        &GameRecoveryCoordinator::default(),
        recipients,
    )
}

/// 打开（并确认）目标好友的会话，并沿用调用方那一份恢复协调器。
///
/// `recovery` 是本次执行所在的共享协调器：会话级的前置驻留
/// （[`ensure_secondary_chat`]）必须与收尾用同一份，否则同一次投递里
/// 入场与收尾的恢复观测会分裂成两套账。
pub(super) fn open_friend_conversation_with_recovery(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
    recipients: &[String],
) -> std::result::Result<(), UiRoutineFailure> {
    ensure_secondary_chat(context, config, recovery)?;
    let point = locate_stable_friend_row(context, ocr, config, recipients)?;
    context
        .device()
        .click_point(point.x, point.y)
        .map_err(|error| after_input_failure("select_friend", error))?;
    wait_ms(
        context,
        config.friend_step_ms,
        "select_friend",
        InputCertainty::AfterInputUnknown,
    )?;
    confirm_friend_conversation(context, ocr, config, recipients, point)
}
pub(super) fn ensure_secondary_chat(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::NormalizingStart);
    context
        .device()
        .ensure_ready(config.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_friend_delivery", error))?;
    // 只要求"在二级"：调用方随后自行定位好友行/大厅行，避免每次切会话都点大厅。
    settle_to(
        context,
        &settle_environment_without_ocr(config, recovery),
        ResidencyTarget::Secondary(ConversationRef::AnySecondary),
    )?;
    Ok(())
}
pub(crate) fn locate_stable_friend_row(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipients: &[String],
) -> std::result::Result<Point, UiRoutineFailure> {
    let mut pages = Vec::<ChangeFingerprint>::new();
    for drag_index in 0..=MAX_FRIEND_LIST_DRAGS {
        match search_current_friend_page(context, ocr, config, recipients)? {
            PageSearch::Found(point) => return Ok(point),
            PageSearch::Missing => {}
        }
        let image = capture_normalized(
            context,
            config,
            "fingerprint_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
        let fingerprint = rect_chat_change_fingerprint(&image, config.friend_list_region)
            .map_err(|error| after_input_failure("fingerprint_friend_list", error))?;
        if pages
            .iter()
            .any(|page| page_matches(page, &fingerprint, config))
        {
            break;
        }
        pages.push(fingerprint);
        if drag_index == MAX_FRIEND_LIST_DRAGS {
            break;
        }
        let (from, to) = friend_list_drag_points(config.friend_list_region);
        context
            .device()
            .drag_point(from.x, from.y, to.x, to.y)
            .map_err(|error| after_input_failure("drag_friend_list", error))?;
        wait_friend_list_stable(context, config)?;
    }
    if config.fuzzy_friend_match {
        return search_top_friend_candidate(context, ocr, config, recipients);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "locate_friend",
        "no unique stable friend row was found within two list drags",
    ))
}
pub(super) fn search_top_friend_candidate(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipients: &[String],
) -> std::result::Result<Point, UiRoutineFailure> {
    let (from, to) = friend_list_drag_points(config.friend_list_region);
    context
        .device()
        .drag_point(to.x, to.y, from.x, from.y)
        .map_err(|error| after_input_failure("scroll_friend_list_to_top", error))?;
    wait_friend_list_stable(context, config)?;
    let image = capture_normalized(
        context,
        config,
        "fuzzy_friend_list",
        InputCertainty::AfterInputUnknown,
    )?;
    let rows = ocr_friend_rows(ocr, &image, config.friend_list_region)?;
    // 多候选：每个候选名各自打分，取所有候选里的最高分。
    let targets = normalize_friend_targets(recipients);
    let mut candidates = rows
        .into_iter()
        .map(|row| {
            let score = targets
                .iter()
                .map(|target| score_friend_candidate(target, &row.text))
                .fold(0.0_f32, f32::max);
            (score, row)
        })
        .filter(|(score, _)| *score >= FUZZY_FRIEND_MIN_SCORE)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let Some((best_score, best)) = candidates.first() else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_friend_fuzzy",
            "no fuzzy friend candidate reached the score threshold",
        ));
    };
    if candidates
        .get(1)
        .is_some_and(|(score, _)| best_score - score < FUZZY_FRIEND_MIN_MARGIN)
    {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_friend_fuzzy",
            "fuzzy friend candidates were not sufficiently distinct",
        ));
    }
    Ok(best.point)
}
pub(crate) fn ocr_friend_rows(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
) -> std::result::Result<Vec<FriendOcrRow>, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_friend_list", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_friend_list", error.into()))?;
    Ok(lines
        .into_iter()
        .filter_map(|line| {
            let text = line.text.trim().to_string();
            (!text.is_empty()).then_some(FriendOcrRow {
                text,
                point: Point::new(
                    region.x + line.bbox.center().x,
                    region.y + line.bbox.center().y,
                ),
            })
        })
        .collect())
}
pub(super) fn score_friend_candidate(target: &str, candidate: &str) -> f32 {
    let target = normalize_lock_text(target);
    let candidate = normalize_lock_text(candidate);
    if target.is_empty() || candidate.is_empty() {
        return 0.0;
    }
    if candidate == target {
        return 1.0;
    }
    let common = longest_common_subsequence(target.chars().collect(), candidate.chars().collect());
    common as f32 / target.chars().count().max(candidate.chars().count()) as f32
}
pub(super) fn longest_common_subsequence(left: Vec<char>, right: Vec<char>) -> usize {
    let mut table = vec![vec![0; right.len() + 1]; left.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        for (j, right_char) in right.iter().enumerate() {
            table[i + 1][j + 1] = if left_char == right_char {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    table[left.len()][right.len()]
}
pub(crate) fn friend_list_drag_points(region: Rect) -> (Point, Point) {
    let avatar_x = region.x.saturating_sub(FRIEND_AVATAR_TEXT_OFFSET_X).max(0);
    let edge_inset = (region.height as i32 / 12).min(FRIEND_DRAG_MAX_EDGE_INSET);
    (
        Point::new(avatar_x, region.bottom() - edge_inset),
        Point::new(avatar_x, region.y + edge_inset),
    )
}
/// 定位“当前大厅”行时使用的拖拽方向：从列表最高头像往下拖（`to -> from`）。
/// 按住列表内容往下拖会让列表向顶部翻，把固定在列表上方的“当前大厅”行带回可视区；
/// 与逐页向下找好友的 `friend_list_drag_points` 方向相反，不要合并成一个。
pub(crate) fn current_hall_restore_drag_points(region: Rect) -> (Point, Point) {
    let (bottom, top) = friend_list_drag_points(region);
    (top, bottom)
}

/// 归一化一组候选名：去空、去重，顺序保留。
///
/// 归一化必须与 `matching_text_rows` 里对 OCR 行做的处理一致，
/// 否则候选与识别结果永远不相等。
pub(super) fn normalize_friend_targets(targets: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::with_capacity(targets.len());
    for target in targets {
        let value = normalize_lock_text(target);
        if value.is_empty() || normalized.iter().any(|existing| existing == &value) {
            continue;
        }
        normalized.push(value);
    }
    normalized
}
pub(super) fn search_current_friend_page(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    targets: &[String],
) -> std::result::Result<PageSearch, UiRoutineFailure> {
    let targets = normalize_friend_targets(targets);
    if targets.is_empty() {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "validate_friend_name",
            "no usable friend name to search",
        ));
    }
    let mut stable_y = None;
    let mut streak = 0_u32;
    for sample in 0..config.stable_count {
        let image = capture_normalized(
            context,
            config,
            "capture_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
        let hits = matching_text_rows(ocr, &image, config.friend_list_region, &targets)?;
        match hits.as_slice() {
            [point] => {
                if stable_y
                    .is_some_and(|y: i32| y.abs_diff(point.y) <= FRIEND_ROW_Y_TOLERANCE as u32)
                {
                    streak = streak.saturating_add(1);
                } else {
                    stable_y = Some(point.y);
                    streak = 1;
                }
                if streak >= config.stable_count {
                    return Ok(PageSearch::Found(*point));
                }
            }
            [] => {
                stable_y = None;
                streak = 0;
            }
            _ => {
                return Err(UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "locate_friend",
                    "multiple complete friend-name matches were visible in the strict list region",
                ));
            }
        }
        if sample + 1 < config.stable_count {
            wait_ms(
                context,
                config.poll_ms,
                "locate_friend",
                InputCertainty::AfterInputUnknown,
            )?;
        }
    }
    Ok(PageSearch::Missing)
}
pub(super) fn confirm_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    targets: &[String],
    selected_point: Point,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    let targets = normalize_friend_targets(targets);
    let image = capture_normalized(
        context,
        config,
        "confirm_friend_conversation",
        InputCertainty::AfterInputUnknown,
    )?;
    let highlighted = highlighted_row(&image, selected_row_region(config));
    let matches = matching_text_rows(ocr, &image, config.friend_list_region, &targets)?;
    let fuzzy_selected = config.fuzzy_friend_match
        && matches.is_empty()
        && ocr_friend_rows(ocr, &image, config.friend_list_region)?
            .into_iter()
            .any(|candidate| {
                candidate.point.y.abs_diff(selected_point.y) <= FRIEND_ROW_Y_TOLERANCE as u32
            });
    if highlighted.is_some_and(|row| {
        point_in_rect(selected_point, row)
            && (matches.iter().any(|point| point_in_rect(*point, row)) || fuzzy_selected)
    }) {
        return Ok(());
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_friend_conversation",
        "highlighted friend row did not contain the requested OCR name",
    ))
}
/// 会话确认/观察共用的左侧侧栏扫描区：**返回按钮下方整条侧栏**。
///
/// 与 `observation::chat::passive::passive_sidebar_region` 同式：上界取返回按钮底边，
/// 下界沿用好友列表配置底边，横向只保留列表两侧的边距。真实二级聊天帧里列表滚动
/// 会把**选中的会话行**推到 `friend_list_region.y` 之上（实测选中行 y≈188–259 /
/// 226–298）；只从列表配置区起点扫描时与高亮行最多重叠 18px，达不到 24px 的最小
/// 行高，唯一高亮行会被漏判。这里只覆盖左侧侧栏（约 x=10..290, y=80..880），
/// **不扩到聊天正文/输入框**。
pub(crate) fn conversation_sidebar_region(friends: Rect, back: Rect) -> Rect {
    let top = back.bottom();
    Rect::new(
        friends.x.saturating_sub(70),
        top,
        friends.width.saturating_add(110),
        (friends.bottom() - top).max(0) as u32,
    )
}

pub(crate) fn conversation_row_region(config: &FriendDeliveryRoutineConfig) -> Rect {
    selected_row_region(config)
}

pub(super) fn selected_row_region(config: &FriendDeliveryRoutineConfig) -> Rect {
    conversation_sidebar_region(
        config.friend_list_region,
        config.screen.secondary_back_rect.into(),
    )
}
pub(crate) fn point_in_rect(point: Point, rect: Rect) -> bool {
    (rect.x..rect.right()).contains(&point.x) && (rect.y..rect.bottom()).contains(&point.y)
}

pub(super) fn highlighted_row(image: &DynamicImage, region: Rect) -> Option<Rect> {
    highlighted_rows(image, region)
        .into_iter()
        .max_by_key(|row| row.height)
}

/// Read-only counterpart of active friend confirmation: the current frame must
/// contain exactly one selected sidebar row. No recipient or click point is assumed.
///
/// **这个严格版本保留给「决定点什么」的路径**（`residency`/恢复导航）：多高亮时
/// 猜错会点到别人的会话，风险不可接受。
///
/// **只读观察**（二级聊天身份判定）请用 [`highlighted_conversation_row_candidates`]：
/// 它允许 ≥1 条亮带，由调用方按 OCR/大厅模板挑最可能的那条。理由见该函数。
pub(crate) fn highlighted_conversation_row(image: &DynamicImage, region: Rect) -> Option<Rect> {
    let rows = highlighted_rows(image, region);
    match rows.as_slice() {
        [row] => Some(*row),
        _ => None,
    }
}

/// 只读观察用：把所有高亮带按「最可能是选中会话行」排序返回（0 条返回空）。
///
/// 为什么不复用 [`highlighted_conversation_row`] 的「恰好一条」约束：二级聊天面板
/// 里行数多、有红点行、头像亮时经常出现 ≥2 条亮带，旧实现直接返回 `None`，
/// 于是 `ChatIdentity::Unknown` → 命令被身份闸丢弃、未读任务永远等不到
/// `Friend(_)`。观察路径**不点击任何东西**，多给几个候选让调用方按证据挑，
/// 比一无所知安全得多。
///
/// 排序（先验）：高者优先（选中行是整行色带，误报多是细带），再按 y 升序。
pub(crate) fn highlighted_conversation_row_candidates(
    image: &DynamicImage,
    region: Rect,
) -> Vec<Rect> {
    let mut rows = highlighted_rows(image, region);
    rows.sort_by(|left, right| {
        right
            .height
            .cmp(&left.height)
            .then_with(|| left.y.cmp(&right.y))
    });
    rows
}

pub(super) fn highlighted_rows(image: &DynamicImage, region: Rect) -> Vec<Rect> {
    let scan_region = region;
    let start = scan_region.y.max(0);
    let end = scan_region.bottom().min(image.height() as i32);
    let mut runs = Vec::new();
    let mut run_start = None;
    for y in start..end {
        let bright = (scan_region.x.max(0)..scan_region.right().min(image.width() as i32))
            .filter(|x| {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                let luminance =
                    (u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])) / 3;
                luminance >= 50
            })
            .count();
        let width =
            (scan_region.right().min(image.width() as i32) - scan_region.x.max(0)).max(1) as usize;
        if bright * 2 >= width {
            run_start.get_or_insert(y);
        } else if let Some(top) = run_start.take()
            && y - top >= 24
        {
            runs.push((top, y));
        }
    }
    if let Some(top) = run_start
        && end - top >= 24
    {
        runs.push((top, end));
    }
    runs.into_iter()
        .map(|(top, bottom)| {
            Rect::new(
                scan_region.x.max(0),
                top,
                (scan_region.right().min(image.width() as i32) - scan_region.x.max(0)).max(0)
                    as u32,
                (bottom - top) as u32,
            )
        })
        .collect()
}
/// 恢复指定好友会话（按候选名 + 高亮行确认，歧义不点击）。
///
/// 收尾由 [`super::settle_environment`] 构造的会话级恢复环境完成，`recovery` 是本次
/// 执行所在的共享恢复协调器，与其它触发源共用同一份冷却与互斥。
///
/// 当前批次的收尾统一走事务包装器（`RestoreEntry`），所以这个显式入口在生产路径上
/// 还没有调用方；保留它是为了让「回到某个具体会话」仍有一条命名清晰、可复用的路径。
#[allow(dead_code)]
pub(super) fn restore_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
    identity: impl Into<String>,
    candidates: Vec<String>,
) -> std::result::Result<RecoveryReport, UiRoutineFailure> {
    settle_to(
        context,
        &settle_environment(ocr, config, recovery),
        ResidencyTarget::Secondary(ConversationRef::friend(identity, candidates)),
    )
}
/// 在一屏好友列表里找**任意一个**候选名，返回所有命中行。
///
/// 为什么要多候选：好友列表上显示的是「备注串」，没有备注时显示玩家自设昵称；
/// 两者都可能出现，没法预先知道是哪一种。候选由装配层统一用
/// identity.friend_search_names 生成（备注串 → 真实昵称），这里只负责逐个匹配。
///
/// 同一屏命中多个**不同**候选时仍返回全部命中，由调用方判歧义——
/// 悄悄挑一个等于赌运气点错人。
pub(crate) fn matching_text_rows(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
    normalized_targets: &[String],
) -> std::result::Result<Vec<Point>, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_ocr_confirmation", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_confirmation", error.into()))?;
    Ok(lines
        .into_iter()
        .filter_map(|line| {
            let recognized = normalize_lock_text(&line.text);
            let matched = normalized_targets
                .iter()
                .any(|target| recognized == *target);
            matched.then(|| {
                Point::new(
                    region.x + line.bbox.center().x,
                    region.y + line.bbox.center().y,
                )
            })
        })
        .collect())
}
pub(super) fn wait_friend_list_stable(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let mut previous = rect_chat_change_fingerprint(
        &capture_normalized(
            context,
            config,
            "observe_scrolled_friend_list",
            InputCertainty::AfterInputUnknown,
        )?,
        config.friend_list_region,
    )
    .map_err(|error| after_input_failure("observe_scrolled_friend_list", error))?;
    for _ in 0..confirmation_attempts(config) {
        wait_ms(
            context,
            config.poll_ms,
            "confirm_scrolled_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = rect_chat_change_fingerprint(
            &capture_normalized(
                context,
                config,
                "confirm_scrolled_friend_list",
                InputCertainty::AfterInputUnknown,
            )?,
            config.friend_list_region,
        )
        .map_err(|error| after_input_failure("confirm_scrolled_friend_list", error))?;
        if page_matches(&previous, &current, config) {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_scrolled_friend_list",
        "friend list did not become stable after scrolling",
    ))
}

pub(super) fn page_matches(
    left: &ChangeFingerprint,
    right: &ChangeFingerprint,
    config: &FriendDeliveryRoutineConfig,
) -> bool {
    let stats = change_stats(left, right);
    stats.mean_abs_diff <= config.stable_mean_threshold
        && stats.changed_ratio <= config.stable_changed_ratio_threshold
}

pub(super) fn confirmation_attempts(config: &FriendDeliveryRoutineConfig) -> u32 {
    let polls = config.timeout_ms / config.poll_ms.max(1);
    polls.max(config.stable_count as u64).min(u32::MAX as u64) as u32
}
