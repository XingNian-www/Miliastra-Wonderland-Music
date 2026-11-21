//! 证据组装与判别日志辅助（探针、区域描述、限速与文案标签）。

use crate::geometry::Rect;
use crate::runtime::ui::UiEvidenceRect;
use crate::runtime::ui::UiTemplateProbeEvidence;
use crate::ui::template::TemplateHit;
use std::time::Instant;

/// 确认弹窗文案关键词锚点的最低分与降采样倍数。
///
/// 文案模板只保留关键词（6/5/3 个汉字，169×29 / 142×29 / 94×32），
/// 搜索区是 620×90 的整行；1/2 下采样即可保住字形，代价约为全分辨率的 1/16。
pub(super) static LAST_CONFIRM_PROBE_LOG_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) fn confirm_probe_log_due() -> bool {
    use std::sync::atomic::Ordering;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_CONFIRM_PROBE_LOG_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 2_000 {
        return false;
    }
    LAST_CONFIRM_PROBE_LOG_MS.store(now, Ordering::Relaxed);
    true
}

pub(crate) const CONFIRM_MESSAGE_MIN: f32 = 0.95;
pub(crate) const CONFIRM_MESSAGE_SCALE: u32 = 2;

/// 确认弹窗诊断日志里的搜索区：位置与尺寸，外加这次实际按几倍下采样在找。
///
/// `Rect` 没有 `Display`，直接 `{:?}` 会在日志里留下 `Rect { x: 1, y: 2, ... }`。
/// 倍数必须写进去：同一块区域在 1× 与 1/2 下的坐标单位不同，只看数字会误判位置。
///
/// 返回的是一整段以「搜索区」起头的描述，调用点直接放在「在……里」中间即可，
/// 不要在前面再写「搜索区」。
pub(super) fn confirm_region_label(region: Rect, scale: u32) -> String {
    format!(
        "搜索区 ({}, {}) {}×{}（按 {} 倍下采样匹配）",
        region.x, region.y, region.width, region.height, scale
    )
}

/// 文字模板命中的内容质量结论（`TemplateHit::quality_ok`）。
pub(super) fn text_match_quality_label(quality_ok: bool) -> &'static str {
    if quality_ok {
        "文字内容质量校验通过"
    } else {
        "文字内容质量校验未通过"
    }
}

/// 确认弹窗锚点不成立的原因：质量门槛与分数门槛是两回事，必须分别说清。
pub(super) fn text_match_reject_label(quality_ok: bool) -> &'static str {
    if quality_ok {
        "原因是相似度未达阈值，不是文字内容质量不足"
    } else {
        "原因是文字内容质量不足，分数再高也不会采纳"
    }
}

/// 确认弹窗文案子类型的业务语言。
///
/// 三个子类型由命中的关键词模板决定；标签是内部英文标识，直接写进日志
/// 会让读日志的人看不出是哪句文案。
pub(super) fn confirm_message_keyword(label: &str) -> &str {
    match label {
        "panel:confirm_block_player" => "屏蔽玩家",
        "panel:confirm_enter_hall" => "进入随机大厅",
        "panel:confirm_leave_hall" => "离开大厅",
        _ => label,
    }
}

pub(super) fn hit_matches(hit: &TemplateHit, threshold: f32) -> bool {
    hit.quality_ok && hit.score >= threshold
}

pub(super) fn candidate_matches(candidate: &Option<TemplateHit>, threshold: f32) -> bool {
    candidate
        .as_ref()
        .is_some_and(|candidate| candidate.quality_ok && candidate.score >= threshold)
}

pub(super) fn template_probe(
    template: &std::path::Path,
    search_rect: Rect,
    threshold: f32,
    candidate: Option<&TemplateHit>,
) -> UiTemplateProbeEvidence {
    let hit = candidate.filter(|candidate| candidate.quality_ok && candidate.score >= threshold);
    let outcome = match (candidate, hit) {
        (_, Some(_)) => "matched",
        (Some(candidate), None) if !candidate.quality_ok => "quality_rejected",
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

pub(super) fn evidence_rect(rect: Rect) -> UiEvidenceRect {
    UiEvidenceRect::new(rect.x, rect.y, rect.width, rect.height)
}

pub(super) fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
