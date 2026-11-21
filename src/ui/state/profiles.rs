//! 资料卡 / 个人资料页 / 头像弹窗 / F2 大厅页的面板判据。

use crate::geometry::Rect;
use crate::runtime::ui::UiStateClassification;
use crate::runtime::ui::UiStateEvidence;
use crate::runtime::ui::UiStateKind as RuntimeUiStateKind;
use crate::ui::template::best_template_candidate_downscaled;
use image::DynamicImage;

use super::anchors::{
    PROFILE_PAGE_BUTTON_SCALE, PROFILE_PAGE_BUTTON_SCORE_MIN, PROFILE_PAGE_WARDROBE_ORIGIN,
    PROFILE_PAGE_WISHLIST_ORIGIN, profile_page_button_hit, profile_page_button_rects,
};
use super::classifier::TemplateUiStateClassifier;
use super::diagnostic::{candidate_matches, hit_matches, template_probe};
use super::qianxing::{QIANXING_F2_BUTTON_MIN, QIANXING_F2_BUTTON_SCALE};
use super::world::world_probe_evidence;

/// ── 资料卡四分类（提瓦特 / 千星 × 好友 / 非好友）─────────────────────────
///
/// 全部使用纯文字模板，不依赖 OCR。每个模板有独立搜索区，且都限定在资料卡内：
/// - 提瓦特资料卡：左页上部 `冒险等阶` / `世界等级`（两张 1920×1080 实拍帧一致）；
/// - 千星资料卡：上部 `奇域等级`，下部白色胶囊 `添加好友` / `查看资料`。
/// 坐标以规范画布 1920×1080 为准（`classify_at` 已归一化）。
///
/// 判定顺序本身就是判据：
/// 1. 提瓦特：两个文字锚点**同时命中** → 提瓦特资料卡；再按「添加好友」有无分好友/非好友。
/// 2. 千星：`奇域等级` 命中 → 千星资料卡；`添加好友` 命中=非好友，否则 `查看资料`=好友。
///    不使用「本期缤纷评分」，避免把其它千星页/评分变化卷进来。
pub(super) const TEYVAT_PROFILE_RANK_SEARCH: Rect = Rect::new(415, 447, 300, 42);
pub(super) const TEYVAT_PROFILE_WORLD_SEARCH: Rect = Rect::new(415, 489, 300, 40);
/// 两个文字锚点用 1/2 下采样：模板 85×23/86×23，搜索区 300×42/300×40，
/// 正样本 0.944/0.927，千星页与其它面板 ≤0.77（实测），空档充足。
pub(super) const TEYVAT_PROFILE_ANCHOR_SCALE: u32 = 2;
/// 提瓦特「添加好友」在卡片右下角的实测原点（模板 98×25 纯文字）。
/// 好友帧没有该按钮，纯文字模板仍可能在附近拿到 0.947；因此**分数 + 位置**双门控。
pub(super) const TEYVAT_PROFILE_ADD_FRIEND_ORIGIN: (i32, i32) = (1211, 815);
pub(super) const TEYVAT_PROFILE_ADD_FRIEND_TOLERANCE: i32 = 12;
/// 千星「奇域等级」：蓝紫渐变带内的白色文字，只留文字、不含左侧星标。
pub(super) const QIANXING_PROFILE_DOMAIN_SEARCH: Rect = Rect::new(780, 345, 120, 50);
/// 千星「添加好友」：卡片下部左侧白色胶囊（非好友）；好友帧这里没有按钮。
pub(super) const QIANXING_PROFILE_ADD_FRIEND_SEARCH: Rect = Rect::new(700, 550, 260, 60);
/// 千星「查看资料」：好友单按钮在左（新帧 ~793,573）、4 按钮版式在右（旧档案卡 ~1028,631），
/// 统一用同一块**三行按钮区**覆盖两种版式；区域仍限定在资料卡内。
pub(super) const QIANXING_PROFILE_VIEW_SEARCH: Rect = Rect::new(740, 540, 460, 190);
/// 资料卡文字锚点 / 动作胶囊的最低分。文字锚点正样本 ≥0.927，负样本 ≤0.77；
/// 动作胶囊正样本 0.915–1.0，负样本 ≤0.80（好友帧的「查看资料」模板 vs「添加好友」0.744）。
pub(super) const PROFILE_CARD_ANCHOR_MIN: f32 = 0.90;
pub(super) const PROFILE_CARD_ACTION_MIN: f32 = 0.95;
pub(super) const PROFILE_CARD_VIEW_MIN: f32 = 0.90;

/// 提瓦特资料卡「添加好友」的搜索区（`teyvat-profile-add-friend.png`，1×）。
///
/// 按钮只在非好友帧出现，位于卡片右下角；纯文字裁剪后实测命中左上角恒为
/// (1211,815)（98×25）。这里只覆盖按钮块 + 上下 32px 余量，能容纳卡片高度 /
/// UI 缩放的抖动，又不是整帧，也不与千星资料卡按钮区重叠。
pub(super) const TEYVAT_PROFILE_ADD_FRIEND_SEARCH: Rect = Rect::new(1179, 783, 162, 89);

impl TemplateUiStateClassifier {
    /// F2 千星大厅页（多人面板）的**首选**判据：左下「大厅列表」+ 右下「邀请玩家」
    /// 两个按钮锚点都命中。
    ///
    /// 两个模板都只保留文字（纯文字裁剪后 88x25 / 117x32），不含图标、底栏或背景；
    /// 搜索区分别复用 `screen.hall_bar_rect`（左下）和 `screen.qianxing_f2_invite_rect`
    /// （右下），所以不会拿宽背景冒充按钮。命中后返回 Secondary·多人面板，
    /// 由 `surface_of` 映射为 `Secondary(MultiplayerPanel)`。
    pub(super) fn f2_hall_page_by_buttons(
        &self,
        image: &DynamicImage,
    ) -> Option<UiStateClassification> {
        let threshold = self
            .templates
            .chat_templates
            .marker_threshold
            .max(QIANXING_F2_BUTTON_MIN);
        let left_region: Rect = self.screen.hall_bar_rect.into();
        let right_region: Rect = self.screen.qianxing_f2_invite_rect.into();
        let left = best_template_candidate_downscaled(
            image,
            Some(left_region),
            &self.templates.qianxing_f2_hall_list_template,
            QIANXING_F2_BUTTON_SCALE,
        )
        .ok()
        .flatten()?;
        if !left.quality_ok || left.score < threshold {
            return None;
        }
        let right = best_template_candidate_downscaled(
            image,
            Some(right_region),
            &self.templates.qianxing_f2_invite_player_template,
            QIANXING_F2_BUTTON_SCALE,
        )
        .ok()
        .flatten()?;
        if !right.quality_ok || right.score < threshold {
            return None;
        }
        Some(UiStateClassification::with_evidence(
            RuntimeUiStateKind::Secondary,
            "secondary:qianxing_panel",
            UiStateEvidence::new(
                vec![
                    template_probe(
                        &self.templates.qianxing_f2_hall_list_template,
                        left_region,
                        threshold,
                        Some(&left),
                    ),
                    template_probe(
                        &self.templates.qianxing_f2_invite_player_template,
                        right_region,
                        threshold,
                        Some(&right),
                    ),
                ],
                None,
                "secondary:qianxing_panel_buttons",
            )
            .with_world_probe(world_probe_evidence(image)),
        ))
    }

    /// 头像弹窗窄探针：只匹配「查看千星奇域档案」一个模板。
    /// 叠在二级聊天之上时基础链只会判成 secondary，这里用单次下采样匹配兜住。
    pub(super) fn avatar_popup_probe(&self, image: &DynamicImage) -> Option<UiStateClassification> {
        let threshold = self.templates.chat_templates.marker_threshold;
        let region = self.panels.as_ref()?.invite_view_star;
        let scale = self
            .templates
            .panel_downsample_factors
            .get("invite_view_star")
            .copied()
            .unwrap_or(2);
        let candidate = best_template_candidate_downscaled(
            image,
            Some(region),
            &self.templates.invite_view_star_template,
            scale,
        )
        .ok()
        .flatten()?;
        if !candidate_matches(&Some(candidate.clone()), threshold) {
            return None;
        }
        Some(UiStateClassification::with_evidence(
            RuntimeUiStateKind::Panel,
            "panel:avatar_popup",
            UiStateEvidence::new(
                vec![template_probe(
                    &self.templates.invite_view_star_template,
                    region,
                    threshold,
                    Some(&candidate),
                )],
                None,
                "panel:avatar_popup",
            ),
        ))
    }
}

impl TemplateUiStateClassifier {
    /// 个人资料页：右下角「心愿单」+「衣橱」**两个按钮模板同时命中**才成立。
    ///
    /// 为什么是双按钮，而不是整页/单一锚点：
    /// - 这一页没有独有的标题文字，左侧资料卡与其它千星子页共用版式，
    ///   单靠「右上白色关闭按钮」只能判到 `panel:qianxing_page`（Other）；
    /// - 两个按钮是固定布局（同一 y、间隙 24 px），且只属于这一页；
    /// - **单命中不算**：其它页面同位置也可能出现白色胶囊按钮，只有成对命中
    ///   才代表个人资料页，因此任一模板低于阈值或不在自己的按钮位置立即返回 `None`。
    ///
    /// 两个搜索区分别只覆盖各自按钮，模板窗口永远碰不到底部 UID 行，
    /// 既不影响匹配分数，也不会因为 UID 内容变化而漂移。
    ///
    /// 调用方必须传入**已归一化到规范画布（1920×1080）**的画面——按钮位置门控
    /// 用的是规范坐标，`classify_at` 入口已经做过 `resize_to_canvas`。
    pub(super) fn profile_page_by_buttons(
        &self,
        image: &DynamicImage,
    ) -> Option<UiStateClassification> {
        let (wishlist_region, wardrobe_region) = profile_page_button_rects(image)?;
        let wishlist = best_template_candidate_downscaled(
            image,
            Some(wishlist_region),
            &self.templates.qianxing_profile_page_wishlist_template,
            PROFILE_PAGE_BUTTON_SCALE,
        )
        .ok()
        .flatten()?;
        if !profile_page_button_hit(
            &wishlist,
            PROFILE_PAGE_WISHLIST_ORIGIN,
            PROFILE_PAGE_BUTTON_SCORE_MIN,
        ) {
            return None;
        }
        let wardrobe = best_template_candidate_downscaled(
            image,
            Some(wardrobe_region),
            &self.templates.qianxing_profile_page_wardrobe_template,
            PROFILE_PAGE_BUTTON_SCALE,
        )
        .ok()
        .flatten()?;
        if !profile_page_button_hit(
            &wardrobe,
            PROFILE_PAGE_WARDROBE_ORIGIN,
            PROFILE_PAGE_BUTTON_SCORE_MIN,
        ) {
            return None;
        }
        Some(UiStateClassification::with_evidence(
            RuntimeUiStateKind::Panel,
            "panel:profile_page",
            UiStateEvidence::new(
                vec![
                    template_probe(
                        &self.templates.qianxing_profile_page_wishlist_template,
                        wishlist_region,
                        PROFILE_PAGE_BUTTON_SCORE_MIN,
                        Some(&wishlist),
                    ),
                    template_probe(
                        &self.templates.qianxing_profile_page_wardrobe_template,
                        wardrobe_region,
                        PROFILE_PAGE_BUTTON_SCORE_MIN,
                        Some(&wardrobe),
                    ),
                ],
                None,
                "panel:profile_page",
            ),
        ))
    }
}

impl TemplateUiStateClassifier {
    /// 资料卡四分类：提瓦特 / 千星 × 好友 / 非好友，全部模板匹配，不用 OCR。
    ///
    /// 判据（与用户确认的效果一致）：
    /// - 提瓦特：`冒险等阶` **且** `世界等级` → 提瓦特资料卡；
    ///   `添加好友`（深色按钮白字）命中 → 非好友，未命中 → 好友。
    /// - 千星：`奇域等级` → 千星资料卡；`添加好友`（白色胶囊深色字）命中 → 非好友；
    ///   未命中但 `查看资料`（白色胶囊）命中 → 好友。
    ///
    /// 为什么放在旧档案卡之前：旧的 `panel:archive_card` 只是兼容兜底，
    /// 只要资料卡专属锚点命中就必须先出具体四分类，不能让旧判据抢走。
    /// 也不用「本期缤纷评分」：它会随赛季变化，且不是好友判据。
    pub(super) fn profile_card_by_templates(
        &self,
        image: &DynamicImage,
        threshold: f32,
    ) -> Option<UiStateClassification> {
        let anchor_min = threshold.max(PROFILE_CARD_ANCHOR_MIN);
        let action_min = threshold.max(PROFILE_CARD_ACTION_MIN);
        let view_min = threshold.max(PROFILE_CARD_VIEW_MIN);

        // ── 提瓦特资料卡：冒险等阶 + 世界等级 双文字锚点 ──────────────────
        let teyvat_rank = best_template_candidate_downscaled(
            image,
            Some(TEYVAT_PROFILE_RANK_SEARCH),
            &self.templates.teyvat_profile_adventure_rank_template,
            TEYVAT_PROFILE_ANCHOR_SCALE,
        )
        .ok()
        .flatten();
        let teyvat_world = best_template_candidate_downscaled(
            image,
            Some(TEYVAT_PROFILE_WORLD_SEARCH),
            &self.templates.teyvat_profile_world_level_template,
            TEYVAT_PROFILE_ANCHOR_SCALE,
        )
        .ok()
        .flatten();
        let teyvat_anchors = teyvat_rank
            .as_ref()
            .is_some_and(|hit| hit_matches(hit, anchor_min))
            && teyvat_world
                .as_ref()
                .is_some_and(|hit| hit_matches(hit, anchor_min));
        if teyvat_anchors {
            let add = best_template_candidate_downscaled(
                image,
                Some(TEYVAT_PROFILE_ADD_FRIEND_SEARCH),
                &self.templates.teyvat_profile_add_friend_template,
                1,
            )
            .ok()
            .flatten();
            // 位置门控：好友帧没有按钮，纯文字模板仍可能在卡片右下角附近拿到
            // 0.947；分数 + 实测原点双门控才能把好友帧稳定挡在 0.95 以下。
            let nonfriend = add.as_ref().is_some_and(|hit| {
                hit_matches(hit, action_min)
                    && (hit.x - TEYVAT_PROFILE_ADD_FRIEND_ORIGIN.0).abs()
                        <= TEYVAT_PROFILE_ADD_FRIEND_TOLERANCE
                    && (hit.y - TEYVAT_PROFILE_ADD_FRIEND_ORIGIN.1).abs()
                        <= TEYVAT_PROFILE_ADD_FRIEND_TOLERANCE
            });
            let label = if nonfriend {
                "panel:teyvat_profile_nonfriend"
            } else {
                "panel:teyvat_profile_friend"
            };
            return Some(UiStateClassification::with_evidence(
                RuntimeUiStateKind::Panel,
                label,
                UiStateEvidence::new(
                    vec![
                        template_probe(
                            &self.templates.teyvat_profile_adventure_rank_template,
                            TEYVAT_PROFILE_RANK_SEARCH,
                            anchor_min,
                            teyvat_rank.as_ref(),
                        ),
                        template_probe(
                            &self.templates.teyvat_profile_world_level_template,
                            TEYVAT_PROFILE_WORLD_SEARCH,
                            anchor_min,
                            teyvat_world.as_ref(),
                        ),
                        template_probe(
                            &self.templates.teyvat_profile_add_friend_template,
                            TEYVAT_PROFILE_ADD_FRIEND_SEARCH,
                            action_min,
                            add.as_ref(),
                        ),
                    ],
                    None,
                    label,
                ),
            ));
        }

        // ── 千星资料卡：奇域等级 单文字锚点 ──────────────────────────────
        let domain = best_template_candidate_downscaled(
            image,
            Some(QIANXING_PROFILE_DOMAIN_SEARCH),
            &self.templates.qianxing_profile_domain_level_template,
            1,
        )
        .ok()
        .flatten();
        if !domain
            .as_ref()
            .is_some_and(|hit| hit_matches(hit, anchor_min))
        {
            return None;
        }
        let add = best_template_candidate_downscaled(
            image,
            Some(QIANXING_PROFILE_ADD_FRIEND_SEARCH),
            &self.templates.qianxing_profile_add_friend_template,
            1,
        )
        .ok()
        .flatten();
        if add.as_ref().is_some_and(|hit| hit_matches(hit, action_min)) {
            let label = "panel:qianxing_profile_nonfriend";
            return Some(UiStateClassification::with_evidence(
                RuntimeUiStateKind::Panel,
                label,
                UiStateEvidence::new(
                    vec![
                        template_probe(
                            &self.templates.qianxing_profile_domain_level_template,
                            QIANXING_PROFILE_DOMAIN_SEARCH,
                            anchor_min,
                            domain.as_ref(),
                        ),
                        template_probe(
                            &self.templates.qianxing_profile_add_friend_template,
                            QIANXING_PROFILE_ADD_FRIEND_SEARCH,
                            action_min,
                            add.as_ref(),
                        ),
                    ],
                    None,
                    label,
                ),
            ));
        }
        let view = best_template_candidate_downscaled(
            image,
            Some(QIANXING_PROFILE_VIEW_SEARCH),
            &self.templates.qianxing_profile_view_profile_template,
            1,
        )
        .ok()
        .flatten()
        .filter(|hit| hit_matches(hit, view_min));
        let (label, region, candidate) = match view {
            Some(candidate) => (
                "panel:qianxing_profile_friend",
                QIANXING_PROFILE_VIEW_SEARCH,
                Some(candidate),
            ),
            None => return None,
        };
        Some(UiStateClassification::with_evidence(
            RuntimeUiStateKind::Panel,
            label,
            UiStateEvidence::new(
                vec![
                    template_probe(
                        &self.templates.qianxing_profile_domain_level_template,
                        QIANXING_PROFILE_DOMAIN_SEARCH,
                        anchor_min,
                        domain.as_ref(),
                    ),
                    template_probe(
                        &self.templates.qianxing_profile_add_friend_template,
                        QIANXING_PROFILE_ADD_FRIEND_SEARCH,
                        action_min,
                        add.as_ref(),
                    ),
                    template_probe(
                        &self.templates.qianxing_profile_view_profile_template,
                        region,
                        view_min,
                        candidate.as_ref(),
                    ),
                ],
                None,
                label,
            ),
        ))
    }
}
