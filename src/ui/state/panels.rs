//! 通用面板判据：确认弹窗、更多设置菜单与面板细化总表。

use crate::geometry::Rect;
use crate::runtime::ui::UiStateClassification;
use crate::runtime::ui::UiStateEvidence;
use crate::runtime::ui::UiStateKind;
use crate::ui::template::best_template_candidate_downscaled;
use image::DynamicImage;
use std::path::PathBuf;

use super::anchors::{
    QIANXING_EXIT_MENU_ORIGIN, QIANXING_EXIT_MENU_SCALE, QIANXING_EXIT_MENU_SEARCH,
    QIANXING_EXIT_MENU_TOLERANCE,
};
use super::classifier::TemplateUiStateClassifier;
use super::diagnostic::{
    CONFIRM_MESSAGE_MIN, CONFIRM_MESSAGE_SCALE, candidate_matches, confirm_message_keyword,
    confirm_probe_log_due, confirm_region_label, template_probe, text_match_quality_label,
    text_match_reject_label,
};
use super::regions::PanelRegions;
use super::sampling::{
    beige_panel_fraction, is_qianxing_overlay_close, overlay_close_white_fraction, screen_tone,
};

impl TemplateUiStateClassifier {
    /// 确认弹窗：**确认按钮 + 关键词文案**双锚点。
    ///
    /// 子类型由文案决定，优先级 `屏蔽玩家 > 进入大厅 > 离开大厅`；多个文案同时
    /// 命中时只取最高优先级，避免一帧出现两个确认子类型。按钮或文案任一缺失都
    /// 返回 `None`，调用方继续按其它面板判据分类——绝不回退到旧的单一按钮标签。
    pub(super) fn confirm_dialog_by_anchors(
        &self,
        image: &DynamicImage,
        panels: &PanelRegions,
        threshold: f32,
    ) -> Option<UiStateClassification> {
        // 「确认」已裁到 59×30（bbox + 2px）；1× 正样本 0.989、命中
        // (1151,741)，4× 会被 floor 成 56×28 并把位置量化到 (1152,740)。
        let button_scale = self
            .templates
            .panel_downsample_factors
            .get("confirm_button")
            .copied()
            .unwrap_or(1);
        let button_candidate = best_template_candidate_downscaled(
            image,
            Some(panels.confirm_button),
            &self.templates.confirm_button_template,
            button_scale,
        );
        // 诊断：确认弹窗在实机上判不出来时，需要知道按钮候选到底是"没候选"
        // 还是"有候选但分数/质量不够"。限速 2 秒，不会刷屏。
        if confirm_probe_log_due() {
            match &button_candidate {
                Ok(Some(hit)) => log::debug!(
                    "确认弹窗按钮候选：在{}里找到「确认」按钮，命中位置 ({}, {}) 为 {}×{}，相似度 {:.4}（阈值 {:.3}），{}",
                    confirm_region_label(panels.confirm_button, button_scale),
                    hit.x,
                    hit.y,
                    hit.width,
                    hit.height,
                    hit.score,
                    threshold,
                    text_match_quality_label(hit.quality_ok)
                ),
                Ok(None) => log::debug!(
                    "确认弹窗按钮无候选：在{}里没有找到「确认」按钮，模板「{}」；本帧按没有确认弹窗处理",
                    confirm_region_label(panels.confirm_button, button_scale),
                    self.templates.confirm_button_template.display()
                ),
                Err(error) => log::debug!("确认弹窗按钮探针失败：{error:#}"),
            }
        }
        let button = button_candidate.ok().flatten()?;
        if !button.quality_ok || button.score < threshold {
            // 只有分数接近阈值时才记一条：逐帧无条件打会刷屏，但确认弹窗
            // 现场需要知道"按钮到底差多少、是不是质量门控拒绝"。
            if button.score >= 0.80 {
                log::debug!(
                    "确认弹窗未成立：按钮在 ({}, {}) 的 {}×{} 处相似度只有 {:.4}（要求不低于 {:.3}），{}；本帧继续按其它面板判据分类",
                    button.x,
                    button.y,
                    button.width,
                    button.height,
                    button.score,
                    threshold,
                    text_match_reject_label(button.quality_ok)
                );
            }
            return None;
        }
        let messages = [
            (
                "panel:confirm_block_player",
                &self.templates.confirm_message_block_template,
            ),
            (
                "panel:confirm_enter_hall",
                &self.templates.confirm_message_random_hall_template,
            ),
            (
                "panel:confirm_leave_hall",
                &self.templates.confirm_message_return_teyvat_template,
            ),
        ];
        for (label, template) in messages {
            let Some(message) = best_template_candidate_downscaled(
                image,
                Some(panels.confirm_message),
                template,
                CONFIRM_MESSAGE_SCALE,
            )
            .ok()
            .flatten() else {
                continue;
            };
            if !message.quality_ok || message.score < CONFIRM_MESSAGE_MIN {
                if message.score >= 0.90 {
                    log::debug!(
                        "确认弹窗文案未成立：「{}」在 {}里的相似度只有 {:.4}（要求不低于 {:.3}），{}；本帧不按该子类型分类",
                        confirm_message_keyword(label),
                        confirm_region_label(panels.confirm_message, CONFIRM_MESSAGE_SCALE),
                        message.score,
                        CONFIRM_MESSAGE_MIN,
                        text_match_reject_label(message.quality_ok)
                    );
                }
                continue;
            }
            return Some(UiStateClassification::with_evidence(
                UiStateKind::Panel,
                label,
                UiStateEvidence::new(
                    vec![
                        template_probe(
                            &self.templates.confirm_button_template,
                            panels.confirm_button,
                            threshold,
                            Some(&button),
                        ),
                        template_probe(
                            template,
                            panels.confirm_message,
                            CONFIRM_MESSAGE_MIN,
                            Some(&message),
                        ),
                    ],
                    None,
                    label,
                ),
            ));
        }
        None
    }
}

impl TemplateUiStateClassifier {
    /// 更多设置菜单优先于资料卡四分类。
    ///
    /// ⋯ 菜单展开时，底下仍然是资料卡；如果先判资料卡，会把「更多设置」
    /// 误判成「千星资料卡·好友」。这个探针只在菜单自身窄条区域里匹配，
    /// 不会扫到资料卡的其它按钮。
    pub(super) fn more_settings_by_menu(
        &self,
        image: &DynamicImage,
        panels: &PanelRegions,
        threshold: f32,
    ) -> Option<UiStateClassification> {
        // 菜单锚点本身已经限定在菜单窄条区域，不再依赖米黄占比；
        // 否则「⋯菜单叠在资料卡上」时米黄只有 0.26，会被资料卡抢走。
        // 菜单里独有的「屏蔽聊天」：区域仍是菜单自身，阈值 0.92
        // （与 refine_panel 后面同一条判据保持一致）。
        let block_min = threshold.max(0.92);
        if let Some(candidate) = best_template_candidate_downscaled(
            image,
            Some(panels.friend_block_chat),
            &self.templates.friend_block_chat_template,
            4,
        )
        .ok()
        .flatten()
            && candidate.quality_ok
            && candidate.score >= block_min
        {
            return Some(UiStateClassification::with_evidence(
                UiStateKind::Panel,
                "panel:more_settings",
                UiStateEvidence::new(
                    vec![template_probe(
                        &self.templates.friend_block_chat_template,
                        panels.friend_block_chat,
                        block_min,
                        Some(&candidate),
                    )],
                    None,
                    "panel:more_settings",
                ),
            ));
        }
        None
    }
}

impl TemplateUiStateClassifier {
    /// 面板细化：好友面板 / 添加好友页 / 更多设置 / 确认弹窗 /
    /// 千星档案卡 · 大厅卡 · 头像弹窗——这些界面的基础链判不出来（落 `Unknown`），
    /// 或叠在二级聊天之上（`Secondary`），用**已配置且实测命中**的模板逐个确认。
    ///
    /// **锚点必须专属该面板**：`friend-more-settings`（⋯）是**共用 chrome**，
    /// 在档案卡与⋯菜单上落在同一坐标（实测档案卡 0.992 甚至高于菜单 0.959），
    /// 因此它**不是**任何面板的判据；更多设置菜单改用**只有菜单展开时才存在**的
    /// `屏蔽聊天` 条目（实测 0.989，其它帧最高 0.854）。
    ///
    /// **文字锚点还必须限定区域**：`屏蔽聊天` 只保留文字，搜索区收窄到
    /// `panels.friend_block_chat`（⋯下拉菜单自身所在窄条），不扫全帧，避免把
    /// 提瓦特好友详情卡的同类按钮列误判成⋯菜单。
    ///
    /// **顺序也参与判别**：大厅卡与档案卡都含一条深色通栏按钮，`进入该大厅`
    /// （大厅卡 0.994）排在 `前往其大厅`（档案卡 0.962）之前，避免大厅卡被
    /// `前往其大厅` 的暗按钮误命中（实测大厅卡 0.905）。
    ///
    /// **成本护栏**：便宜且特异的排前面、**命中即停**；大区域靠调用方收窄区域 +
    /// 帧级短路摊薄（成本实测见 `dist/ui-flow/README.md`）。
    pub(super) fn refine_panel(&self, image: &DynamicImage) -> Option<UiStateClassification> {
        let panels = self.panels.as_ref()?;
        let threshold = self.templates.chat_templates.marker_threshold;
        // ── 确认弹窗优先：确认按钮 + 关键词文案双锚点 ──────────────────────
        // 所有确认弹窗都必须双锚点；只有按钮、没有文案不算确认弹窗。
        // 放在最前面：确认弹窗可能叠在千星覆盖页/好友页之上，若先走覆盖页门控
        // 就会在到达这里之前返回 `panel:qianxing_page`。按钮探针区域只有 81×58，
        // 未命中时成本可忽略；命中后才扫描 620×90 的文案行。
        if let Some(confirm) = self.confirm_dialog_by_anchors(image, panels, threshold) {
            return Some(confirm);
        }
        // ── 更多设置菜单优先于资料卡 ────────────────────────────────────
        // ⋯菜单展开时底下仍是资料卡；先判资料卡会把「更多设置」误判成
        // 「千星资料卡·好友」。菜单锚点只在自身窄条区域内匹配。
        if let Some(menu) = self.more_settings_by_menu(image, panels, threshold) {
            return Some(menu);
        }
        // ── 资料卡四分类优先（模板匹配，不用 OCR）────────────────────────
        // 顺序要求：必须在旧 `panel:archive_card`、添加好友详情之前命中即停，
        // 否则资料卡会被旧档案卡判据或 `not_state` 抢走。
        // 四类只由「冒险等阶+世界等级 / 奇域等级」与「添加好友 / 查看资料」决定。
        if let Some(profile) = self.profile_card_by_templates(image, threshold) {
            return Some(profile);
        }
        let anchor_threshold = threshold.max(0.95);
        // ── 个人资料页优先（右下角双按钮）──────────────────────────────
        // 比「右上白色关闭按钮」更具体，也便宜：只在右下角 520×130 里做两次
        // 1/2 下采样匹配。命中即停，不再计算米黄/暗度门控，也不会被
        // 档案卡/大厅卡分支抢走；单个按钮命中时返回 None，继续走原分支。
        if let Some(profile) = self.profile_page_by_buttons(image) {
            return Some(profile);
        }
        // ── 千星覆盖页优先（经验证集校验的门控）──────────────────────────
        // 千星页和提瓦特面板共用白色圆角控件，提瓦特模板在千星页会误命中（0.73–0.92）。
        // 门控来自正/负验证集：米黄面板占比（teyvat ≥0.34）、页面暗度（退出菜单 ≥0.55）、
        // 右上关闭按钮（千星页有）。每类专属锚点只在对应门控内使用，避免互串。
        let beige = beige_panel_fraction(image);
        let dark_ratio = screen_tone(image).0;
        let overlay_close = is_qianxing_overlay_close(image);
        if overlay_close {
            let close_frac = overlay_close_white_fraction(image);
            if beige < 0.34 || close_frac > 0.5 {
                return Some(UiStateClassification::with_evidence(
                    UiStateKind::Panel,
                    "panel:qianxing_page",
                    UiStateEvidence::new(Vec::new(), None, "panel:qianxing_page"),
                ));
            }
        }
        // 退出菜单覆层：米黄极少（0.047）或整片偏暗（暗占比高）。
        // 模板只保留第一行「退出至登录界面」（197×29），搜索区固定在该行实测位置
        // 附近；单行文字很短，再用原点门控挡住区域内其它近似文字。
        if beige < 0.12 || dark_ratio >= 0.40 {
            let candidate = best_template_candidate_downscaled(
                image,
                Some(QIANXING_EXIT_MENU_SEARCH),
                &self.templates.qianxing_exit_menu_template,
                QIANXING_EXIT_MENU_SCALE,
            )
            .ok()
            .flatten();
            if candidate.as_ref().is_some_and(|hit| {
                hit.quality_ok
                    && hit.score >= anchor_threshold
                    && (hit.x - QIANXING_EXIT_MENU_ORIGIN.0).abs() <= QIANXING_EXIT_MENU_TOLERANCE
                    && (hit.y - QIANXING_EXIT_MENU_ORIGIN.1).abs() <= QIANXING_EXIT_MENU_TOLERANCE
            }) {
                return Some(UiStateClassification::with_evidence(
                    UiStateKind::Panel,
                    "panel:qianxing_page",
                    UiStateEvidence::new(
                        vec![template_probe(
                            &self.templates.qianxing_exit_menu_template,
                            QIANXING_EXIT_MENU_SEARCH,
                            anchor_threshold,
                            candidate.as_ref(),
                        )],
                        None,
                        "panel:qianxing_page",
                    ),
                ));
            }
        }
        // 最后一个数字是**降采样倍数**：代价 ≈ 候选位置数 × 模板像素数，两个维度都除以
        // `scale`，总代价降到约 `1/scale⁴`（4 即约 256×）。只给"大模板"降采样；小图标
        // （friend-panel 49×35 / search 50×25）本身几乎免费，降采样反而把信息抹掉。
        let cases: [(&str, &PathBuf, Rect, u32); 6] = [
            (
                "panel:friend_list",
                &self.templates.friend_panel_template,
                panels.friend_panel,
                1,
            ),
            (
                "panel:more_settings",
                &self.templates.friend_block_chat_template,
                panels.friend_block_chat,
                4,
            ),
            (
                "panel:hall_card",
                &self.templates.invite_enter_hall_template,
                panels.invite_enter_hall,
                // 纯文字「进入该大厅」：1/2 下采样正样本 ~0.96；
                // 1/4 会把字形压得太小、正样本掉到阈值下。
                2,
            ),
            (
                "panel:archive_card",
                &self.templates.invite_goto_hall_template,
                panels.invite_goto_hall,
                // 纯文字「前往其大厅」本次裁到 122×26（bbox + 2px）。1/2 下采样时
                // 模板原点 (1017,573) 与区域原点 (780,560) 的相位差 2px，正样本
                // 掉到 0.889；1× 实测正样本 0.990/1.000、负样本 ≤0.863（质量拒绝），
                // 因此改回 1×。同区域的「查看资料」兜底也用同一个 1×。
                1,
            ),
            (
                "panel:add_friend",
                &self.templates.friend_search_panel_template,
                panels.friend_search_panel,
                1,
            ),
            (
                "panel:avatar_popup",
                &self.templates.invite_view_star_template,
                panels.invite_view_star,
                // 纯文字「查看千星奇域档案」：1/2 正样本 0.946，负样本
                // （大厅卡/更多设置）分数虽然接近 0.90，但文字质量校验会拒绝。
                2,
            ),
        ];
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
            let scale = self
                .templates
                .panel_downsample_factors
                .get(key)
                .copied()
                .unwrap_or(scale);
            let candidate =
                match best_template_candidate_downscaled(image, Some(region), path, scale) {
                    Ok(candidate) => candidate,
                    Err(_) => continue,
                };
            // 「屏蔽聊天」只有 4 个字：纯文字化后字形完整、抗锯齿连续，质量校验不再
            // 像旧版残缺 mask 那样偶然拒绝近似按钮（实测地图侧栏「真境剧诗」、
            // 好友详情「申请加入」都能到 0.90–0.91）。真面板实测 0.947–0.979，因此这个
            // 短文字锚点单独要求 ≥0.92；其余面板仍用统一阈值。
            let case_threshold = if label == "panel:more_settings" {
                threshold.max(0.92)
            } else {
                threshold
            };
            if candidate_matches(&candidate, case_threshold) {
                return Some(UiStateClassification::with_evidence(
                    UiStateKind::Panel,
                    label,
                    UiStateEvidence::new(
                        vec![template_probe(
                            path,
                            region,
                            case_threshold,
                            candidate.as_ref(),
                        )],
                        None,
                        label,
                    ),
                ));
            }
            // 档案卡有两种按钮版式：旧版有「前往其大厅」，新版只有「查看资料」。
            // 两者都是纯文字锚点（同一套文字质量校验），新版用同目录的
            // `invite-view-profile.png` 在同一搜索区内兜底；文件不存在时匹配器返回
            // None，分类继续按原流程落到 qianxing_page/unknown。
            // 「添加好友」不再是档案卡兜底：它只服务提瓦特资料卡「非好友」判据，
            // 且该按钮会被资料卡浮层覆盖。
            if label == "panel:archive_card" {
                let alternate = path.with_file_name("invite-view-profile.png");
                if let Ok(Some(candidate)) =
                    best_template_candidate_downscaled(image, Some(region), &alternate, scale)
                    && candidate.quality_ok
                    && candidate.score >= threshold
                {
                    return Some(UiStateClassification::with_evidence(
                        UiStateKind::Panel,
                        label,
                        UiStateEvidence::new(
                            vec![template_probe(
                                &alternate,
                                region,
                                threshold,
                                Some(&candidate),
                            )],
                            None,
                            label,
                        ),
                    ));
                }
            }
        }
        None
    }
}
