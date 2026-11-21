//! 分类入口编排：分类器状态、`classify_at` 与 `UiStateClassifier` 实现。

use crate::config;
use crate::geometry::Rect;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::ui::UiStateClassification;
use crate::runtime::ui::UiStateClassifier;
use crate::runtime::ui::UiStateEvidence;
use crate::runtime::ui::UiStateKind;
use crate::ui::frame::Canvas;
use crate::ui::frame::resize_to_canvas;
use crate::ui::template::TemplateHit;
use crate::ui::template::best_template_candidate;
use anyhow::Result;
use image::DynamicImage;
use std::time::Instant;

use super::anchors::{
    QIANXING_MAP_ANCHOR_SCORE_MIN, QIANXING_MAP_ANCHOR_SEARCH, QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT,
    QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH, qianxing_map_anchor_measure, qianxing_map_anchor_rect,
};
use super::args::ResolvedUiTemplateArgs;
use super::classify::detect_ui_state_with_evidence;
use super::diagnostic::template_probe;
use super::fallback::{GameEntryDetection, GameFallbackState, probe_game_state};
use super::qianxing::{is_bright_qianxing_map, is_qianxing_map, is_qianxing_player_panel};
use super::regions::PanelRegions;
use super::sampling::is_qianxing_overlay_close;
use super::world::{world_is_teyvat_in, world_probe_evidence};

#[derive(Clone)]
pub(crate) struct TemplateUiStateClassifier {
    pub(super) templates: ResolvedUiTemplateArgs,
    pub(super) screen: config::ScreenConfig,
    pub(super) game_entry: Option<GameEntryDetection>,
    pub(super) game_fallback: GameFallbackState,
    pub(super) panels: Option<PanelRegions>,
}

impl TemplateUiStateClassifier {
    pub(crate) fn new(templates: ResolvedUiTemplateArgs, screen: config::ScreenConfig) -> Self {
        Self {
            templates,
            screen,
            game_entry: None,
            game_fallback: GameFallbackState::new(2),
            panels: None,
        }
    }

    /// 注入面板判据区域（好友面板 / 添加好友页 / 更多设置 / 屏蔽聊天 / 确认弹窗 / invite 三步）。
    pub(crate) fn with_panel_regions(mut self, panels: PanelRegions) -> Self {
        self.panels = Some(panels);
        self
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
}

impl TemplateUiStateClassifier {
    /// 逐帧界面分类。
    ///
    /// **关于 `target: "timing"` 的级别**：这些耗时日志是逐帧执行的（值守循环每轮都分类），
    /// 按 logging-style.md 二「每轮都会执行的上限 trace 或 debug，绝不允许 info」
    /// 定为 `debug`。级别调整不影响脚本消费——`timing` 是独立日志流，
    /// 字段格式完全未变（§六要求保持），脚本按 `total=`/`marker=` 等键解析照旧。
    /// 需要采集性能数据时把 `logging.level` 设成 `debug` 即可，此时行数与以前一致。
    pub(crate) fn classify_at(
        &mut self,
        image: &DynamicImage,
        now: Instant,
    ) -> Result<UiStateClassification> {
        // 与截图入口共用同一套「缩放到画布」实现：尺寸不匹配时缩放，匹配时直接借用原图。
        let canvas = Canvas {
            width: self.screen.expected_width,
            height: self.screen.expected_height,
            resize: true,
        };
        let normalized = resize_to_canvas(image, &canvas, None);
        let image = normalized.as_ref().unwrap_or(image);
        let (state, evidence) =
            match detect_ui_state_with_evidence(image, &self.templates, &self.screen) {
                Ok(result) => result,
                Err(error) => {
                    self.game_fallback.reset();
                    return Err(error);
                }
            };
        // **层级短路**：一级/二级是正常值守的两种界面，基础判据一旦确认，
        // 就直接返回，不跑世界探针、F2 按钮、资料卡四分类等逐帧重活。
        // 只有基础判据不是一级/二级时，才进入下面的覆盖层/面板细化链。
        let base_kind = state.kind;
        // 一级：正常返回；只有右上圆形关闭按钮存在时才做一次廉价覆盖页纠正
        // （千星覆盖页的蓝色图标会被聊天 marker 误数成 8 个蓝点）。
        if base_kind == UiStateKind::Primary {
            if self.panels.is_some() && is_qianxing_overlay_close(image) {
                return Ok(self.profile_page_by_buttons(image).unwrap_or_else(|| {
                    UiStateClassification::with_evidence(
                        UiStateKind::Panel,
                        "panel:qianxing_page",
                        evidence.clone(),
                    )
                }));
            }
            return Ok(UiStateClassification::with_evidence(
                base_kind,
                state.to_string(),
                evidence,
            ));
        }
        // 二级：正常返回；头像弹窗叠在二级之上时会先看到 secondary，用一个
        // 窄区域模板兜住这类面板，避免为它每帧跑整条面板细化链。
        if base_kind == UiStateKind::Secondary {
            if let Some(profile) = self.avatar_popup_probe(image) {
                return Ok(profile);
            }
            return Ok(UiStateClassification::with_evidence(
                base_kind,
                state.to_string(),
                evidence,
            ));
        }
        if matches!(
            base_kind,
            UiStateKind::Loading
                | UiStateKind::GameGate
                | UiStateKind::MainMenu
                | UiStateKind::Overworld
        ) {
            return Ok(UiStateClassification::with_evidence(
                base_kind,
                state.to_string(),
                evidence,
            ));
        }
        // 世界判据是**正交于界面状态**的一维：它回答"哪个世界"，不参与
        // primary/secondary/... 的判定，只是随证据一起报出去，供查询与断言。
        // 计数落在两侧空档之间时它的结论是 unknown，不会倒向任何一侧。
        let evidence = evidence.with_world_probe(world_probe_evidence(image));
        // F2 千星大厅页（大厅内按 F2 打开的多人面板）首选**两个底部按钮**锚点：
        // 左下「大厅列表」+ 右下「邀请玩家」。两者都命中才判 F2，因此不靠顶部
        // 「返回提瓦特」或通用 chrome；也不会落成覆盖层·大厅卡 / 主菜单。
        // 这个判据放在所有状态分支之前：该页偶尔会被聊天 marker 误抢成 Primary，
        // 精确按钮对命中时应统一纠正为 Secondary·多人面板。
        // 走到这里只会是 Panel/Unknown：F2 多人面板只可能出现在这两类语境。
        if let Some(classification) = self.f2_hall_page_by_buttons(image) {
            self.game_fallback.reset();
            return Ok(classification);
        }
        // 千星大厅的**多人面板**（在大厅里按 F2 打开）第 60 节之前一直落在 `Unknown`，
        // 于是连续两帧之后会触发回退探针。这里在回退**之前**把它认出来，
        // 归到二级面板。放在这个位置是有讲究的：
        //   - 它在 `Unknown` 之后才判，所以不会抢走 primary/chat/main_menu 等已有状态；
        //   - 它在回退之前判，所以不会再被回退探针误认成大世界。
        if state.kind == UiStateKind::Unknown {
            // **首选：右下角锚点模板匹配**（第 74 节）。整屏暗青比例只是后备：
            // 它判的是"整屏偏暗青"，对亮底地图变体、对其它深色页都不够特异；
            // 而右下角「千星奇域·大厅」+ 庙宇图标是这一页独有的措辞 + 图形。
            // 放在 Unknown 分支最前面：它比 F2 面板（底栏描述子）、提瓦特世界判据都更具体。
            // 证据沿用检测链（保留世界判据/标记探针），只追加本锚点探针并换 final_rule。
            if let Some(matched) =
                qianxing_map_anchor_measure(image, &self.templates.qianxing_map_label_template)?
                && matched.score >= QIANXING_MAP_ANCHOR_SCORE_MIN
            {
                self.game_fallback.reset();
                let anchor_rect =
                    qianxing_map_anchor_rect(image).unwrap_or(QIANXING_MAP_ANCHOR_SEARCH);
                let hit = TemplateHit {
                    kind: "template".to_string(),
                    x: matched.x,
                    y: matched.y,
                    width: QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH,
                    height: QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT,
                    score: matched.score,
                    quality_ok: true,
                };
                let evidence = evidence
                    .clone()
                    .with_template_probe(template_probe(
                        &self.templates.qianxing_map_label_template,
                        anchor_rect,
                        QIANXING_MAP_ANCHOR_SCORE_MIN,
                        Some(&hit),
                    ))
                    .with_final_rule("qianxing_map_label_template");
                return Ok(UiStateClassification::with_evidence(
                    UiStateKind::Secondary,
                    "secondary:qianxing_map",
                    evidence,
                ));
            }
            // 顺序有讲究：先 F2 多人面板（更具体），再千星地图。
            // 两者的特征完全不同（前者是底栏描述子、后者是全屏色调），实测不会互相抢，
            // 但把更具体的放前面是更稳的写法。
            if is_qianxing_player_panel(image) {
                // 冻结描述子兜底：按钮锚点因缩放/位移/局部遮挡没同时命中时，
                // 仍用已标定的底栏左半结构把 F2 认出来。它只是**兜底**，标签与
                // 按钮判据一致（Secondary·多人面板），不再归到覆盖层·大厅卡。
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    UiStateKind::Secondary,
                    "secondary:qianxing_panel",
                    UiStateEvidence::new(
                        evidence.template_probes().to_vec(),
                        evidence.marker_probe().cloned(),
                        "secondary:qianxing_panel_ref",
                    )
                    .with_world_probe(world_probe_evidence(image)),
                ));
            }
            // **提瓦特大世界**：第 63 节的覆盖率清点发现，150 帧整屏证据里有 **30 帧**
            // 是货真价实的提瓦特大世界（世界判据判为 `teyvat`），却全部落在 `unknown`。
            // 原因是 app 的 `overworld` 判定依赖右上角祈愿顶栏这一个模板，
            // 那段录制里没有它，于是整段失判（第 59 节已量化过一次）。
            //
            // 这里直接用第 57/58 节已验证的**世界判据**补：HUD 在场**且**判为提瓦特，
            // 那就是提瓦特大世界。它比"祈愿顶栏"正交得多——菜单/面板的 HUD 不在场，
            // 会被 `world_hud_scale` 直接挡掉（第 59 节实测：千星面板 `not_applicable`）。
            if world_is_teyvat_in(image, self.screen.hall_bar_rect.into()) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    UiStateKind::Overworld,
                    "overworld:world_criterion",
                    evidence,
                ));
            }
            if is_qianxing_map(image) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    UiStateKind::Secondary,
                    "secondary:qianxing_map",
                    evidence,
                ));
            }
            // 亮底千星地图变体：必须在提瓦特世界判据之后（提瓦特地图/传送帧归 Overworld）。
            if is_bright_qianxing_map(image) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    UiStateKind::Secondary,
                    "secondary:qianxing_map",
                    evidence,
                ));
            }
        }
        let mut base =
            UiStateClassification::with_evidence(state.kind, state.to_string(), evidence);
        // 千星覆盖页偶尔会被聊天 marker 误抢成 Primary（例：wonderland-collection 的蓝色圆形图标
        // 被当成 8 个蓝 marker）。覆盖页有"右上白色关闭按钮"、大厅没有（实测 ≤0.13），
        // 因此 Primary 只要带这个关闭按钮就改判覆盖层。
        if self.panels.is_some()
            && base.kind() == UiStateKind::Primary
            && is_qianxing_overlay_close(image)
        {
            // 个人资料页同样带这个白色关闭按钮，蓝色星点背景还可能被当成聊天 marker。
            // 右下角「心愿单」+「衣橱」双命中时优先判个人资料页；否则维持覆盖层·其他。
            base = self.profile_page_by_buttons(image).unwrap_or_else(|| {
                UiStateClassification::with_evidence(
                    UiStateKind::Panel,
                    "panel:qianxing_page",
                    base.evidence().clone(),
                )
            });
        }
        if base.kind() != UiStateKind::Unknown {
            self.game_fallback.reset();
            // 面板叠在二级聊天之上时（例如头像弹窗），基础链只看到 secondary；
            // 这里做一次面板细化，把它认出来。
            // **只在 Secondary 上细化**：MainMenu/Loading/Primary/Overworld 已有强判据，
            // 面板模板不能把它们覆盖掉（否则主菜单千星会被大厅卡模板误判成 Panel）。
            if base.kind() == UiStateKind::Secondary {
                // F2 页面的基础链偶尔会先判成二级聊天（聊天 marker 在），
                // 描述子命中时纠正为多人面板；否则再做普通面板细化。
                if is_qianxing_player_panel(image) {
                    return Ok(UiStateClassification::with_evidence(
                        UiStateKind::Secondary,
                        "secondary:qianxing_panel",
                        UiStateEvidence::new(
                            Vec::new(),
                            base.evidence().marker_probe().cloned(),
                            "secondary:qianxing_panel_ref",
                        )
                        .with_world_probe(world_probe_evidence(image)),
                    ));
                }
                return Ok(self.refine_panel(image).unwrap_or(base));
            }
            return Ok(base);
        }
        let result = self.game_fallback.observe_unknown(now, base, || {
            probe_game_state(
                image,
                &self.templates,
                &self.screen,
                self.game_entry.as_ref(),
            )
        })?;
        if let Some(refined) = self.refine_panel(image) {
            return Ok(refined);
        }
        // refine_panel 的专属锚点没命中时，再看"右上白色关闭按钮"这个千星全屏覆盖页结构。
        // 放在 refine 之后是为了保留已有专属 label（大厅卡/好友面板…），
        // 也让 friend_delivery 依赖的"大厅信息页(大厅卡)"保持原有行为。
        // 覆盖页判据只在面板接线已注入时启用（生产/门禁都会注入）。未注入面板的
        // 轻量测试运行时保持原有 unknown 行为，避免改变旧调用点的语义。
        if self.panels.is_some() && is_qianxing_overlay_close(image) {
            // 大厅卡/主菜单这类页面右上角是「返回提瓦特」胶囊 + 关闭按钮；
            // 千星全屏覆盖页只有圆形关闭按钮。存在返回提瓦特时保留原有大厅卡语义。
            let threshold = self.templates.chat_templates.marker_threshold;
            let strip = Rect {
                x: image.width().saturating_sub(300) as i32,
                y: 0,
                width: 300,
                height: 90,
            };
            let back_to_teyvat =
                best_template_candidate(image, Some(strip), &self.templates.main_menu_template)
                    .ok()
                    .flatten()
                    .is_some_and(|hit| hit.quality_ok && hit.score >= threshold);
            let (label, rule) = if back_to_teyvat {
                ("panel:hall_card", "panel:hall_card")
            } else {
                ("panel:qianxing_page", "panel:qianxing_page")
            };
            return Ok(UiStateClassification::with_evidence(
                UiStateKind::Panel,
                label,
                UiStateEvidence::new(Vec::new(), None, rule),
            ));
        }
        Ok(result)
    }
}

impl UiStateClassifier for TemplateUiStateClassifier {
    fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification> {
        self.classify_at(image, Instant::now())
    }

    fn classify_cached(
        &mut self,
        image: &DynamicImage,
        cached: &UiStateClassification,
    ) -> Result<UiStateClassification> {
        // A confirmed fallback also needs periodic revalidation while pixels stay still.
        if self.game_fallback.unknown_since.is_some() || cached.kind() == UiStateKind::Unknown {
            self.classify(image)
        } else {
            Ok(cached.clone())
        }
    }
}
