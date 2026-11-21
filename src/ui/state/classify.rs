//! 逐帧基础判据链：聊天标记 → 大厅底栏 → 加载 → 返回按钮 → 二级聊天 → 派蒙宫格。

use crate::config;
use crate::geometry::Rect;
use crate::observation::chat::count_scanned_chat_markers;
use crate::observation::chat::scan_chat_markers;
use crate::runtime::ui::UiMarkerProbeEvidence;
use crate::runtime::ui::UiStateEvidence;
use crate::ui::template::best_template_candidate;
use anyhow::Result;
use image::DynamicImage;
use std::time::Instant;

use super::args::ResolvedUiTemplateArgs;
use super::diagnostic::{candidate_matches, elapsed_ms, evidence_rect, template_probe};
use super::hall::{
    HALL_BAR_WHITE_NEAR, hall_bar_looks_like_hall_keys, hall_bar_signature,
    hall_bar_signature_with_threshold,
};
use super::kind::UiState;
use super::paimon::paimon_menu_grid_structure;
use super::sampling::loading_rule;

#[cfg(test)]
pub(super) fn detect_ui_state(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<UiState> {
    Ok(detect_ui_state_with_evidence(image, templates, screen)?.0)
}

pub(super) fn detect_ui_state_with_evidence(
    image: &DynamicImage,
    templates: &ResolvedUiTemplateArgs,
    screen: &config::ScreenConfig,
) -> Result<(UiState, UiStateEvidence)> {
    let started = Instant::now();
    let threshold = templates.chat_templates.marker_threshold;
    let mut template_probes = Vec::with_capacity(2);

    // ── 1) 聊天标记：最高优先级的模板匹配 ────────────────────────────────
    // 三个标记模板形状**完全不同**：蓝色建筑图标 / 黄色四角星 / 粉色人形剪影。
    // 实测 328 帧：蓝/黄只出现在千星侧（大厅 93 蓝、22 黄、1 蓝+黄，面板 3+7），
    // 提瓦特 0 帧带蓝/黄 ⇒ 蓝/黄命中就是大厅，不需要其它信息（也不需要算底栏）。
    // 粉色**不能作判据**：提瓦特世界聊天 6/6 帧都是 pink-only（第 46 节缺陷②），
    // 所以 pink-only 不在这里返回，落到下面的二次判断流程（底栏结构 → 世界判据）。
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
    if blue > 0 || yellow > 0 {
        log::debug!(target: "timing",
            "UI 状态检测耗时: total={}ms marker={}ms state=primary rule=primary_chat_markers blue={} yellow={} pink={}",
            elapsed_ms(started),
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

    // ── 2) 没有蓝/黄（无标记，或只有粉色）：底栏结构 → 大厅 ─────────────
    // 底栏是**唯一**的大厅判据（蓝/黄标记之外），按实际像素几何看三件事：
    //   ① 块数 **7–8**（用户 2026-11-10 明确：千星大厅底栏只有 7~8 个白键；
    //      聊天框开=7、关=8，两者都是 Primary。旧范围 7–14 过宽）
    //   ② 每块高度落在 22–36 px（实测 26–28；菜单整块白底是 60–64）
    //   ③ 所有块在**同一条水平线**上：顶部 y 极差 ≤4 px
    // 白色判据用严格的 225（不是纯 255：JPEG/缩放帧的白色只有 240–254）。
    let hall_region: Rect = screen.hall_bar_rect.into();
    let (hall_tiles, hall_min_height_px, hall_max_height_px, hall_top_spread_px) =
        hall_bar_signature(image, hall_region);
    let hall_structure = hall_bar_looks_like_hall_keys(
        hall_tiles,
        hall_min_height_px,
        hall_max_height_px,
        hall_top_spread_px,
    );
    // **有损输入兜底**：JPEG/DPI 缩放帧的白键被压到 240–254，严格 255 会数出 0 块。
    // 用户要求 Primary 的键数判据只认纯 255，所以这里**不拿 225 数键**，只在
    // 「严格 255 一块都没有」时用近白结构确认它确实是大厅那排 7–8 键（同高、同一水平线），
    // 再判 Primary；证据规则名为 `primary_hall_bar_near_white`，日志可区分。
    // 线上无损抓图仍走严格 255 主路径。
    let near_white_hall = if !hall_structure && hall_tiles == 0 {
        let (near_tiles, near_min, near_max, near_spread) =
            hall_bar_signature_with_threshold(image, hall_region, HALL_BAR_WHITE_NEAR);
        // **关键护栏**：主菜单千星 hub 宫格也有 7–9 块白瓦片，但它右下角有「返回提瓦特」，
        // 必须先让主菜单判据优先，不能把菜单判成大厅。
        let menu_visible = if near_tiles >= 7 {
            let region: Rect = screen.main_menu_rect.into();
            let candidate =
                best_template_candidate(image, Some(region), &templates.main_menu_template)?;
            candidate_matches(&candidate, threshold)
        } else {
            false
        };
        // 有损 JPEG 的大厅底栏白键会被压到 240–254，且可能多/少拆出一块：
        // 同高（26–32px）、同水平线（极差 ≤4px）、7–9 块即按大厅兜底；
        // 严格 255 主路径与「7–8」用户口径不变。
        let loose_lossy =
            (7..=9).contains(&near_tiles) && near_min >= 26 && near_max <= 32 && near_spread <= 4;
        !menu_visible
            && (hall_bar_looks_like_hall_keys(near_tiles, near_min, near_max, near_spread)
                || loose_lossy)
    } else {
        false
    };
    if hall_structure || near_white_hall {
        let hall_rule = if hall_structure {
            "primary_hall_bar"
        } else {
            "primary_hall_bar_near_white"
        };
        log::debug!(target: "timing",
            "UI 状态检测耗时: total={}ms marker={}ms state=primary rule={} tiles={} h_min={} h_max={} top_spread={}",
            elapsed_ms(started),
            marker_ms,
            hall_rule,
            hall_tiles,
            hall_min_height_px,
            hall_max_height_px,
            hall_top_spread_px
        );
        return Ok((
            UiState::primary_hall(),
            UiStateEvidence::new(template_probes, Some(marker_probe), hall_rule),
        ));
    }

    // ── 3) 加载界面是**真实界面**，不是 unknown，所以在剩下的分支里最先判它。
    // 它整屏只有极端色调，任何游戏界面的暗/白占比都远达不到（见上面两条阈值的实测来源），
    // 因此放在最前面既便宜又不会抢掉其它状态。具体外观由证据里的规则名区分。
    if let Some(rule) = loading_rule(image) {
        let total_ms = elapsed_ms(started);
        log::debug!(target: "timing",
            "UI 状态检测耗时: total={}ms state={rule}",
            total_ms,
        );
        return Ok((
            UiState::loading(),
            UiStateEvidence::new(template_probes, None, rule),
        ));
    }

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
        // 奇域主菜单上也挂着同一个圆形返回按钮，所以"返回按钮命中"并不等于二级聊天。
        // 再看右下角有没有「返回提瓦特」按钮：有就是主菜单（technical-docs/ui-state-machine.md §44）。
        // 这次匹配只在返回按钮命中之后才做——它本来就是用来否决二级判定的，
        // 放在这个位置，让"不是二级界面"的常见路径不必多付一次模板匹配。
        let menu_region: Rect = screen.main_menu_rect.into();
        let menu_candidate =
            best_template_candidate(image, Some(menu_region), &templates.main_menu_template)?;
        let menu_visible = candidate_matches(&menu_candidate, threshold);
        template_probes.push(template_probe(
            &templates.main_menu_template,
            menu_region,
            threshold,
            menu_candidate.as_ref(),
        ));
        let back_ms = elapsed_ms(back_started);
        if menu_visible {
            log::debug!(target: "timing",
                "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=main_menu",
                elapsed_ms(started),
                back_ms,
                marker_ms
            );
            return Ok((
                UiState::main_menu(),
                UiStateEvidence::new(template_probes, None, "main_menu_template"),
            ));
        }
        // 二级聊天**专属锚点**：输入行右侧的「发送」圆钮（深色圆底 + 黄色对勾）。
        // 主菜单/主菜单大世界/千星多人面板/千星地图都没有它；而奇域主菜单淡出过渡帧
        // 仍带着左上圆形返回按钮（旧实现会把这类帧判成 Secondary，见 §44）。
        // 先于派蒙探针判定：聊天界面必须直接判二级，同时「无发送锚点」本身就是
        // 主菜单大世界多证据中的一条（无聊天证据）。
        let submit_region: Rect = screen.secondary_chat_submit_rect.into();
        let submit_candidate = best_template_candidate(
            image,
            Some(submit_region),
            &templates.secondary_chat_submit_template,
        )?;
        let submit_visible = candidate_matches(&submit_candidate, threshold);
        template_probes.push(template_probe(
            &templates.secondary_chat_submit_template,
            submit_region,
            threshold,
            submit_candidate.as_ref(),
        ));
        if submit_visible {
            log::debug!(target: "timing",
                "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=secondary_chat",
                elapsed_ms(started),
                back_ms,
                marker_ms
            );
            return Ok((
                UiState::secondary_chat(),
                UiStateEvidence::new(template_probes, None, "secondary_chat_anchor"),
            ));
        }
        let back_ms = elapsed_ms(back_started);

        // 主菜单大世界（ESC 系统菜单）**布局无关**判定（用户要求极快、不用商城、
        // 不依赖固定坐标）：
        //   ① 返回按钮命中（已走到这里）；② 无「返回提瓦特」（上方已否决）；
        //   ③ 无聊天发送锚点（刚判过）；
        //   ④ 左侧菜单区域检测到「多瓦片宫格结构」（downscale 颜色 + 连通域，
        //      要求 ≥8 块同尺寸瓦片、≥3 列、≥2 行）。
        // 菜单项平移/增删/顺序变化都不影响：只看结构，不看「队伍配置/成就」位置。
        let grid_started = Instant::now();
        let paimon_grid = paimon_menu_grid_structure(image);
        let grid_ms = elapsed_ms(grid_started);
        if let Some((grid_tiles, grid_rows, grid_cols)) = paimon_grid {
            log::debug!(target: "timing",
                "UI 状态检测耗时: total={}ms back={}ms marker={}ms grid={}ms state=paimon_menu tiles={} rows={} cols={}",
                elapsed_ms(started),
                back_ms,
                marker_ms,
                grid_ms,
                grid_tiles,
                grid_rows,
                grid_cols
            );
            // 规则名 `paimon_menu_grid`：residency/transaction/state_observation
            // 用它区分「主菜单大世界 vs 主菜单千星」来决定是否允许 Esc 恢复大厅。
            return Ok((
                UiState::paimon_menu(),
                UiStateEvidence::new(template_probes, None, "paimon_menu_grid"),
            ));
        }
        // 返回按钮命中但没有足够的派蒙/聊天证据：这是主菜单/过渡/其它面板，不硬猜。
        log::debug!(target: "timing",
            "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=unknown (back_hit_without_chat_anchor)",
            elapsed_ms(started),
            back_ms,
            marker_ms
        );
        return Ok((
            UiState::unknown(),
            UiStateEvidence::new(template_probes, Some(marker_probe), "no_reliable_anchor"),
        ));
    }
    let back_ms = elapsed_ms(back_started);

    log::debug!(target: "timing",
        "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=unknown",
        elapsed_ms(started),
        back_ms,
        marker_ms
    );
    Ok((
        UiState::unknown(),
        UiStateEvidence::new(template_probes, Some(marker_probe), "no_reliable_anchor"),
    ))
}
