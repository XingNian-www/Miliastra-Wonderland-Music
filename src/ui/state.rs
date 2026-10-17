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
    UiWorldProbeEvidence,
};
use crate::ui::geometry::{Point, Rect, crop_canvas};
#[cfg(test)]
use crate::ui::template::best_template_hit;
use crate::ui::template::{
    TemplateHit, best_template_candidate, best_template_candidate_downscaled,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct UiTemplateArgs {
    friend_template: Option<PathBuf>,
    secondary_back_template: Option<PathBuf>,
    main_menu_template: Option<PathBuf>,
    hall_bar_template: Option<PathBuf>,
    chat_templates: TemplateArgs,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedUiTemplateArgs {
    pub(crate) friend_template: PathBuf,
    pub(crate) secondary_back_template: PathBuf,
    /// 主菜单「返回提瓦特」按钮模板。它比 secondary_back 优先判定：
    /// 奇域主菜单上也有那个圆形返回按钮，若不先挡住，菜单会被误判成二级聊天。
    pub(crate) main_menu_template: PathBuf,
    /// 千星大厅底栏 👥 O 图标组模板。
    /// 聊天标记判据必须同时命中它——提瓦特的世界聊天也有蓝/黄/粉气泡（第 46 节缺陷）。
    #[allow(dead_code)]
    pub(crate) hall_bar_template: PathBuf,
    pub(crate) world_wish_template: PathBuf,
    pub(crate) chat_templates: ResolvedTemplateArgs,
    /// 面板判据模板（好友面板 / 更多设置菜单 / 添加好友页 / 确认弹窗 /
    /// 千星档案卡 · 大厅卡 · 头像弹窗）。取自 `moderation` / `invite` 配置，
    /// 已在真实帧上逐个验证命中（见 dist/ui-flow/README.md）。
    ///
    /// 注意这里**没有** `friend_more_settings`（⋯）：它是共用 chrome，不是专属锚点，
    /// 更多设置菜单改由 `friend_block_chat_template`（菜单内才有的 `屏蔽聊天`）判定。
    pub(crate) friend_panel_template: PathBuf,
    pub(crate) friend_search_panel_template: PathBuf,
    pub(crate) friend_block_chat_template: PathBuf,
    pub(crate) friend_confirm_template: PathBuf,
    pub(crate) invite_view_star_template: PathBuf,
    pub(crate) invite_goto_hall_template: PathBuf,
    pub(crate) invite_enter_hall_template: PathBuf,
    /// 提瓦特派蒙菜单（ESC 系统菜单）的专属锚点。它和二级聊天共用同一个圆形返回
    /// 按钮，只靠返回按钮会把菜单误判成二级；这个「商城」图标只在该菜单出现。
    pub(crate) paimon_menu_panel_template: PathBuf,
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
            main_menu_template: self
                .main_menu_template
                .clone()
                .unwrap_or_else(|| templates.main_menu.clone()),
            hall_bar_template: self
                .hall_bar_template
                .clone()
                .unwrap_or_else(|| templates.hall_bar.clone()),
            world_wish_template: templates.world_wish.clone(),
            chat_templates: self.chat_templates.resolve(templates, ocr),
            friend_panel_template: templates.friend_panel.clone(),
            friend_search_panel_template: templates.friend_search_panel.clone(),
            friend_block_chat_template: templates.friend_block_chat.clone(),
            friend_confirm_template: templates.friend_confirm.clone(),
            invite_view_star_template: templates.invite_view_star.clone(),
            invite_goto_hall_template: templates.invite_goto_hall.clone(),
            invite_enter_hall_template: templates.invite_enter_hall.clone(),
            paimon_menu_panel_template: templates.paimon_menu_panel.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UiStateKind {
    Primary,
    Secondary,
    /// 奇域主菜单：独立状态，不能并进 Unknown，否则会触发游戏态回退（§44.10）。
    MainMenu,
    /// 加载界面：真实界面（两种外观），不能并进 Unknown（§55）。
    Loading,
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

    /// 千星大厅：靠**底栏结构**判出来且当场没有聊天标记。
    ///
    /// 大厅是最常见的状态，气泡只是它的可选内容：聊天框关着、或气泡模板没命中时，
    /// 旧逻辑会落到 unknown。这个构造器只服务那条新路径，标签用 `primary:hall`
    /// 与「靠气泡判出来」的 `primary:marker …` 区分开。
    fn primary_hall() -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "hall",
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

    /// 千星奇域主菜单：独立状态。
    fn main_menu() -> Self {
        Self {
            state: UiStateKind::MainMenu,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "main_menu",
        }
    }

    /// 提瓦特派蒙菜单（ESC 系统菜单）。它属于主菜单一族（`MainMenu`），
    /// 但用**专属锚点**判定，因此证据里的规则名与奇域主菜单不同。
    fn paimon_menu() -> Self {
        Self {
            state: UiStateKind::MainMenu,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "paimon_menu",
        }
    }

    /// 加载界面。两种外观（黑底徽记 / 白底元素）都归到这一个状态，
    /// 具体是哪一种由证据里的 finalRule（loading_dark_screen / loading_white_screen）区分。
    fn loading() -> Self {
        Self {
            state: UiStateKind::Loading,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "loading",
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
            UiStateKind::MainMenu => RuntimeUiStateKind::MainMenu,
            UiStateKind::Loading => RuntimeUiStateKind::Loading,
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
            UiStateKind::Primary if self.source == "hall" => {
                write!(formatter, "primary:hall")
            }
            UiStateKind::Primary => write!(
                formatter,
                "primary:marker blue={} yellow={} pink={}",
                self.blue_count, self.yellow_count, self.pink_count
            ),
            UiStateKind::Secondary => write!(formatter, "secondary:chat"),
            UiStateKind::MainMenu => write!(formatter, "main_menu"),
            UiStateKind::Loading => write!(formatter, "loading"),
            UiStateKind::Unknown => write!(formatter, "unknown"),
        }
    }
}

/// 面板判据用到的区域（来自 `moderation` / `invite` 配置）。
///
/// 由 [`TemplateUiStateClassifier::with_panel_regions`] 注入；**未注入时面板判据不跑**，
/// 行为与从前完全一致（因此现有 `new()` 调用点与测试零改动）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PanelRegions {
    pub(crate) friend_panel: Rect,
    pub(crate) friend_search_panel: Rect,
    pub(crate) friend_block_chat: Rect,
    pub(crate) friend_confirm: Rect,
    pub(crate) invite_view_star: Rect,
    pub(crate) invite_goto_hall: Rect,
    pub(crate) invite_enter_hall: Rect,
}

#[derive(Clone)]
pub(crate) struct TemplateUiStateClassifier {
    templates: ResolvedUiTemplateArgs,
    screen: config::ScreenConfig,
    game_entry: Option<GameEntryDetection>,
    game_fallback: GameFallbackState,
    panels: Option<PanelRegions>,
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

    pub(crate) fn classify_at(
        &mut self,
        image: &DynamicImage,
        now: Instant,
    ) -> Result<UiStateClassification> {
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
        // 世界判据是**正交于界面状态**的一维：它回答"哪个世界"，不参与
        // primary/secondary/... 的判定，只是随证据一起报出去，供查询与断言。
        // 计数落在两侧空档之间时它的结论是 unknown，不会倒向任何一侧。
        let evidence = evidence.with_world_probe(world_probe_evidence(image));
        // 千星大厅的**多人面板**（在大厅里按 F2 打开）第 60 节之前一直落在 `Unknown`，
        // 于是连续两帧之后会触发回退探针。这里在回退**之前**把它认出来，
        // 归到二级面板。放在这个位置是有讲究的：
        //   - 它在 `Unknown` 之后才判，所以不会抢走 primary/chat/main_menu 等已有状态；
        //   - 它在回退之前判，所以不会再被回退探针误认成大世界。
        if state.runtime_kind() == RuntimeUiStateKind::Unknown {
            // 顺序有讲究：先 F2 多人面板（更具体），再千星地图。
            // 两者的特征完全不同（前者是底栏描述子、后者是全屏色调），实测不会互相抢，
            // 但把更具体的放前面是更稳的写法。
            if is_qianxing_player_panel(image) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    RuntimeUiStateKind::Secondary,
                    "secondary:qianxing_panel",
                    evidence,
                ));
            }
            if is_qianxing_map(image) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    RuntimeUiStateKind::Secondary,
                    "secondary:qianxing_map",
                    evidence,
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
            if world_is_teyvat(image) {
                self.game_fallback.reset();
                return Ok(UiStateClassification::with_evidence(
                    RuntimeUiStateKind::Overworld,
                    "overworld:world_criterion",
                    evidence,
                ));
            }
        }
        let base =
            UiStateClassification::with_evidence(state.runtime_kind(), state.to_string(), evidence);
        if base.kind() != RuntimeUiStateKind::Unknown {
            self.game_fallback.reset();
            // 面板叠在二级聊天之上时（例如头像弹窗），基础链只看到 secondary；
            // 这里做一次面板细化，把它认出来。
            return Ok(self.refine_panel(image).unwrap_or(base));
        }
        let result = self.game_fallback.observe_unknown(now, base, || {
            probe_game_state(
                image,
                &self.templates,
                &self.screen,
                self.game_entry.as_ref(),
            )
        })?;
        Ok(self.refine_panel(image).unwrap_or(result))
    }

    /// 面板细化：好友面板 / 添加好友页 / 更多设置 / 确认弹窗 /
    /// 千星档案卡 · 大厅卡 · 头像弹窗——这些界面的基础链判不出来（落 `Unknown`），
    /// 或叠在二级聊天之上（`Secondary`），用**已配置且实测命中**的模板逐个确认。
    ///
    /// **锚点必须专属该面板**：`friend-more-settings`（⋯）是**共用 chrome**，
    /// 在档案卡与⋯菜单上落在同一坐标（实测档案卡 0.992 甚至高于菜单 0.959），
    /// 因此它**不是**任何面板的判据；更多设置菜单改用**只有菜单展开时才存在**的
    /// `屏蔽聊天` 条目（实测 0.989，其它帧最高 0.854）。
    ///
    /// **顺序也参与判别**：大厅卡与档案卡都含一条深色通栏按钮，`进入该大厅`
    /// （大厅卡 0.994）排在 `前往其大厅`（档案卡 0.962）之前，避免大厅卡被
    /// `前往其大厅` 的暗按钮误命中（实测大厅卡 0.905）。
    ///
    /// **成本护栏**：便宜且特异的排前面、**命中即停**；大区域靠调用方收窄区域 +
    /// 帧级短路摊薄（成本实测见 `dist/ui-flow/README.md`）。
    fn refine_panel(&self, image: &DynamicImage) -> Option<UiStateClassification> {
        let panels = self.panels.as_ref()?;
        let threshold = self.templates.chat_templates.marker_threshold;
        // 最后一个数字是**降采样倍数**：代价 ≈ 候选位置数 × 模板像素数，两个维度都除以
        // `scale`，总代价降到约 `1/scale⁴`（4 即约 256×）。只给"大模板"降采样；小图标
        // （friend-panel 49×35 / search 96×33）本身几乎免费，降采样反而把信息抹掉。
        let cases: [(&str, &PathBuf, Rect, u32); 7] = [
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
                "panel:add_friend",
                &self.templates.friend_search_panel_template,
                panels.friend_search_panel,
                1,
            ),
            (
                "panel:confirm_dialog",
                &self.templates.friend_confirm_template,
                panels.friend_confirm,
                4,
            ),
            (
                "panel:hall_card",
                &self.templates.invite_enter_hall_template,
                panels.invite_enter_hall,
                4,
            ),
            (
                "panel:archive_card",
                &self.templates.invite_goto_hall_template,
                panels.invite_goto_hall,
                4,
            ),
            (
                "panel:avatar_popup",
                &self.templates.invite_view_star_template,
                panels.invite_view_star,
                4,
            ),
        ];
        for (label, path, region, scale) in cases {
            let candidate =
                match best_template_candidate_downscaled(image, Some(region), path, scale) {
                    Ok(candidate) => candidate,
                    Err(_) => continue,
                };
            if candidate_matches(&candidate, threshold) {
                return Some(UiStateClassification::with_evidence(
                    RuntimeUiStateKind::Panel,
                    label,
                    UiStateEvidence::new(
                        vec![template_probe(path, region, threshold, candidate.as_ref())],
                        None,
                        label,
                    ),
                ));
            }
        }
        None
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
        if self.game_fallback.unknown_since.is_some()
            || cached.kind() == RuntimeUiStateKind::Unknown
        {
            self.classify(image)
        } else {
            Ok(cached.clone())
        }
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
    let wish_matched = candidate_matches(&candidate, templates.chat_templates.marker_threshold);
    // 祈愿顶栏命中**还不够**：千星大厅的多人面板也会命中它（第 59 节实测）。
    // 再叠一道正交的世界判据——必须「有 HUD 且判为提瓦特」才算大世界。
    let world_is_teyvat = wish_matched && world_is_teyvat(image);
    let rule = match (wish_matched, world_is_teyvat) {
        (true, true) => "world_wish_template",
        // 明确区分「顶栏命中但世界判据否掉」：这条规则名让误判在日志里一眼可见，
        // 不会再伪装成正常的大世界判定。
        (true, false) => "world_wish_without_teyvat_hud",
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
        log::info!(target: "timing",
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
    //   ① 块数 7–14（一排键；拼图 17 块被上限挡掉）
    //   ② 每块高度落在 22–36 px（实测 26–28；菜单整块白底是 60–64）
    //   ③ 所有块在**同一条水平线**上：顶部 y 极差 ≤4 px
    // 白色判据用严格的 225（不是纯 255：JPEG/缩放帧的白色只有 240–254）。
    let hall_region: Rect = screen.hall_bar_rect.into();
    let (hall_tiles, hall_min_height_px, hall_max_height_px, hall_top_spread_px) =
        hall_bar_signature(image, hall_region);
    let hall_structure = (HALL_BAR_MIN_TILES..=HALL_BAR_MAX_TILES).contains(&hall_tiles)
        && hall_min_height_px >= HALL_BAR_MIN_TILE_HEIGHT_PX
        && hall_max_height_px <= HALL_BAR_MAX_TILE_HEIGHT_PX
        && hall_top_spread_px <= HALL_BAR_MAX_TILE_TOP_SPREAD_PX;
    if hall_structure {
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms marker={}ms state=primary rule=primary_hall_bar tiles={} h_min={} h_max={} top_spread={}",
            elapsed_ms(started),
            marker_ms,
            hall_tiles,
            hall_min_height_px,
            hall_max_height_px,
            hall_top_spread_px
        );
        return Ok((
            UiState::primary_hall(),
            UiStateEvidence::new(template_probes, Some(marker_probe), "primary_hall_bar"),
        ));
    }

    // ── 3) 加载界面是**真实界面**，不是 unknown，所以在剩下的分支里最先判它。
    // 它整屏只有极端色调，任何游戏界面的暗/白占比都远达不到（见上面两条阈值的实测来源），
    // 因此放在最前面既便宜又不会抢掉其它状态。具体外观由证据里的规则名区分。
    if let Some(rule) = loading_rule(image) {
        let total_ms = elapsed_ms(started);
        log::info!(target: "timing",
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
            log::info!(target: "timing",
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
        // 提瓦特派蒙菜单（ESC 系统菜单）也挂着**同一个**圆形返回按钮：右侧世界照常，
        // 左侧换成功能宫格。用「商城」袋状图标作专属锚点，把它从二级聊天里摘出来。
        // 与主菜单探针同层、同样只在返回按钮命中后才跑，不给常见路径加开销。
        let paimon_region: Rect = screen.paimon_menu_rect.into();
        let paimon_candidate = best_template_candidate(
            image,
            Some(paimon_region),
            &templates.paimon_menu_panel_template,
        )?;
        let paimon_visible = candidate_matches(&paimon_candidate, threshold);
        template_probes.push(template_probe(
            &templates.paimon_menu_panel_template,
            paimon_region,
            threshold,
            paimon_candidate.as_ref(),
        ));
        if paimon_visible {
            let back_ms = elapsed_ms(back_started);
            log::info!(target: "timing",
                "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=paimon_menu",
                elapsed_ms(started),
                back_ms,
                marker_ms
            );
            return Ok((
                UiState::paimon_menu(),
                UiStateEvidence::new(template_probes, None, "paimon_menu_panel_template"),
            ));
        }
        log::info!(target: "timing",
            "UI 状态检测耗时: total={}ms back={}ms marker={}ms state=secondary_chat",
            elapsed_ms(started),
            back_ms,
            marker_ms
        );
        return Ok((
            UiState::secondary_chat(),
            UiStateEvidence::new(template_probes, None, "secondary_back_template"),
        ));
    }
    let back_ms = elapsed_ms(back_started);

    log::info!(target: "timing",
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

/// 底栏「白色按键块数」的范围（2px 采样下的 4 连通块，单块至少 [`HALL_BAR_TILE_MIN_AREA`] 个采样格）。
///
/// 来源（1133 帧整屏实测，2026-10-17）：大厅 **7–12 块**；提瓦特 2–4 块；主菜单 3–10 块；登录/加载 0 块。
/// 这是**结构**量而不是模板分数，对位移/缩放不敏感；改采样步长或 `screen.hall_bar_rect` 必须重测。
const HALL_BAR_MIN_TILES: usize = 7;

/// 块数上限：只为挡住「接触表 / 拼图」这类非整屏诊断图（实测 17 块）；真实大厅最多 12 块。
const HALL_BAR_MAX_TILES: usize = 14;

/// 底栏白键的**白色下限**（每通道 min(R,G,B)）。
///
/// **不用纯 255**：实测整屏语料里纯白只覆盖 **89/182** 大厅帧——很多帧是 JPEG/回放、
/// 或经过 DPI 缩放重采样，白色被压到 240–254（255→89、250→116、245→131、240→141）。
/// 225 能让 **224/225** 大厅帧命中，同时比旧的 200 严格得多（200 会把背景亮块也算进来）。
const HALL_BAR_WHITE_MIN: u8 = 225;

/// 单块白键的**真实像素高度**允许区间。
///
/// 大厅底栏键实测 **26–28 px**（1080p 语料；用户实机另有 25 / 32 px 两档），
/// 取 **[22, 36]** 覆盖这两档、两侧留 2–4 px 余量。
/// 主菜单 / 设置宫格是整块白底 ⇒ 60–64 px，直接排除。
const HALL_BAR_MIN_TILE_HEIGHT_PX: usize = 22;
const HALL_BAR_MAX_TILE_HEIGHT_PX: usize = 36;

/// 所有键必须落在**同一条水平线**上：顶部 y 极差 ≤4 真实像素（"误差留一点点"）。
/// 实测大厅帧里 7–8 个键的顶部完全一致（极差 0）；设置宫格则是 985 与 1045 两行（极差 60）。
const HALL_BAR_MAX_TILE_TOP_SPREAD_PX: usize = 4;

/// 计入「一块按键」的最小面积（单位：2px 采样格）。低于它的白点按噪点丢弃。
const HALL_BAR_TILE_MIN_AREA: usize = 40;

/// 统计指定区域里"近白"像素的占比（每 2 像素取样一次，与定阈值时的量法一致）。
///
/// 千星大厅底栏有多个白色圆角按键（Enter / O / Alt+N / N），提瓦特底栏只有一个 Enter，
/// 所以这个占比能把"提瓦特的世界聊天气泡"和"千星聊天标记"区分开。
/// 与模板匹配不同，它**对位移和缩放都不敏感**——第 47 节就是栽在 1:1 灰度 SAD 上的。
/// 整屏色调统计的取样步长：每 20 像素取一点（1920x1080 下约 5.2k 个样本）。
/// **必须与定阈值时的量法一致**，改步长要重新量阈值。
const SCREEN_TONE_SAMPLE_STEP: u32 = 20;
/// 记一个"暗点"的灰度上限。
const SCREEN_TONE_DARK_MAX: f32 = 40.0;
/// 记一个"近白点"的通道下限：min(R,G,B) 不小于它才算近白。
const SCREEN_TONE_WHITE_MIN: u8 = 200;

/// 黑底徽记型加载界面的**暗像素占比下限**。
///
/// 来源（实测区间中点，不是凑的）：加载帧 **98.46~98.53%**，
/// 而语料里**任何真实画面**最高只有 **26.00%**（活动面板），
/// 两者之间没有任何一帧 ⇒ 取 (26.00 + 98.46) / 2 = **62.23%**。
const LOADING_DARK_RATIO_MIN: f32 = 0.6223;

/// 白底元素型加载界面的**近白占比下限**。
///
/// 来源：加载帧 **99.77~100%**，任何真实画面最高 **43.31%**（登录界面）
/// ⇒ 取 (43.31 + 99.77) / 2 = **71.54%**。
const LOADING_WHITE_RATIO_MIN: f32 = 0.7154;

/// 世界判据（提瓦特 vs 千星）统计的**青色条纹带**区域。
///
/// 千星在大厅左上角派蒙头像周围多了一整套装饰（半透明蓝色圆盘、青色条纹带、
/// 黄色四角星、金色闪电条），提瓦特没有。其中西南侧这条青色条纹带最干净。
/// 坐标是**相对整帧左上角的绝对坐标**，在第 57 节的 1920x1080 帧上量出来的。
///
/// 与第 48 节的底栏近白占比同理，这是**区域上的标量统计**而不是 1:1 模板匹配，
/// 所以对位移与轻度缩放都不敏感（实测位移 ±6 像素时计数完全不变）。
const WORLD_CYAN_BAND_RECT: Rect = Rect::new(76, 60, 18, 25);

/// 记一个像素属于青色条纹带的**相对色差下限**：R 必须比 G、B 都低这么多。
///
/// 用相对色差而不是绝对亮度阈值（例如 G>150 且 B>150 且 R<130），是为了
/// **对整屏亮度平移与增益都不敏感**。实测对比（提瓦特最高 / 千星最低之间的空档）：
/// - 绝对规则：亮度 ±31 时空档只剩 3~5，增益 0.78 时两侧区间**交错**（空档 −34）
/// - 相对规则：亮度 ±31 时空档 166（两个方向完全相同），增益 0.78/1.325 时空档 188/124
const WORLD_CYAN_CHANNEL_DIFF: i32 = 30;

/// 判为**千星**的青色条纹带像素数**下限**。
///
/// 来源（实测空档内的**三等分点**，不是凑的）：语料上提瓦特侧最高 **26**
/// （雪山帧；冷启动与沙漠帧都是 0），千星侧最低 **154**（大厅帧）⇒ 空档 26~154。
/// 空档长度 128，三等分点 26 + 128/3 ≈ 68.7 与 26 + 256/3 ≈ 111.3，
/// 取整成 68 / 111。两侧余量对称（提瓦特侧 42，千星侧 43）。
const WORLD_CYAN_QIANXING_MIN: usize = 111;

/// 判为**提瓦特**的青色条纹带像素数**上限**。
/// 高于它、又不到 `WORLD_CYAN_QIANXING_MIN` 的，一律返回 Unknown ——
/// 与 `unknown` 的纪律一致：判不出来就不猜。
const WORLD_CYAN_TEYVAT_MAX: usize = 68;

/// 世界 HUD 参考块：4 张标注提瓦特 + 6 张标注千星帧在 (40,37)-(71,76) 这 1280 像素上
/// 逐通道归一化后的均值。生成脚本 \`dist/worlddata/mkref.py\`，格式：
/// magic("MWW1") + version(u32) + x(i32) + y(i32) + width(u32) + height(u32)
/// + 提瓦特参考(i16×N) + 千星参考(i16×N)，N = width*height*3，归一化值 ×1000。
///
/// 内嵌而不是走配置：它是一份**冻结的标定数据**，改了要连同第 57 节的实测一起改，
/// 不适合让用户随手配置。
const WORLD_HUD_REF: &[u8] = include_bytes!("../../assets/world-hud-ref.bin");
const WORLD_HUD_REF_MAGIC: &[u8] = b"MWW1";
const WORLD_HUD_REF_VERSION: u32 = 1;
const WORLD_HUD_REF_SCALE: f32 = 1000.0;
const WORLD_HUD_REF_HEADER: usize = 24;
/// 比较时允许的像素平移范围。第 57 节实测：不允许平移时，整片精确模式匹配
/// **±1 像素就翻判断**；允许 ±2 之后位移 ±1/±2/±3 都不再翻。
const WORLD_HUD_SEARCH: i32 = 2;

/// 比较时允许的**尺度**搜索范围。
///
/// 第 58 节的扰动实测：0.85 / 1.15 缩放后拒判（`classify_at` 把分辨率归一之后，
/// UI 本身还是被放大/缩小了）。这里直接把这些尺度纳入搜索：每个采样格点改成取
/// 对应大小窗口的平均（scale != 1 即降采样），把放大带来的模糊吃进平均值里。
///
/// 范围取 ±15%：与第 58 节扰动实测用的 0.85 / 1.15 对齐。
/// **不放开无边界搜索**：搜索范围越大越容易撞上菜单内容，
/// 范围是否安全由 `hud_presence_is_separated_by_reference_distance` 那条
/// "在场与不在场区间不重叠"的断言守住，而不是靠感觉。
const WORLD_HUD_SCALES: [f32; 7] = [0.85, 0.90, 0.95, 1.00, 1.05, 1.10, 1.15];

/// HUD 在场判定的**距离上限**（两组参考里较近的那一个）。
///
/// 来源（实测的对数中点，不是凑的）：用本节实际落地的算法（±2 搜索、1280 像素、
/// 逐通道平均绝对差）在语料上量——HUD 在场最大 **0.0579**（白天大厅），
/// HUD 不在最小 **1.0000**（纯白加载）⇒ 空档 17.3 倍，
/// 对数中点 √(0.0579×1.0000) ≈ 0.2406，取 **0.24**。
const WORLD_HUD_PRESENT_MAX: f32 = 0.24;

/// 从内嵌参考块里解出区域与两组参考的字节切片。
fn parse_world_hud_ref() -> Option<(Rect, &'static [u8], &'static [u8])> {
    let bytes = WORLD_HUD_REF;
    if bytes.len() < WORLD_HUD_REF_HEADER || &bytes[0..4] != WORLD_HUD_REF_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != WORLD_HUD_REF_VERSION {
        return None;
    }
    let x = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let y = i32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let width = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    let values = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(3)?;
    if bytes.len() != WORLD_HUD_REF_HEADER + values * 2 * 2 {
        return None;
    }
    let teyvat = &bytes[WORLD_HUD_REF_HEADER..WORLD_HUD_REF_HEADER + values * 2];
    let qianxing = &bytes[WORLD_HUD_REF_HEADER + values * 2..];
    Some((Rect::new(x, y, width, height), teyvat, qianxing))
}

/// 读参考块里的第 index 个归一化值。
fn world_hud_ref_value(block: &[u8], index: usize) -> f32 {
    let offset = index * 2;
    let raw = i16::from_le_bytes([block[offset], block[offset + 1]]);
    f32::from(raw) / WORLD_HUD_REF_SCALE
}

/// 画面与两组世界参考的归一化平均绝对差，各取 ±2 像素搜索范围内的最小值。
///
/// 返回 \`(到提瓦特, 到千星)\`；画面太小或参考块损坏时返回 \`None\`（拒判）。
/// 归一化是逐通道减区域均值再除平均绝对偏差 —— 与青色标量用相对色差同理，
/// 为的是对整屏亮度平移与增益都不敏感。
fn world_hud_distances(image: &DynamicImage) -> Option<(f32, f32, f32)> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let (rect, teyvat_ref, qianxing_ref) = parse_world_hud_ref()?;
    let sampler = RgbSampler::new(image);
    let (image_width, image_height) = sampler.dimensions();
    let width = rect.width as i32;
    let height = rect.height as i32;
    let count = (width * height) as f32;

    let pixel = |x: i32, y: i32| -> [f32; 3] {
        let clamped_x = x.clamp(0, image_width as i32 - 1) as u32;
        let clamped_y = y.clamp(0, image_height as i32 - 1) as u32;
        let p = sampler.pixel(clamped_x, clamped_y);
        [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])]
    };

    let mut best_teyvat = f32::MAX;
    let mut best_qianxing = f32::MAX;
    // 达到最佳距离时用的尺度。世界判据的青色标量必须用**同一个尺度**去量，
    // 否则门放行了缩放帧、标量却还在原始坐标上数，会数出接近 0 的青色
    // 从而**自信判成提瓦特** —— 那比拒判更糟。
    let mut best_scale = 1.0_f32;
    for scale in WORLD_HUD_SCALES {
        // 预计算每个采样格点覆盖的整数像素窗口。它与 dx/dy 无关，
        // 放到外层算一次，省掉内层重复的 floor/ceil。
        // scale = 1 时每个窗口就是一个像素，退化成逐像素比较（与首版一致）。
        let mut windows: Vec<[i32; 4]> = Vec::with_capacity((width * height) as usize);
        for j in 0..height {
            for i in 0..width {
                // **原点也要跟着缩放**：UI 整体被放大/缩小时，
                // 区域的原点会同步移动，只缩放步长是错的（第 58 节踩过这个坑：
                // 只缩放步长会让 0.85 / 1.15 的帧无论怎么搜都找不到参考）。
                let left = (rect.x as f32 + i as f32) * scale;
                let top = (rect.y as f32 + j as f32) * scale;
                let first_x = left.floor() as i32;
                let first_y = top.floor() as i32;
                let last_x = ((left + scale).ceil() as i32).max(first_x + 1);
                let last_y = ((top + scale).ceil() as i32).max(first_y + 1);
                windows.push([first_x, first_y, last_x, last_y]);
            }
        }
        // 采样一个格点：取窗口内的平均。scale != 1 时这是**降采样**，
        // 正是为了把放大后的模糊吃进平均值里，而不是逐个像素去比。
        let sample = |window: [i32; 4], dx: i32, dy: i32| -> [f32; 3] {
            let mut acc = [0.0_f32; 3];
            let mut samples = 0.0_f32;
            for y in window[1]..window[3] {
                for x in window[0]..window[2] {
                    let p = pixel(x + dx, y + dy);
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                    samples += 1.0;
                }
            }
            if samples <= 0.0 {
                return pixel(window[0] + dx, window[1] + dy);
            }
            [acc[0] / samples, acc[1] / samples, acc[2] / samples]
        };
        for dy in -WORLD_HUD_SEARCH..=WORLD_HUD_SEARCH {
            for dx in -WORLD_HUD_SEARCH..=WORLD_HUD_SEARCH {
                // 每个格点**只采样一次**，均值/偏差/参考三遍复用同一批样本。
                // 此前三遍各采样一次，等于同一次比较里把最贵的采样做了三遍。
                let sampled = windows
                    .iter()
                    .map(|window| sample(*window, dx, dy))
                    .collect::<Vec<[f32; 3]>>();
                // 第一遍：逐通道均值。
                let mut sums = [0.0_f32; 3];
                for p in &sampled {
                    sums[0] += p[0];
                    sums[1] += p[1];
                    sums[2] += p[2];
                }
                let means = [sums[0] / count, sums[1] / count, sums[2] / count];
                // 第二遍：逐通道平均绝对偏差。
                let mut deviations = [0.0_f32; 3];
                for p in &sampled {
                    deviations[0] += (p[0] - means[0]).abs();
                    deviations[1] += (p[1] - means[1]).abs();
                    deviations[2] += (p[2] - means[2]).abs();
                }
                let deviations = [
                    deviations[0] / count,
                    deviations[1] / count,
                    deviations[2] / count,
                ];
                // 第三遍：与两组参考比。
                let mut total_teyvat = 0.0_f32;
                let mut total_qianxing = 0.0_f32;
                for (index, p) in sampled.iter().enumerate() {
                    for channel in 0..3 {
                        let deviation = if deviations[channel] > 1e-6 {
                            deviations[channel]
                        } else {
                            1.0
                        };
                        let normalized = (p[channel] - means[channel]) / deviation;
                        total_teyvat += (normalized
                            - world_hud_ref_value(teyvat_ref, index * 3 + channel))
                        .abs();
                        total_qianxing += (normalized
                            - world_hud_ref_value(qianxing_ref, index * 3 + channel))
                        .abs();
                    }
                }
                let divisor = count * 3.0;
                let teyvat_distance = total_teyvat / divisor;
                let qianxing_distance = total_qianxing / divisor;
                if teyvat_distance.min(qianxing_distance) < best_teyvat.min(best_qianxing) {
                    best_scale = scale;
                }
                best_teyvat = best_teyvat.min(teyvat_distance);
                best_qianxing = best_qianxing.min(qianxing_distance);
            }
        }
    }
    Some((best_teyvat, best_qianxing, best_scale))
}

/// 左上角 HUD（派蒙头像区）在不在；在则返回它所在的**尺度**。
///
/// 世界判据只在它**在场**时才有意义：全屏菜单的左上角是菜单内容，
/// 青色计数恒为 0，不加这道门就会把菜单误报成提瓦特。
///
/// 返回尺度是为了让青色标量在**同一尺度**上量。否则门放行了缩放帧、
/// 标量还在原始坐标上数，会数出接近 0 的青色从而自信判成提瓦特 —— 那比拒判更糟。
fn world_hud_scale(image: &DynamicImage) -> Option<f32> {
    let (teyvat, qianxing, scale) = world_hud_distances(image)?;
    if teyvat.min(qianxing) <= WORLD_HUD_PRESENT_MAX {
        Some(scale)
    } else {
        None
    }
}

/// 整屏色调判据适用的最小画面尺寸。
/// 本项目所有实机截图都是 1920x1080；而证据目录里还有大量小裁剪图
/// （按钮、色条等，例如 72x42），它们整屏占比没有意义，不参与这条判据。
const FULL_FRAME_MIN_WIDTH: u32 = 1280;
const FULL_FRAME_MIN_HEIGHT: u32 = 720;

/// 稀疏采样探针共用的 RGB 取值器：RGB8/RGBA8 **直接取像素、不做整帧转换**；
/// 其余色彩类型回退到一次性转换。
///
/// 整屏统计类判据（色调、青色带、千星地图色调）只采样几千个点，而原来的
/// `to_rgb8()` 每次都要拷 200 万像素——那是这些判据里最贵的部分，且每帧重复多次。
enum RgbSampler<'a> {
    Borrowed(&'a image::RgbImage),
    Rgba(&'a image::RgbaImage),
    Owned(image::RgbImage),
}

impl RgbSampler<'_> {
    fn new(image: &DynamicImage) -> RgbSampler<'_> {
        match image {
            DynamicImage::ImageRgb8(rgb) => RgbSampler::Borrowed(rgb),
            DynamicImage::ImageRgba8(rgba) => RgbSampler::Rgba(rgba),
            other => RgbSampler::Owned(other.to_rgb8()),
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        match self {
            RgbSampler::Borrowed(rgb) => rgb.dimensions(),
            RgbSampler::Rgba(rgba) => rgba.dimensions(),
            RgbSampler::Owned(rgb) => rgb.dimensions(),
        }
    }

    fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        match self {
            RgbSampler::Borrowed(rgb) => {
                let pixel = rgb.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
            RgbSampler::Rgba(rgba) => {
                let pixel = rgba.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
            RgbSampler::Owned(rgb) => {
                let pixel = rgb.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
        }
    }
}

/// 整屏统计，返回 (暗像素占比, 近白像素占比)。
///
/// 加载界面的两种外观都是**整屏极端色调**（黑底 98%+ 暗 / 白底 99%+ 白），
/// 与任何游戏画面都不重叠，所以这个量与位置、缩放都无关，非常稳。
fn screen_tone(image: &DynamicImage) -> (f32, f32) {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    let mut total = 0_u32;
    let mut dark = 0_u32;
    let mut white = 0_u32;
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let pixel = sampler.pixel(x, y);
            let luma = 0.299 * f32::from(pixel[0])
                + 0.587 * f32::from(pixel[1])
                + 0.114 * f32::from(pixel[2]);
            if luma < SCREEN_TONE_DARK_MAX {
                dark += 1;
            }
            if pixel[0].min(pixel[1]).min(pixel[2]) >= SCREEN_TONE_WHITE_MIN {
                white += 1;
            }
            total += 1;
            x += SCREEN_TONE_SAMPLE_STEP;
        }
        y += SCREEN_TONE_SAMPLE_STEP;
    }
    if total == 0 {
        return (0.0, 0.0);
    }
    (dark as f32 / total as f32, white as f32 / total as f32)
}

/// 判断画面是否为加载界面；是则返回证据里的规则名（区分黑底/白底两种）。
fn loading_rule(image: &DynamicImage) -> Option<&'static str> {
    // 这条判据是为**整屏截图**定义的：加载界面是整屏极端色调。
    // 对裁剪出来的小图（比如按钮、色条）整屏占比没有意义，直接不判，避免误报。
    let (width, height) = (image.width(), image.height());
    if width < FULL_FRAME_MIN_WIDTH || height < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let (dark_ratio, white_ratio) = screen_tone(image);
    if dark_ratio >= LOADING_DARK_RATIO_MIN {
        return Some("loading_dark_screen");
    }
    if white_ratio >= LOADING_WHITE_RATIO_MIN {
        return Some("loading_white_screen");
    }
    None
}

/// 底栏结构：`(白色按键块数, 单块中位面积)`。
///
/// 千星大厅的底栏是一排**小圆角白键**（聊天 / Enter / 👥 / O / 麦克风 / Alt+N / 喇叭 / N）：
/// 块数多（7–12），每块是**同高、同一条水平线的小圆角键**（实测高 26–28 px、顶部 y 极差 0）；
/// 提瓦特只有一个 Enter（2–4 块）；主菜单 / 设置宫格是整块白底（高 60–64 px，或分成两行）。
/// 返回 `(块数, 最小高度 px, 最大高度 px, 顶部 y 极差 px)`。
/// 用几何量而不是模板分数，是为了绕开 1:1 SAD 对缩放不鲁棒的问题（第 47 节教训）；
/// 也不用「近白占比」——那是亮度量，亮背景大厅会把它顶高。
fn hall_bar_signature(image: &DynamicImage, rect: Rect) -> (usize, usize, usize, usize) {
    let Ok(region) = crop_canvas(image, rect) else {
        return (0, 0, 0, 0);
    };
    let rgb = region.to_rgb8();
    let (width, height) = rgb.dimensions();
    let grid_w = (width as usize).div_ceil(2);
    let grid_h = (height as usize).div_ceil(2);
    let mut grid = vec![false; grid_w * grid_h];
    for gy in 0..grid_h {
        for gx in 0..grid_w {
            let pixel = rgb.get_pixel((gx * 2) as u32, (gy * 2) as u32);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= HALL_BAR_WHITE_MIN {
                grid[gy * grid_w + gx] = true;
            }
        }
    }
    let total = grid_w * grid_h;
    let mut seen = vec![false; total];
    let mut stack: Vec<usize> = Vec::new();
    let mut heights_px: Vec<usize> = Vec::new();
    let mut tops_px: Vec<usize> = Vec::new();
    for start in 0..total {
        if !grid[start] || seen[start] {
            continue;
        }
        stack.clear();
        stack.push(start);
        seen[start] = true;
        let mut area = 0_usize;
        let mut y_min = usize::MAX;
        let mut y_max = 0_usize;
        while let Some(index) = stack.pop() {
            area += 1;
            let x = index % grid_w;
            let y = index / grid_w;
            y_min = y_min.min(y);
            y_max = y_max.max(y);
            if x > 0 {
                let n = index - 1;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if x + 1 < grid_w {
                let n = index + 1;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if y > 0 {
                let n = index - grid_w;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if y + 1 < grid_h {
                let n = index + grid_w;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if area >= HALL_BAR_TILE_MIN_AREA {
            // 采样格 → 真实像素：高度 ×2、顶部 y ×2（区域内的相对 y 即可，极差不受影响）。
            heights_px.push((y_max - y_min + 1) * 2);
            tops_px.push(y_min * 2);
        }
    }
    if heights_px.is_empty() {
        return (0, 0, 0, 0);
    }
    let min_height = *heights_px.iter().min().unwrap_or(&0);
    let max_height = *heights_px.iter().max().unwrap_or(&0);
    let top_spread = tops_px.iter().max().unwrap_or(&0) - tops_px.iter().min().unwrap_or(&0);
    (heights_px.len(), min_height, max_height, top_spread)
}

/// 世界判据的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorldGuess {
    Teyvat,
    Qianxing,
    /// 计数落在两侧空档之间，或画面太小根本量不了。**显式拒判**，不猜。
    Unknown,
}

impl WorldGuess {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Teyvat => "teyvat",
            Self::Qianxing => "qianxing",
            Self::Unknown => "unknown",
        }
    }
}

/// 统计左上角青色条纹带里的像素数；画面太小（不是整帧）时返回 `None`。
///
/// 这条判据只在**整帧截图**上有意义：它用的是相对整帧左上角的绝对坐标。
/// 与 `loading_rule` 一样，小裁剪图（按钮、色条）直接不判，避免误报。
/// 统计左上角青色条纹带里的格点数；画面太小（不是整帧）时返回 `None`。
///
/// 这条判据只在**整帧截图**上有意义：它用的是相对整帧左上角的绝对坐标。
/// 与 `loading_rule` 一样，小裁剪图（按钮、色条）直接不判，避免误报。
///
/// `scale` 是 HUD 在场检测找到的尺度。**UI 被整体缩放时区域原点要跟着缩**，
/// 否则会在错误坐标上数、数出接近 0 的青色，进而自信判成提瓦特。
/// 每个格点取对应大小窗口的**多数票**（scale = 1 时窗口只有一个像素，
/// 与第一版逐像素的规则逐字相同）。
fn world_cyan_band_count(image: &DynamicImage, scale: f32) -> Option<usize> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let sampler = RgbSampler::new(image);
    let (image_width, image_height) = sampler.dimensions();
    let step = scale.max(1.0);
    let threshold = WORLD_CYAN_CHANNEL_DIFF as f32;
    let mut count = 0_usize;
    for j in 0..WORLD_CYAN_BAND_RECT.height {
        for i in 0..WORLD_CYAN_BAND_RECT.width {
            let left = (WORLD_CYAN_BAND_RECT.x as f32 + i as f32) * scale;
            let top = (WORLD_CYAN_BAND_RECT.y as f32 + j as f32) * scale;
            let first_x = left.floor() as i32;
            let first_y = top.floor() as i32;
            let last_x = ((left + step).ceil() as i32).max(first_x + 1);
            let last_y = ((top + step).ceil() as i32).max(first_y + 1);
            let mut hits = 0.0_f32;
            let mut samples = 0.0_f32;
            for y in first_y..last_y {
                for x in first_x..last_x {
                    let clamped_x = x.clamp(0, image_width as i32 - 1) as u32;
                    let clamped_y = y.clamp(0, image_height as i32 - 1) as u32;
                    let pixel = sampler.pixel(clamped_x, clamped_y);
                    let red = f32::from(pixel[0]);
                    let green = f32::from(pixel[1]);
                    let blue = f32::from(pixel[2]);
                    if green - red > threshold && blue - red > threshold {
                        hits += 1.0;
                    }
                    samples += 1.0;
                }
            }
            if samples > 0.0 && hits * 2.0 >= samples {
                count += 1;
            }
        }
    }
    Some(count)
}

/// 由青色条纹带计数判世界；落在空档里就返回 `WorldGuess::Unknown`。
fn guess_world_from_cyan_count(count: Option<usize>) -> WorldGuess {
    match count {
        None => WorldGuess::Unknown,
        Some(count) if count >= WORLD_CYAN_QIANXING_MIN => WorldGuess::Qianxing,
        Some(count) if count <= WORLD_CYAN_TEYVAT_MAX => WorldGuess::Teyvat,
        Some(_) => WorldGuess::Unknown,
    }
}

/// 组装世界判据的证据。
///
/// **只在 HUD 在场时给结论。** 这条判据读的是左上角派蒙头像区的青色条纹带，
/// 而全屏菜单（奇域枢纽菜单、派蒙菜单、提瓦特地图、背包、祈愿…）的左上角是
/// **菜单内容**不是 HUD，那里青色计数恒为 0 —— 不加这道门就会把菜单一律
/// 自信地误报成「提瓦特」，正是目标禁止的行为。
///
/// 门用的是 **HUD 在场检测**（`world_hud_scale`），**不是界面 kind**。
/// 用 kind 做过一版，实测有两个毛病：`unknown` 里同时装着全屏菜单和提瓦特大世界，
/// 而 app 的 `overworld` 只能靠"连续 2 帧 unknown"的回退得到 —— 于是那版门
/// 既挡不住菜单也判不了提瓦特，只剩千星大厅一侧能用。
///
/// 门之外仍然把计数报出来（便于复核），但结论固定为 `not_applicable`。
/// 门内如果计数落在两侧空档里，结论仍然是 `unknown`。
/// 画面是不是「**提瓦特大世界**」——人站在提瓦特露天地面、HUD 俱全。
///
/// **为什么要单独抽出来**：第 59 节实测到一次自信误判——**千星大厅的多人面板**
/// （左侧「当前大厅」、右侧玩家列表、右下「邀请玩家」）在实时路径上被判成了 `overworld`。
/// 根因是 `probe_game_state` 回退探针只看右上角祈愿顶栏这一个模板，
/// 而那个模板在千星面板上**也会命中**。
///
/// 所以「祈愿顶栏命中」不足以判大世界，必须再加一道**正交**的世界判据：
/// 既要**有 HUD**（`world_hud_scale` 为 `Some`），又要**判为提瓦特**。
/// 千星大厅本身也有 HUD，但青带计数落在 `qianxing` 一侧，于是被这里挡住。
pub(crate) fn world_is_teyvat(image: &DynamicImage) -> bool {
    // 必须用 `world_hud_scale` 而不是 `world_hud_distances`：前者带**HUD 在场门**
    // （`teyvat.min(qianxing) <= WORLD_HUD_PRESENT_MAX`），后者没有。
    // 少了这道门，全屏面板会被放行，而它左上角的青色计数接近 0，
    // 于是被自信判成提瓦特 —— 这正是第一版写错的地方，实测被回归用例当场抓住。
    let Some(scale) = world_hud_scale(image) else {
        // HUD 不在场 ⇒ 这是全屏面板/菜单，不是大世界。
        return false;
    };
    let Some(count) = world_cyan_band_count(image, scale) else {
        return false;
    };
    matches!(guess_world_from_cyan_count(Some(count)), WorldGuess::Teyvat)
}

fn world_probe_evidence(image: &DynamicImage) -> UiWorldProbeEvidence {
    let hud_scale = world_hud_scale(image);
    let count = world_cyan_band_count(image, hud_scale.unwrap_or(1.0));
    let guess = match hud_scale {
        Some(_) => guess_world_from_cyan_count(count).name(),
        None => "not_applicable",
    };
    UiWorldProbeEvidence::new(
        UiEvidenceRect::new(
            WORLD_CYAN_BAND_RECT.x,
            WORLD_CYAN_BAND_RECT.y,
            WORLD_CYAN_BAND_RECT.width,
            WORLD_CYAN_BAND_RECT.height,
        ),
        count,
        WORLD_CYAN_TEYVAT_MAX,
        WORLD_CYAN_QIANXING_MIN,
        guess,
    )
}

/// 千星奇域地图判据的采样起点与步长。
///
/// 与定阈值时的量法**必须一致**：第 59 节量的是 120x68 粗网格（每格 16x16 像素）
/// 的**格心**，等价于从 8 开始、每 16 像素取样一次。改步长要重新量阈值。
const QIANXING_MAP_SAMPLE_OFFSET: u32 = 8;
const QIANXING_MAP_SAMPLE_STEP: u32 = 16;
/// 记一个"暗点"的灰度上限（与 `screen_tone` 同一口径）。
const QIANXING_MAP_DARK_MAX: f32 = 40.0;
/// 记一个"暗青点"的灰度上限：够暗之后再看偏青还是偏紫。
const QIANXING_MAP_DARK_TEAL_LUMA_MAX: f32 = 100.0;
/// 判定"偏青"的通道差下限：B 比 R 高这么多。
const QIANXING_MAP_TEAL_DIFF: i32 = 10;
/// 判为千星奇域地图的**暗占比下限**。
///
/// 来源（实测空档中点，不是凑的）：第 59 节用 4 帧千星地图与 25 帧其他界面量得——
/// 千星地图暗占比 **37.50 ~ 47.02 %**，其余最高 **26.57 %**（活动一览）
/// ⇒ 空档 26.57~37.50，取中点 **32 %**。
const QIANXING_MAP_DARK_MIN: f32 = 0.32;
/// 判为千星奇域地图的**暗青占比下限**。
///
/// 来源：千星地图 **74.41 ~ 76.31 %**，其余最高 **52.57 %**（提瓦特地图）
/// ⇒ 空档 52.57~74.41，取中点 **64 %**。
///
/// **为什么要两个量一起用**：单看暗占比只有 1.41 倍空档、单看暗青只有 1.42 倍，
/// 都偏窄；但千星地图是**唯一两项同时最高**的界面——整屏是一层深青虚空，
/// 而同样偏暗的「活动一览」是**深紫底**（R 比 B 高），暗青占比明显不够。
/// 两项同时成立才判千星，等于把两个弱条件叠成一个强条件。
const QIANXING_MAP_DARK_TEAL_MIN: f32 = 0.64;

/// 全屏"深青虚空"统计，返回 `(暗占比, 暗青占比)`；画面太小（不是整帧）返回 `None` 拒判。
///
/// 千星奇域「大厅多人面板」（F2）的冻结参考。
///
/// 生成脚本 `dist/qianxingdata/mkf2ref.py`，格式：
/// magic("MWF2") + version(u32) + width(u32) + height(u32) + x(i32) + y(i32)
/// + 归一化参考(i16 x N)，N = width*height*3，归一化值 x1000。
///
/// **区域为什么取"底栏左半"**：第 60 节把三帧 F2 面板（玩家列表各不相同）与 30 帧
/// 其他界面放在一起做区域候选实验，5 个候选都分得很开；选它是因为它是**纯界面骨架**
///（「大厅列表」「新建大厅」两个按钮所在的那条底栏），既不依赖玩家内容、
/// 也不依赖大厅名字与人数，最不容易被"换了个人在大厅"带偏。
///
/// 区域用**格索引**（120x68 网格，每格 16x16 像素），不是像素坐标：
/// 第 (cx,cy) 格的采样点是 `(cx*16+8, cy*16+8)`。这个口径与做实验时完全一致。
const QIANXING_PANEL_REF: &[u8] = include_bytes!("../../assets/qianxing-f2-ref.bin");
const QIANXING_PANEL_REF_MAGIC: &[u8] = b"MWF2";
const QIANXING_PANEL_REF_VERSION: u32 = 1;
const QIANXING_PANEL_REF_SCALE: f32 = 1000.0;
const QIANXING_PANEL_REF_HEADER: usize = 24;
const QIANXING_PANEL_CELL: u32 = 16;
const QIANXING_PANEL_CELL_OFFSET: u32 = 8;

/// 判为千星大厅多人面板的**距离上限**。
///
/// 来源（实测的对数中点，不是凑的）：第 60 节用 3 帧真实 F2 面板与 30 帧其他界面量——
/// 正样本最大距离 **0.0460**，负样本最小距离 **1.0982**，空档 **23.9 倍**，
/// 对数中点 sqrt(0.0460 x 1.0982) ≈ 0.2248，取 **0.22**。
const QIANXING_PANEL_MAX: f32 = 0.22;

/// 比距离时允许的像素平移范围。
///
/// 取 **2** 有两个依据：
/// 1. 世界判据第 57 节的实测——不允许平移时 ±1 像素就翻判断，加 ±2 搜索后 ±1/±2 都不再翻；
/// 2. 第 61 节对本判据的扰动实测——不做搜索时位移 ±3 距离 0.1969、几乎贴住阈值 0.22，
///    加 ±2 搜索后最优对齐只剩 1 像素残差，距离掉回 ≈±1 的水平。
const QIANXING_PANEL_SEARCH: i32 = 2;

/// 解析冻结参考，返回 `(x, y, width, height)` 与**参考本体的字节切片**。
///
/// 参考本体按小端 i16 存。这里**不解码成 `&[i16]`**（那要么 `unsafe`、要么多一次分配），
/// 而是在算距离时逐个解码——480 个格点、1440 个值，开销可以忽略，也省掉一块 `unsafe`。
fn parse_qianxing_panel_ref() -> Option<(u32, u32, u32, u32, &'static [u8])> {
    let bytes = QIANXING_PANEL_REF;
    if bytes.len() < QIANXING_PANEL_REF_HEADER || &bytes[0..4] != QIANXING_PANEL_REF_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != QIANXING_PANEL_REF_VERSION {
        return None;
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let x = i32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let y = i32::from_le_bytes(bytes[20..24].try_into().ok()?);
    if x < 0 || y < 0 || width == 0 || height == 0 {
        return None;
    }
    let values = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(3)?;
    let body = &bytes[QIANXING_PANEL_REF_HEADER..];
    if body.len() != values * 2 {
        return None;
    }
    Some((x as u32, y as u32, width, height, body))
}

/// 按格采样并归一化：减通道均值，再除平均绝对偏差。
///
/// 与参考用的是**同一套归一化**（世界判据也是这一套），所以对整体明暗变化不敏感，
/// 只比"空间花纹"。平均绝对偏差太小时返回 `None`（纯色块没有花纹可比，应当拒判）。
fn qianxing_panel_sample(
    sampler: &RgbSampler<'_>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
) -> Option<Vec<f32>> {
    let (image_width, image_height) = sampler.dimensions();
    if image_width < FULL_FRAME_MIN_WIDTH || image_height < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let mut raw = Vec::with_capacity((width * height * 3) as usize);
    for cell_y in y..y + height {
        for cell_x in x..x + width {
            // **必须钳位**：最后一行格心是 `67*16+8 = 1080`，而 1080 行画面的有效行是 0..1079。
            // 定阈值时的 dump 脚本做了同样的钳位（`if y >= height { y = height-1 }`），
            // 两边口径必须一致，否则参考与实际采样对不上——第一版漏了钳位，
            // 于是正样本直接"量不出来"，被分离度用例当场抓住。
            // 平移后的采样点先**边界检查**：越界说明这个偏移量在这个画面上算不了，
            // 直接放弃该偏移（上层会去试别的偏移量），**不做钳位**——
            // 钳位会把边缘一整行压到同一像素上，凭空制造出一段假花纹。
            let px = (cell_x * QIANXING_PANEL_CELL + QIANXING_PANEL_CELL_OFFSET) as i32 + offset_x;
            let py = (cell_y * QIANXING_PANEL_CELL + QIANXING_PANEL_CELL_OFFSET) as i32 + offset_y;
            if px < 0 || py < 0 || px >= image_width as i32 || py >= image_height as i32 {
                return None;
            }
            let pixel = sampler.pixel(px as u32, py as u32);
            raw.push(f32::from(pixel[0]));
            raw.push(f32::from(pixel[1]));
            raw.push(f32::from(pixel[2]));
        }
    }
    if raw.is_empty() {
        return None;
    }
    let mean = raw.iter().sum::<f32>() / raw.len() as f32;
    for value in raw.iter_mut() {
        *value -= mean;
    }
    let deviation = raw.iter().map(|value| value.abs()).sum::<f32>() / raw.len() as f32;
    if deviation < 1e-6 {
        return None;
    }
    for value in raw.iter_mut() {
        *value /= deviation;
    }
    Some(raw)
}

/// 画面是不是**千星奇域的大厅多人面板**（在大厅里按 F2 打开的那个）。
///
/// 判法：底栏左半的归一化描述子与冻结参考的**逐通道平均绝对差**不超过阈值。
/// 量不了（画面太小 / 纯色）就返回 `false`，交给上层的其它判据或拒判。
fn is_qianxing_player_panel(image: &DynamicImage) -> bool {
    qianxing_player_panel_distance(image).is_some_and(|distance| distance <= QIANXING_PANEL_MAX)
}

/// 与冻结参考的逐通道平均绝对差；`None` 表示这次量不了（画面太小 / 纯色 / 参考损坏）。
///
/// 单独暴露出来，是为了让分离度自证用例能断言**两侧区间不重叠**，而不是只看布尔结论——
/// 只看布尔就退化成了"调阈值凑已观测结果"。
fn qianxing_player_panel_distance(image: &DynamicImage) -> Option<f32> {
    // **允许 ±2 像素平移，取 25 个偏移里最小的那个距离。**
    //
    // 这不是可选项：第 61 节的扰动实测里，不做搜索时位移 ±3 的距离是 **0.1969**，
    // 而阈值是 **0.22**——只有 10% 余量，等于在悬崖边上。
    // 项目里世界判据早就踩过同一个坑（第 57 节："不允许平移时 ±1 像素就翻判断"），
    // 修法就是给它加 ±2 搜索。这里照做：位移 ±3 时最优对齐只剩 1 像素残差，
    // 距离会掉回 ≈±1 的水平（约 0.10），余量从 10% 变成一倍以上。
    //
    // **这是原理性修法，不是调阈值**：阈值 0.22 是实验量出来的空档中点，一个字没动。
    let (x, y, width, height, reference) = parse_qianxing_panel_ref()?;
    // **整帧不做任何转换**：此前每个偏移各自 `to_rgb8()` 一次（25 个偏移 = 25 次全帧拷贝），
    // 改用共享取值器后 RGBA 帧零转换，只做逐格点采样。
    let sampler = RgbSampler::new(image);
    let mut best: Option<f32> = None;
    for offset_y in -QIANXING_PANEL_SEARCH..=QIANXING_PANEL_SEARCH {
        for offset_x in -QIANXING_PANEL_SEARCH..=QIANXING_PANEL_SEARCH {
            let Some(distance) = qianxing_panel_distance_at(
                &sampler, reference, x, y, width, height, offset_x, offset_y,
            ) else {
                continue;
            };
            best = Some(best.map_or(distance, |current: f32| current.min(distance)));
        }
    }
    best
}

/// 在给定平移量下与参考比距离。采样点越界时返回 `None`（该偏移算不了）。
#[allow(clippy::too_many_arguments)]
fn qianxing_panel_distance_at(
    sampler: &RgbSampler<'_>,
    reference: &[u8],
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
) -> Option<f32> {
    let sample = qianxing_panel_sample(sampler, x, y, width, height, offset_x, offset_y)?;
    if sample.len() * 2 != reference.len() {
        return None;
    }
    let mut total = 0.0_f32;
    for (index, value) in sample.iter().enumerate() {
        let raw = i16::from_le_bytes([reference[index * 2], reference[index * 2 + 1]]);
        total += (*value - f32::from(raw) / QIANXING_PANEL_REF_SCALE).abs();
    }
    Some(total / sample.len() as f32)
}

/// 与 `screen_tone` 一样是**整屏标量统计**而不是模板匹配，所以对位移不敏感。
fn qianxing_map_tone(image: &DynamicImage) -> Option<(f32, f32)> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    let mut total = 0_u32;
    let mut dark = 0_u32;
    let mut dark_teal = 0_u32;
    let mut y = QIANXING_MAP_SAMPLE_OFFSET;
    while y < height {
        let mut x = QIANXING_MAP_SAMPLE_OFFSET;
        while x < width {
            let pixel = sampler.pixel(x, y);
            let red = f32::from(pixel[0]);
            let green = f32::from(pixel[1]);
            let blue = f32::from(pixel[2]);
            let luma = 0.299 * red + 0.587 * green + 0.114 * blue;
            if luma < QIANXING_MAP_DARK_MAX {
                dark += 1;
            }
            if luma < QIANXING_MAP_DARK_TEAL_LUMA_MAX
                && i32::from(pixel[2]) - i32::from(pixel[0]) > QIANXING_MAP_TEAL_DIFF
            {
                dark_teal += 1;
            }
            total += 1;
            x += QIANXING_MAP_SAMPLE_STEP;
        }
        y += QIANXING_MAP_SAMPLE_STEP;
    }
    if total == 0 {
        return None;
    }
    Some((dark as f32 / total as f32, dark_teal as f32 / total as f32))
}

/// 是不是千星奇域地图：两个条件必须**同时**成立；量不了就拒判。
fn is_qianxing_map(image: &DynamicImage) -> bool {
    qianxing_map_tone(image).is_some_and(|(dark, dark_teal)| {
        dark >= QIANXING_MAP_DARK_MIN && dark_teal >= QIANXING_MAP_DARK_TEAL_MIN
    })
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

    /// 主菜单必须比二级聊天先判：奇域主菜单上有和二级界面同一个圆形返回按钮，
    /// 但右下角多一个「返回提瓦特」按钮。没有这条规则时，8 张主菜单帧会被
    /// secondary_back 误判成二级聊天（technical-docs/ui-state-machine.md §24/§44）。
    ///
    /// 每帧都用全新分类器：菜单判成 Unknown，连续两帧 Unknown 会触发游戏态回退，
    /// 那会让 final_rule 变成回退规则，测不到我们真正要测的东西。
    #[test]
    fn main_menu_is_classified_before_secondary_back() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
            // 特征化断言：把"已知残留"钉死。淡出帧按钮只剩两成不透明度，
            // 主菜单模板抓不到，于是落到 secondary_back。将来若这条规则改成
            // 能连淡出帧一起覆盖，本断言会失败，提醒回来更新文档结论。
            // 实机侧风险有限：过渡帧不稳定，应用只在稳定类型变化时才推 ui.state 事件。
            assert_eq!(
                c.evidence().final_rule(),
                "secondary_back_template",
                "{path} 的已知残留行为变了，请更新 technical-docs/ui-state-machine.md §44"
            );
        }
        let secondaries = ["secondary-full.png", "sec-fixture.jpg", "round78-live.png"];
        for name in secondaries {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let image = image::open(&path).unwrap();
            let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
            let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
            let c = classifier.classify_at(&image, Instant::now()).unwrap();
            assert_eq!(
                c.kind(),
                RuntimeUiStateKind::Secondary,
                "{path} 必须仍是二级聊天"
            );
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
        let fixture =
            image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg").unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let c = classifier.classify_at(&fixture, Instant::now()).unwrap();
        assert_eq!(c.kind(), RuntimeUiStateKind::Secondary);
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
    fn teyvat_world_chat_is_not_the_hall() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        // 语料跨两个目录：证据目录与运行目录，两边都要找。
        let classify = |name: &str| {
            let path = if std::path::Path::new(&format!("technical-docs/ui-state-evidence/{name}"))
                .exists()
            {
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
                RuntimeUiStateKind::Primary,
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
                RuntimeUiStateKind::Primary,
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
        for name in [
            "loading-screens/loading-dark-A.jpg",
            "loading-screens/loading-dark-B.jpg",
            "loading-screens/loading-dark-C.jpg",
            "loading-screens/loading-dark-D.jpg",
            "loading-screens/loading-dark-E.jpg",
            "loading-screens/loading-dark-teleport.jpg",
        ] {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let (kind, rule, (dark, _white)) = classify_with_rule(&path);
            assert_eq!(
                kind,
                RuntimeUiStateKind::Loading,
                "{path} 是黑底加载界面，必须判成 loading 而不是 unknown"
            );
            assert_eq!(rule, "loading_dark_screen", "{path} 应走黑底那条规则");
            dark_floor = dark_floor.min(dark);
        }

        // 白底元素型（冷启动进世界）：实测近白占比 99.77~100%
        let mut white_floor = f32::MAX;
        for name in [
            "blank-frame.jpg",
            "loading-screen.jpg",
            "cold-start/enter-1.jpg",
            "cold-start/stage-1.jpg",
        ] {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let (kind, rule, (_dark, white)) = classify_with_rule(&path);
            assert_eq!(kind, RuntimeUiStateKind::Loading, "{path} 是白底加载界面");
            assert_eq!(rule, "loading_white_screen", "{path} 应走白底那条规则");
            white_floor = white_floor.min(white);
        }

        // 分离度自证：真实画面里暗像素占比最高、近白占比最高分别是多少
        let mut dark_ceiling = 0.0_f32;
        let mut white_ceiling = 0.0_f32;
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
            let (dark, white) = screen_tone(&image);
            dark_ceiling = dark_ceiling.max(dark);
            white_ceiling = white_ceiling.max(white);
        }
        assert!(
            dark_floor > dark_ceiling,
            "黑底加载与真实画面必须不重叠：加载最低 {dark_floor} vs 真实最高 {dark_ceiling}"
        );
        assert!(
            white_floor > white_ceiling,
            "白底加载与真实画面必须不重叠：加载最低 {white_floor} vs 真实最高 {white_ceiling}"
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
    /// 没有这道门时，判据会把派蒙菜单、奇域枢纽菜单、提瓦特地图一律**自信地**
    /// 报成「提瓦特」—— 这正是目标禁止的行为：判不出来就返回 unknown，而不是猜一个。
    ///
    /// 这条测试是补写的：第 58 节先把青色标量接进了证据链，实测立刻发现
    /// 派蒙菜单被判成 teyvat，才回头补门。第一版门用的是界面 kind，实测又发现
    /// 它同时挡住了提瓦特侧（overworld 只能靠连续 2 帧 unknown 的回退得到），
    /// 于是改成 HUD 在场检测 —— 见下面两条正面测试。
    #[test]
    fn world_probe_never_guesses_a_world_outside_hud_states() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
            assert_ne!(
                c.kind(),
                RuntimeUiStateKind::Primary,
                "{path} 不该被判成千星大厅"
            );
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let world_of = |path: &str| {
            let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
            let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
            let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
            let c = classifier.classify_at(&image, Instant::now()).unwrap();
            let world = c
                .evidence()
                .world_probe()
                .unwrap_or_else(|| panic!("{path} 应带世界探针证据"));
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
    /// 第 59 节把正样本从 1 帧补到 4 帧之后才写这条判据——
    /// 第 58 节就是因为拿 n=1 的候选直接上线，才在菜单上自信判错。
    #[test]
    fn qianxing_map_is_separated_by_dark_and_dark_teal_ratios() {
        let tone_of = |path: &str| {
            let image = image::open(path).unwrap_or_else(|e| panic!("打不开 {path}: {e}"));
            qianxing_map_tone(&image).unwrap_or_else(|| panic!("{path} 应能算出统计量"))
        };

        // 正样本：4 帧千星奇域地图（第 56 节 1 帧 + 第 59 节 3 帧不同缩放）
        let mut dark_floor = f32::MAX;
        let mut teal_floor = f32::MAX;
        for name in [
            "qianxing-entry-56/07-qianxing-map.jpg",
            "qianxing-ui-59/qxmap-a.jpg",
            "qianxing-ui-59/qxmap-b.jpg",
            "qianxing-ui-59/qxmap-c.jpg",
        ] {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let (dark, teal) = tone_of(&path);
            assert!(
                is_qianxing_map(&image::open(&path).unwrap()),
                "{path} 是千星地图，必须判为千星（暗 {dark:.4} / 暗青 {teal:.4}）"
            );
            dark_floor = dark_floor.min(dark);
            teal_floor = teal_floor.min(teal);
        }

        // 负样本：25 帧其他界面（含同样偏暗的活动一览，与偏青的提瓦特地图）
        let mut dark_ceiling = 0.0_f32;
        let mut teal_ceiling = 0.0_f32;
        for name in [
            "sweep-49/02-map.jpg",
            "loading-screens/teyvat-map-full-55.jpg",
            "loading-screens/01-map.jpg",
            "loading-screens/before-teleport-map.jpg",
            "sweep-49/01-paimon-menu.jpg",
            "sweep-50/01-map.jpg",
            "sweep-49/03-inventory.jpg",
            "sweep-49/04-character.jpg",
            "sweep-49/05-adventure-handbook.jpg",
            "sweep-49/10-F2.jpg",
            "sweep-49/10-F2b.jpg",
            "sweep-50/03-after-F2.jpg",
            "sweep-50/06-F2-real.jpg",
            "sweep-49/11-activity.jpg",
            "sweep-49/11-activity-b.jpg",
            "sweep-49/06-wish.jpg",
            "sweep-49/07-friends.jpg",
            "qianxing-entry-56/05-hub-menu.jpg",
            "exit-to-teyvat-56/01-hub-menu.jpg",
            "transition-to-overworld/02-hub-menu.jpg",
            "qianxing-halls-58/hall3-01.jpg",
            "qianxing-halls-58/hall2-01.jpg",
            "qianxing-ui-59/start-hall.jpg",
            "teyvat-regions-58/snow-01.jpg",
            "teyvat-regions-58/desert-01.jpg",
        ] {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let (dark, teal) = tone_of(&path);
            dark_ceiling = dark_ceiling.max(dark);
            teal_ceiling = teal_ceiling.max(teal);
        }

        // 两个轴各自都必须不重叠——这是"两项同时成立"能挡住负样本的根本原因。
        assert!(
            dark_floor > dark_ceiling,
            "暗占比两侧必须不重叠：千星最低 {dark_floor:.4} vs 其余最高 {dark_ceiling:.4}"
        );
        assert!(
            teal_floor > teal_ceiling,
            "暗青占比两侧必须不重叠：千星最低 {teal_floor:.4} vs 其余最高 {teal_ceiling:.4}"
        );
        assert!(
            QIANXING_MAP_DARK_MIN > dark_ceiling && QIANXING_MAP_DARK_MIN <= dark_floor,
            "暗占比阈值 {} 必须严格落在空档 ({dark_ceiling:.4}, {dark_floor:.4}] 内",
            QIANXING_MAP_DARK_MIN
        );
        assert!(
            QIANXING_MAP_DARK_TEAL_MIN > teal_ceiling && QIANXING_MAP_DARK_TEAL_MIN <= teal_floor,
            "暗青阈值 {} 必须严格落在空档 ({teal_ceiling:.4}, {teal_floor:.4}] 内",
            QIANXING_MAP_DARK_TEAL_MIN
        );

        // 小裁剪图必须拒判，而不是拿一小块去猜界面。
        let tiny = DynamicImage::new_rgb8(120, 68);
        assert_eq!(qianxing_map_tone(&tiny), None);
        assert!(!is_qianxing_map(&tiny));
    }
    /// 反向保证：把上面那批真实画面逐帧过一遍分类器，**一个都不许**被判成 loading。
    #[test]
    fn no_real_screen_is_classified_as_loading() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
                RuntimeUiStateKind::Loading,
                "{path} 是真实游戏画面，不许被判成 loading"
            );
        }
    }

    #[test]
    fn chat_markers_need_the_hall_bottom_bar_to_mean_the_hall() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
                RuntimeUiStateKind::Primary,
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
            let image = image::open(&path).unwrap();
            let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
            let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
            let c = classifier.classify_at(&image, Instant::now()).unwrap();
            assert_eq!(c.kind(), RuntimeUiStateKind::Primary, "{path} 仍是千星大厅");
        }
    }

    #[test]
    fn hub_menu_never_falls_through_to_overworld_after_unknown_grace() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
                    RuntimeUiStateKind::Overworld,
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
            let probed =
                probe_game_state_without_entry(&image, &templates, &config.screen).unwrap();
            assert_ne!(
                probed.kind(),
                RuntimeUiStateKind::Overworld,
                "{path} 站在千星大厅里，回退探针不该把它判成大世界"
            );
            assert_ne!(
                probed.evidence().final_rule(),
                "world_wish_template",
                "{path} 被世界判据否掉时应报 world_wish_without_teyvat_hud"
            );
        }
        // 正样本：真实的提瓦特大世界帧仍须通过世界判据，别把门关死。
        for name in [
            "teyvat-regions-58/snow-01.jpg",
            "teyvat-regions-58/desert-01.jpg",
        ] {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let image = image::open(&path).unwrap();
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
        let base =
            image::open("technical-docs/ui-state-evidence/qianxing-ui-59/r10-now.jpg").unwrap();
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
            "world_wish_without_teyvat_hud",
            "祈愿顶栏命中但没有提瓦特 HUD，回退探针必须被世界判据拦下"
        );
        // 第 32 秒：还在探针冷却期（5 秒）里，返回的是基础分类 ⇒ 判据名回到 no_reliable_anchor。
        // 这一对断言把"探针何时跑、跑完门如何裁决"两个时刻都钉住了。
        let cooled = classifier
            .classify_at(&world, now + Duration::from_secs(32))
            .unwrap();
        assert_eq!(cooled.label(), "unknown");
        assert_eq!(cooled.evidence().final_rule(), "no_reliable_anchor");
        // OCR 调用次数是 3 不是 4：第 32 秒在冷却期里，探针根本没跑，
        // 所以不会产生第 4 次 `点击进入` 识别。次数本身也被钉住了。
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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
        // 仍然是 3：第 34 秒还在同一次冷却期内，第 33 秒那帧是 secondary 直接返回。
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
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
        assert_eq!(cached.kind(), RuntimeUiStateKind::Unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let candidate = classifier.classify_cached(&image, &cached).unwrap();
        assert_eq!(candidate.label(), "unknown:game_probe");
        let confirmed = classifier.classify_cached(&image, &candidate).unwrap();
        assert_eq!(confirmed.kind(), RuntimeUiStateKind::GameGate);
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
        // 探针顺序：secondary_back，然后是返回按钮命中后才跑的两个否决探针
        // （main_menu 奇域主菜单、paimon_menu 提瓦特派蒙菜单）。后两者都用来挡住
        // 和二级聊天共用的那个圆形返回按钮。
        // 2026-10-17：好友按钮判据并入大厅（与大厅重复，65/66 帧同判），探针从 4 个减到 3 个。
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
            evidence.template_probes()[2].hit_rect().is_none(),
            "二级界面上不该出现派蒙菜单的功能宫格锚点"
        );
    }

    /// 底栏结构判据：大厅 = 7–14 块**同高、同一条水平线的小圆角白键**
    /// （白 ≥225、块高 26–28 px、顶部 y 极差 0）；主菜单 / 设置宫格是整块白底（60–64 px）
    /// 或跨两行，提瓦特只有一个 Enter，都不满足。
    ///
    /// 2026-10-17 第三轮：白色判据收紧到 **225**（纯 255 只覆盖 89/182 大厅帧：
    /// JPEG/回放/DPI 缩放帧的白色只有 240–254）；再按**实际像素高度 + 同一水平线**判形状：
    /// 主菜单白卡高 60–64 px、设置宫格还跨两行（顶部 985/1045），都不满足。
    #[test]
    fn hall_bar_structure_separates_hall_from_menu_and_teyvat() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let rect: Rect = config.screen.hall_bar_rect.into();
        let signature = |name: &str| {
            let path = format!("technical-docs/ui-state-evidence/{name}");
            let image = image::open(&path).unwrap();
            hall_bar_signature(&image, rect)
        };
        let is_hall = |name: &str| {
            let (tiles, min_height, max_height, top_spread) = signature(name);
            (HALL_BAR_MIN_TILES..=HALL_BAR_MAX_TILES).contains(&tiles)
                && min_height >= HALL_BAR_MIN_TILE_HEIGHT_PX
                && max_height <= HALL_BAR_MAX_TILE_HEIGHT_PX
                && top_spread <= HALL_BAR_MAX_TILE_TOP_SPREAD_PX
        };
        for name in [
            "hall-chat-open.jpg",
            "qianxing-halls-58/hall3-01.jpg",
            "full-hall.jpg",
            "hall-chat-closed.jpg",
        ] {
            assert!(is_hall(name), "{name} 是千星大厅，底栏应是多块同高、同一水平线的小白键");
        }
        for name in ["wonderland-menu.png", "qianxing-ui-59/r16-settings-grid.png"] {
            assert!(!is_hall(name), "{name} 是整块白底/跨行，不得判成大厅");
        }
        let (_, menu_min_height, _, _) = signature("wonderland-menu.png");
        assert!(
            menu_min_height > HALL_BAR_MAX_TILE_HEIGHT_PX,
            "主菜单白卡的块高应超过键高上限（实测 60–64 px）"
        );
        let (_, _, _, settings_top_spread) = signature("qianxing-ui-59/r16-settings-grid.png");
        assert!(
            settings_top_spread > HALL_BAR_MAX_TILE_TOP_SPREAD_PX,
            "设置宫格应跨两行（顶部极差应大于 4 px）"
        );
        let (teyvat_tiles, _, _, _) =
            signature("transition-to-overworld/05-overworld-teyvat.jpg");
        assert!(teyvat_tiles < HALL_BAR_MIN_TILES, "提瓦特底栏只有一个 Enter");
    }

    fn panel_regions_from(config: &AppConfig) -> PanelRegions {
        PanelRegions {
            friend_panel: config.moderation.friend_panel_region.into(),
            friend_search_panel: config.moderation.search_panel_region.into(),
            friend_block_chat: config.moderation.block_chat_region.into(),
            friend_confirm: config.moderation.confirm_region.into(),
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let panels = panel_regions_from(&config);
        for (name, label) in [
            ("panel-friend-list-1920x1080.jpg", "panel:friend_list"),
            ("panel-add-friend-1920x1080.jpg", "panel:add_friend"),
            ("panel-more-settings-1920x1080.jpg", "panel:more_settings"),
            ("panel-confirm-dialog-1920x1080.jpg", "panel:confirm_dialog"),
            ("panel-archive-card-1920x1080.jpg", "panel:archive_card"),
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
                RuntimeUiStateKind::Panel,
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

    /// 派蒙菜单（提瓦特 ESC 系统菜单）和二级聊天共用**同一个**圆形返回按钮，但它左侧
    /// 换成功能宫格。只靠 `secondary_back` 会把它误判成二级聊天；改用「商城」图标这个专属
    /// 锚点后判为 `MainMenu`，证据规则名与奇域主菜单区分开。
    #[test]
    fn paimon_menu_is_not_mistaken_for_secondary_chat() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
        let image = image::open("tests/fixtures/ui/menu-paimon-1920x1080.jpg")
            .expect("打开派蒙菜单夹具失败");
        let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
        let classification = classifier.classify_at(&image, Instant::now()).unwrap();
        assert_eq!(
            classification.kind(),
            RuntimeUiStateKind::MainMenu,
            "派蒙菜单被误判成 {}",
            classification.kind().name()
        );
        assert_eq!(
            classification.evidence().final_rule(),
            "paimon_menu_panel_template"
        );
    }
}
