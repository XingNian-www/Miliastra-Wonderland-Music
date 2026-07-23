use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::card_games::LandlordConfig;
use crate::features::custom_workflow::{CustomWorkflowConfig, WorkflowTimingConfig};
use crate::features::idiom_chain::IdiomChainConfig;
use crate::features::invite::{InviteConfig, InviteTimingConfig};
use crate::features::moderation::{ModerationConfig, ModerationTimingConfig};
use crate::features::playback::{MatchConfig, PlaybackTimingConfig, QueueConfig, SongDedupConfig};
use crate::features::song_request::{AiConfig, SongReviewConfig};
use crate::features::startup::StartupConfig;
use crate::features::turtle_soup::TurtleSoupConfig;
use crate::features::undercover::UndercoverConfig;
use crate::runtime::player::PlayerObservationConfig;
use crate::runtime::player_io::{PlayerRuntimeConfig, PlayerRuntimeConfigError};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub window: WindowConfig,
    pub screen: ScreenConfig,
    pub stability: StabilityConfig,
    pub timing: TimingConfig,
    pub ocr: OcrConfig,
    pub templates: TemplateConfig,
    pub output: OutputConfig,
    pub moderation: ModerationConfig,
    pub feeluown: FeelUOwnConfig,
    pub http: HttpConfig,
    pub logging: LoggingConfig,
    pub tui: TuiConfig,
    pub state: StateConfig,
    pub queue: QueueConfig,
    pub song_dedup: SongDedupConfig,
    pub idiom_chain: IdiomChainConfig,
    pub landlord: LandlordConfig,
    pub undercover: UndercoverConfig,
    pub turtle_soup: TurtleSoupConfig,
    pub ai: AiConfig,
    pub song_review: SongReviewConfig,
    pub matching: MatchConfig,
    pub hotkeys: HotkeyConfig,
    pub startup: StartupConfig,
    pub invite: InviteConfig,
    pub friend_delivery: FriendDeliveryConfig,
    pub custom_workflows: CustomWorkflowConfig,
}

const BUILTIN_STABILITY_COUNT: u32 = 2;
const PLAYER_FAST_OBSERVATION_INTERVAL: Duration = Duration::from_millis(300);
const PLAYER_OBSERVATION_COMMAND_CAPACITY: usize = 16;
const PLAYER_ACTIVE_FAST_DEMAND_CAPACITY: usize = 16;
const PLAYER_CONTROL_QUEUE_CAPACITY: usize = 16;
const PLAYER_SEARCH_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StabilityConfig {
    pub default_count: u32,
    pub ui_state_count: u32,
    pub secondary_hall_count: u32,
}

pub(crate) fn resolve_stability_count(local: u32, global: u32) -> u32 {
    if local > 1 {
        local
    } else if global > 1 {
        global
    } else {
        BUILTIN_STABILITY_COUNT
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    /// 支持逗号、分号、竖线或空白分隔的多个进程名。
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
    pub focus_point: PointConfig,
}

impl AppConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        self.timing.validate()?;
        self.player_runtime_config()
            .context("校验播放器运行时配置")?;
        if self.window.target_process.trim().is_empty() {
            bail!("window.target_process 不能为空");
        }
        if self.window.content_width == 0 || self.window.content_height == 0 {
            bail!("window.content_width 和 window.content_height 必须大于 0");
        }
        if self.screen.expected_width == 0 || self.screen.expected_height == 0 {
            bail!("screen.expected_width 和 screen.expected_height 必须大于 0");
        }
        if self.window.content_width != self.screen.expected_width
            || self.window.content_height != self.screen.expected_height
        {
            bail!(
                "window.content_width/content_height 必须与 screen.expected_width/expected_height 一致"
            );
        }
        self.screen.validate()?;
        self.ocr.validate()?;
        self.templates.validate()?;
        self.feeluown.validate()?;
        self.http.validate()?;
        self.logging.validate()?;
        self.tui.validate()?;
        self.state.validate()?;
        self.hotkeys.validate()?;
        self.queue.validate()?;
        self.song_dedup.validate()?;
        self.matching.validate()?;
        self.idiom_chain.validate()?;
        self.landlord.validate()?;
        self.undercover.validate()?;
        self.invite.validate(&self.timing.invite)?;
        self.moderation.validate(&self.timing.moderation)?;
        self.custom_workflows.validate()?;
        self.startup.validate()?;
        self.ai.validate()?;
        self.song_review.validate()?;
        self.turtle_soup.validate()?;
        self.validate_ui_geometry()?;
        Ok(())
    }

    fn validate_ui_geometry(&self) -> Result<()> {
        let canvas = (self.screen.expected_width, self.screen.expected_height);
        for (rect, field) in [
            (self.invite.friend_list_region, "invite.friend_list_region"),
            (self.invite.friend_chat_region, "invite.friend_chat_region"),
            (self.invite.view_star_region, "invite.view_star_region"),
            (self.invite.goto_hall_region, "invite.goto_hall_region"),
            (self.invite.enter_hall_region, "invite.enter_hall_region"),
            (
                self.moderation.friend_panel_region,
                "moderation.friend_panel_region",
            ),
            (
                self.moderation.search_panel_region,
                "moderation.search_panel_region",
            ),
            (
                self.moderation.more_settings_region,
                "moderation.more_settings_region",
            ),
            (
                self.moderation.block_chat_region,
                "moderation.block_chat_region",
            ),
            (
                self.moderation.blacklist_region,
                "moderation.blacklist_region",
            ),
            (self.moderation.confirm_region, "moderation.confirm_region"),
            (
                self.startup.enter_game_text_region,
                "startup.enter_game_text_region",
            ),
            (
                self.startup.wonderland_enter_button_region,
                "startup.wonderland_enter_button_region",
            ),
            (self.startup.main_ui_region, "startup.main_ui_region"),
            (
                self.startup.wonderland_close_region,
                "startup.wonderland_close_region",
            ),
        ] {
            validate_rect_in_canvas(rect, field, canvas)?;
        }
        for (point, field) in [
            (self.window.focus_point, "window.focus_point"),
            (self.output.focus_point, "output.focus_point"),
            (self.output.chat_click_2, "output.chat_click_2"),
            (
                self.moderation.search_input_point,
                "moderation.search_input_point",
            ),
            (
                self.moderation.search_button_point,
                "moderation.search_button_point",
            ),
            (
                self.startup.wonderland_card_point,
                "startup.wonderland_card_point",
            ),
        ] {
            validate_point_in_canvas(point, field, canvas)?;
        }
        for workflow in self
            .custom_workflows
            .workflows
            .iter()
            .filter(|workflow| workflow.enabled)
        {
            for (index, step) in workflow.steps.iter().enumerate() {
                if let Some(region) = step.region {
                    validate_rect_in_canvas(
                        region,
                        &format!("custom_workflows.{}.steps[{index}].region", workflow.name),
                        canvas,
                    )?;
                }
                if let Some(point) = step.point {
                    validate_point_in_canvas(
                        point,
                        &format!("custom_workflows.{}.steps[{index}].point", workflow.name),
                        canvas,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_stability_count(&self, local: u32) -> u32 {
        resolve_stability_count(local, self.stability.default_count)
    }

    pub(crate) fn resolve_stability_count_usize(&self, local: usize) -> usize {
        if local > 1 {
            local
        } else {
            self.resolve_stability_count(local as u32) as usize
        }
    }

    pub(crate) fn player_runtime_config(
        &self,
    ) -> std::result::Result<PlayerRuntimeConfig, PlayerRuntimeConfigError> {
        let normal_observation_interval =
            Duration::from_millis(self.timing.playback.monitor_status_ms);
        let fast_observation_interval =
            if normal_observation_interval > PLAYER_FAST_OBSERVATION_INTERVAL {
                PLAYER_FAST_OBSERVATION_INTERVAL
            } else {
                normal_observation_interval / 2
            };
        let defaults = PlayerObservationConfig::default();
        let config = PlayerRuntimeConfig {
            observation: PlayerObservationConfig {
                uri_stable_samples: self
                    .resolve_stability_count(self.timing.playback.uri_stable_samples)
                    as usize,
                transport_stable_samples: self
                    .resolve_stability_count(self.timing.playback.transport_stable_samples)
                    as usize,
                stale_timeout: Duration::from_millis(self.timing.playback.stale_timeout_ms),
                ..defaults
            },
            normal_observation_interval,
            fast_observation_interval,
            observation_command_capacity: PLAYER_OBSERVATION_COMMAND_CAPACITY,
            active_fast_demand_capacity: PLAYER_ACTIVE_FAST_DEMAND_CAPACITY,
            control_queue_capacity: PLAYER_CONTROL_QUEUE_CAPACITY,
            search_queue_capacity: PLAYER_SEARCH_QUEUE_CAPACITY,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| {
            format!(
                "读取配置失败: {}。请将发布包中的 config.yaml 放在程序工作目录",
                path.display()
            )
        })?;
        serde_yaml::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))
    }
}

fn validate_unit_interval(value: f32, field: &str) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{} 必须是 0 到 1 之间的有限小数", field);
    }
    Ok(())
}

pub(crate) fn validate_rect(rect: RectConfig, field: &str) -> Result<()> {
    if rect.width == 0 || rect.height == 0 {
        bail!("{} 的 width 和 height 必须大于 0", field);
    }
    Ok(())
}

fn validate_rect_in_canvas(
    rect: RectConfig,
    field: &str,
    (canvas_width, canvas_height): (u32, u32),
) -> Result<()> {
    validate_rect(rect, field)?;
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || right > i64::from(canvas_width)
        || bottom > i64::from(canvas_height)
    {
        bail!(
            "{} 必须完整位于 {}x{} 画布内",
            field,
            canvas_width,
            canvas_height
        );
    }
    Ok(())
}

fn validate_point_in_canvas(
    point: PointConfig,
    field: &str,
    (canvas_width, canvas_height): (u32, u32),
) -> Result<()> {
    if point.x < 0
        || point.y < 0
        || i64::from(point.x) >= i64::from(canvas_width)
        || i64::from(point.y) >= i64::from(canvas_height)
    {
        bail!(
            "{} 必须位于 {}x{} 画布内",
            field,
            canvas_width,
            canvas_height
        );
    }
    Ok(())
}

fn validate_nonempty_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{} 不能为空", field);
    }
    Ok(())
}

#[cfg(test)]
fn bundled_config_yaml() -> &'static str {
    include_str!("../../config.yaml")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RectConfig {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointConfig {
    pub x: i32,
    pub y: i32,
}

impl PointConfig {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenConfig {
    pub expected_width: u32,
    pub expected_height: u32,
    pub warn_on_size_mismatch: bool,
    pub chat_rect: RectConfig,
    pub friend_rect: RectConfig,
    pub secondary_back_rect: RectConfig,
    pub secondary_hall_rect: RectConfig,
    pub hall_name_rect: RectConfig,
    pub hall_time_rect: RectConfig,
}

impl ScreenConfig {
    fn validate(&self) -> Result<()> {
        for (rect, field) in [
            (self.chat_rect, "screen.chat_rect"),
            (self.friend_rect, "screen.friend_rect"),
            (self.secondary_back_rect, "screen.secondary_back_rect"),
            (self.secondary_hall_rect, "screen.secondary_hall_rect"),
            (self.hall_name_rect, "screen.hall_name_rect"),
            (self.hall_time_rect, "screen.hall_time_rect"),
        ] {
            validate_rect_in_canvas(rect, field, (self.expected_width, self.expected_height))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingConfig {
    pub watchdog_restart_ms: u64,
    pub loop_idle_ms: u64,
    pub chat_scan: ChatScanTimingConfig,
    pub command: CommandTimingConfig,
    pub input: InputTimingConfig,
    pub workflow: WorkflowTimingConfig,
    pub hall: HallTimingConfig,
    pub invite: InviteTimingConfig,
    pub moderation: ModerationTimingConfig,
    pub playback: PlaybackTimingConfig,
    pub decision: DecisionTimingConfig,
    pub external: ExternalTimingConfig,
}

impl TimingConfig {
    fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.watchdog_restart_ms, "timing.watchdog_restart_ms"),
            (self.loop_idle_ms, "timing.loop_idle_ms"),
            (self.chat_scan.fallback_ms, "timing.chat_scan.fallback_ms"),
            (
                self.chat_scan.change_debounce_ms,
                "timing.chat_scan.change_debounce_ms",
            ),
            (
                self.chat_scan.change_cooldown_ms,
                "timing.chat_scan.change_cooldown_ms",
            ),
            (self.command.ui_timeout_ms, "timing.command.ui_timeout_ms"),
            (
                self.command.return_retry_ms,
                "timing.command.return_retry_ms",
            ),
            (self.command.post_settle_ms, "timing.command.post_settle_ms"),
            (self.command.help_batch_ms, "timing.command.help_batch_ms"),
            (
                self.input.after_activate_ms,
                "timing.input.after_activate_ms",
            ),
            (self.input.focus_ms, "timing.input.focus_ms"),
            (self.input.open_chat_ms, "timing.input.open_chat_ms"),
            (self.input.click_ms, "timing.input.click_ms"),
            (self.input.text_ms, "timing.input.text_ms"),
            (self.input.send_ms, "timing.input.send_ms"),
            (self.hall.page_settle_ms, "timing.hall.page_settle_ms"),
            (
                self.hall.ocr_sample_interval_ms,
                "timing.hall.ocr_sample_interval_ms",
            ),
            (self.decision.timeout_ms, "timing.decision.timeout_ms"),
            (self.decision.poll_ms, "timing.decision.poll_ms"),
            (
                self.external.feeluown_rpc_timeout_ms,
                "timing.external.feeluown_rpc_timeout_ms",
            ),
            (
                self.external.volume_smooth_step_ms,
                "timing.external.volume_smooth_step_ms",
            ),
            (
                self.external.ai_request_timeout_ms,
                "timing.external.ai_request_timeout_ms",
            ),
        ] {
            if value == 0 {
                bail!("{} 必须大于 0", field);
            }
        }
        self.workflow.validate()?;
        self.playback.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatScanTimingConfig {
    pub fallback_ms: u64,
    pub change_debounce_ms: u64,
    pub change_cooldown_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTimingConfig {
    pub ui_timeout_ms: u64,
    pub return_retry_ms: u64,
    pub post_settle_ms: u64,
    pub help_batch_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputTimingConfig {
    pub after_activate_ms: u64,
    pub focus_ms: u64,
    pub open_chat_ms: u64,
    pub click_ms: u64,
    pub text_ms: u64,
    pub send_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HallTimingConfig {
    pub page_settle_ms: u64,
    pub ocr_sample_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTimingConfig {
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTimingConfig {
    pub feeluown_rpc_timeout_ms: u64,
    pub volume_smooth_step_ms: u64,
    pub ai_request_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrConfig {
    /// MNN/PaddleOCR detection model. It is only required when an MNN backend
    /// is selected; OpenVINO-only deployments can omit it.
    #[serde(default)]
    pub det_model: Option<PathBuf>,
    /// MNN/PaddleOCR recognition model. It is only required when an MNN backend
    /// is selected; OpenVINO-only deployments can omit it.
    #[serde(default)]
    pub rec_model: Option<PathBuf>,
    pub charset: PathBuf,
    pub min_confidence: f32,
    pub threads: i32,
    pub backend_priority: Vec<String>,
    /// Optional OpenVINO IR model configuration. This is ignored unless
    /// `openvino` appears in `backend_priority`.
    #[serde(default)]
    pub openvino: OpenVinoConfig,
    pub det_max_side_len: u32,
    pub det_score_threshold: f32,
    pub det_unclip_ratio: f32,
    pub det_min_area: u32,
    pub det_box_border: u32,
    pub change_mean_threshold: f32,
    pub change_pixel_threshold: f32,
    pub text_left_gap: i32,
    pub block_top_padding: i32,
    pub block_bottom_padding: i32,
    pub max_block_height: i32,
    pub same_line_y_tolerance: i32,
    pub marker_dedupe_x: i32,
    pub marker_dedupe_y: i32,
    pub next_marker_min_gap: i32,
    pub right_padding: i32,
    pub batch_recognize: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVinoConfig {
    /// Detection model XML file exported as OpenVINO IR.
    #[serde(default)]
    pub det_model: Option<PathBuf>,
    /// Detection model weights file paired with `det_model`.
    #[serde(default)]
    pub det_weights: Option<PathBuf>,
    /// Recognition model XML file exported as OpenVINO IR.
    #[serde(default)]
    pub rec_model: Option<PathBuf>,
    /// Recognition model weights file paired with `rec_model`.
    #[serde(default)]
    pub rec_weights: Option<PathBuf>,
    /// OpenVINO device name, normally `CPU` (also `GPU`/`NPU` when installed).
    #[serde(default = "default_openvino_device")]
    pub device: String,
}

impl Default for OpenVinoConfig {
    fn default() -> Self {
        Self {
            det_model: None,
            det_weights: None,
            rec_model: None,
            rec_weights: None,
            device: default_openvino_device(),
        }
    }
}

fn default_openvino_device() -> String {
    "CPU".to_string()
}

impl OpenVinoConfig {
    fn validate(&self) -> Result<()> {
        for (path, field) in [
            (&self.det_model, "ocr.openvino.det_model"),
            (&self.det_weights, "ocr.openvino.det_weights"),
            (&self.rec_model, "ocr.openvino.rec_model"),
            (&self.rec_weights, "ocr.openvino.rec_weights"),
        ] {
            let Some(path) = path else {
                bail!("{field} 在启用 OpenVINO 后端时不能为空");
            };
            validate_nonempty_path(path, field)?;
        }
        if self.device.trim().is_empty() {
            bail!("ocr.openvino.device 不能为空");
        }
        Ok(())
    }
}

impl OcrConfig {
    fn validate(&self) -> Result<()> {
        validate_unit_interval(self.min_confidence, "ocr.min_confidence")?;
        validate_unit_interval(self.det_score_threshold, "ocr.det_score_threshold")?;
        validate_unit_interval(self.change_pixel_threshold, "ocr.change_pixel_threshold")?;
        if !self.change_mean_threshold.is_finite() || self.change_mean_threshold < 0.0 {
            bail!("ocr.change_mean_threshold 必须是非负有限小数");
        }
        if self.threads <= 0 {
            bail!("ocr.threads 必须大于 0");
        }
        if self.det_max_side_len == 0 || self.det_min_area == 0 || self.max_block_height <= 0 {
            bail!("OCR 检测尺寸和文本块高度必须大于 0");
        }
        if !self.det_unclip_ratio.is_finite() || self.det_unclip_ratio <= 0.0 {
            bail!("ocr.det_unclip_ratio 必须是正有限小数");
        }
        if self.backend_priority.is_empty() {
            bail!("ocr.backend_priority 不能为空");
        }
        for backend in &self.backend_priority {
            if !matches!(
                backend.trim().to_ascii_lowercase().as_str(),
                "cuda" | "vulkan" | "opencl" | "open-cl" | "openvino" | "cpu"
            ) {
                bail!("ocr.backend_priority 包含不支持的后端: {}", backend);
            }
        }
        let openvino_selected = self
            .backend_priority
            .iter()
            .any(|backend| backend.trim().eq_ignore_ascii_case("openvino"));
        if openvino_selected {
            self.openvino.validate()?;
        }
        let mnn_selected = self
            .backend_priority
            .iter()
            .any(|backend| !backend.trim().eq_ignore_ascii_case("openvino"));
        if mnn_selected {
            for (path, field) in [
                (&self.det_model, "ocr.det_model"),
                (&self.rec_model, "ocr.rec_model"),
            ] {
                let Some(path) = path else {
                    bail!("{field} 在启用 MNN 后端时不能为空");
                };
                validate_nonempty_path(path, field)?;
            }
        }
        validate_nonempty_path(&self.charset, "ocr.charset")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateConfig {
    pub blue_marker: PathBuf,
    pub yellow_marker: PathBuf,
    pub pink_marker: PathBuf,
    pub friend: PathBuf,
    pub secondary_back: PathBuf,
    pub secondary_hall: PathBuf,
    pub invite_view_star: PathBuf,
    pub invite_goto_hall: PathBuf,
    pub invite_enter_hall: PathBuf,
    pub friend_panel: PathBuf,
    pub friend_search_panel: PathBuf,
    pub friend_more_settings: PathBuf,
    pub friend_block_chat: PathBuf,
    pub friend_blacklist: PathBuf,
    pub friend_confirm: PathBuf,
    pub marker_threshold: f32,
}

impl TemplateConfig {
    fn validate(&self) -> Result<()> {
        validate_unit_interval(self.marker_threshold, "templates.marker_threshold")?;
        for (path, field) in [
            (&self.blue_marker, "templates.blue_marker"),
            (&self.yellow_marker, "templates.yellow_marker"),
            (&self.pink_marker, "templates.pink_marker"),
            (&self.friend, "templates.friend"),
            (&self.secondary_back, "templates.secondary_back"),
            (&self.secondary_hall, "templates.secondary_hall"),
            (&self.invite_view_star, "templates.invite_view_star"),
            (&self.invite_goto_hall, "templates.invite_goto_hall"),
            (&self.invite_enter_hall, "templates.invite_enter_hall"),
            (&self.friend_panel, "templates.friend_panel"),
            (&self.friend_search_panel, "templates.friend_search_panel"),
            (&self.friend_more_settings, "templates.friend_more_settings"),
            (&self.friend_block_chat, "templates.friend_block_chat"),
            (&self.friend_blacklist, "templates.friend_blacklist"),
            (&self.friend_confirm, "templates.friend_confirm"),
        ] {
            validate_nonempty_path(path, field)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    pub send_enabled: bool,
    pub focus_point: PointConfig,
    pub chat_click_2: PointConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeelUOwnConfig {
    pub host: String,
    pub port: u16,
}

impl FeelUOwnConfig {
    fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("feeluown.host 不能为空");
        }
        if self.port == 0 {
            bail!("feeluown.port 必须大于 0");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    pub access_token: String,
}

impl HttpConfig {
    fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.host.trim().is_empty() {
            bail!("http.host 不能为空");
        }
        if self.port == 0 {
            bail!("http.port 必须大于 0");
        }
        if !matches!(
            self.host.trim().to_ascii_lowercase().as_str(),
            "127.0.0.1" | "localhost" | "::1"
        ) && self.access_token.trim().is_empty()
        {
            bail!("HTTP 监听非本机地址时必须设置 http.access_token");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
    pub rotate_daily: bool,
    pub retain_days: u32,
}

impl LoggingConfig {
    fn validate(&self) -> Result<()> {
        validate_nonempty_path(&self.dir, "logging.dir")?;
        if !matches!(
            self.level.trim().to_ascii_lowercase().as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            bail!("logging.level 必须是 error/warn/info/debug/trace 之一");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    pub enabled: bool,
    pub refresh_ms: u64,
    pub log_lines: usize,
}

impl TuiConfig {
    fn validate(&self) -> Result<()> {
        if self.enabled && (self.refresh_ms == 0 || self.log_lines == 0) {
            bail!("tui.refresh_ms 和 tui.log_lines 必须大于 0");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    pub playback_state_path: PathBuf,
    pub hall_state_path: PathBuf,
    pub queue_path: PathBuf,
    pub executed_commands_log_path: PathBuf,
}

impl StateConfig {
    fn validate(&self) -> Result<()> {
        for (path, field) in [
            (&self.playback_state_path, "state.playback_state_path"),
            (&self.hall_state_path, "state.hall_state_path"),
            (&self.queue_path, "state.queue_path"),
            (
                &self.executed_commands_log_path,
                "state.executed_commands_log_path",
            ),
        ] {
            validate_nonempty_path(path, field)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub pause_key: String,
    pub exit_key: String,
}

impl HotkeyConfig {
    fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.pause_key.trim().is_empty() || self.exit_key.trim().is_empty() {
            bail!("hotkeys.pause_key 和 hotkeys.exit_key 不能为空");
        }
        if self.pause_key.eq_ignore_ascii_case(&self.exit_key) {
            bail!("hotkeys.pause_key 和 hotkeys.exit_key 不能相同");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FriendDeliveryConfig {
    /// Maximum automatic retries for a message that is confirmed not to have been sent.
    pub auto_retry_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    type ConfigMutation = (&'static str, fn(&mut AppConfig));

    fn playback_timing(yaml: &str) -> PlaybackTimingConfig {
        serde_yaml::from_str(yaml).expect("valid playback timing config")
    }

    #[test]
    fn playback_observation_fields_accept_explicit_inheritance_and_stale_timeout() {
        let playback = playback_timing(
            r#"
search_settle_ms: 2000
status_poll_ms: 1000
status_retries: 15
skip_status_initial_ms: 500
skip_status_poll_ms: 300
skip_status_retries: 5
monitor_tick_ms: 200
monitor_status_ms: 1000
uri_stable_samples: 0
transport_stable_samples: 0
stale_timeout_ms: 5000
"#,
        );

        assert_eq!(playback.uri_stable_samples, 0);
        assert_eq!(playback.transport_stable_samples, 0);
        assert_eq!(playback.stale_timeout_ms, 5000);
    }

    #[test]
    fn playback_observation_rejects_zero_stale_timeout() {
        let error = serde_yaml::from_str::<PlaybackTimingConfig>(
            r#"
search_settle_ms: 2000
status_poll_ms: 1000
status_retries: 15
skip_status_initial_ms: 500
skip_status_poll_ms: 300
skip_status_retries: 5
monitor_tick_ms: 200
monitor_status_ms: 1000
uri_stable_samples: 0
transport_stable_samples: 0
stale_timeout_ms: 0
"#,
        )
        .expect_err("zero stale timeout must be rejected");

        assert!(error.to_string().contains("positive integer"));
    }

    #[test]
    fn playback_observation_stability_uses_local_then_global_then_builtin_default() {
        let local = playback_timing(
            r#"
search_settle_ms: 2000
status_poll_ms: 1000
status_retries: 15
skip_status_initial_ms: 500
skip_status_poll_ms: 300
skip_status_retries: 5
monitor_tick_ms: 200
monitor_status_ms: 1000
uri_stable_samples: 4
transport_stable_samples: 3
stale_timeout_ms: 7500
"#,
        );
        assert_eq!(resolve_stability_count(local.uri_stable_samples, 6), 4);
        assert_eq!(
            resolve_stability_count(local.transport_stable_samples, 6),
            3
        );
        assert_eq!(local.stale_timeout_ms, 7500);

        let inherited = PlaybackTimingConfig {
            uri_stable_samples: 1,
            transport_stable_samples: 0,
            ..local
        };
        assert_eq!(resolve_stability_count(inherited.uri_stable_samples, 6), 6);
        assert_eq!(
            resolve_stability_count(inherited.transport_stable_samples, 6),
            6
        );
        assert_eq!(resolve_stability_count(inherited.uri_stable_samples, 1), 2);
    }

    #[test]
    fn app_config_builds_the_complete_player_runtime_config_once() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default app config");
        config.stability.default_count = 6;
        config.timing.playback.uri_stable_samples = 4;
        config.timing.playback.transport_stable_samples = 1;
        config.timing.playback.monitor_status_ms = 1_000;
        config.timing.playback.stale_timeout_ms = 7_500;

        let runtime = config
            .player_runtime_config()
            .expect("valid player runtime config");

        assert_eq!(runtime.observation.uri_stable_samples, 4);
        assert_eq!(runtime.observation.transport_stable_samples, 6);
        assert_eq!(
            runtime.observation.stale_timeout,
            Duration::from_millis(7_500)
        );
        assert_eq!(runtime.normal_observation_interval, Duration::from_secs(1));
        assert_eq!(
            runtime.fast_observation_interval,
            Duration::from_millis(300)
        );
        assert_eq!(runtime.observation_command_capacity, 16);
        assert_eq!(runtime.active_fast_demand_capacity, 16);
        assert_eq!(runtime.control_queue_capacity, 16);
        assert_eq!(runtime.search_queue_capacity, 16);

        config.stability.default_count = 1;
        config.timing.playback.uri_stable_samples = 0;
        config.timing.playback.transport_stable_samples = 1;
        let runtime = config
            .player_runtime_config()
            .expect("invalid local and global counts use the built-in default");
        assert_eq!(runtime.observation.uri_stable_samples, 2);
        assert_eq!(runtime.observation.transport_stable_samples, 2);
    }

    #[test]
    fn default_app_config_passes_startup_validation() {
        let config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");

        config.validate().expect("default config is valid");
    }

    #[test]
    fn default_direction_workflows_click_middle_before_every_direction_key() {
        let config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");

        let workflows = &config.custom_workflows.workflows;
        let direction_start = workflows
            .iter()
            .position(|workflow| workflow.name == "hold-w")
            .expect("missing default hold-w workflow");
        let mouse = direction_start
            .checked_sub(1)
            .and_then(|index| workflows.get(index))
            .expect("middle mouse workflow must precede WSAD");
        assert_eq!(mouse.name, "鼠标中键");
        assert!(mouse.commands.is_empty());
        assert_eq!(mouse.message_types, ["pink"]);
        assert!(!mouse.allow_args);
        assert_eq!(
            mouse
                .steps
                .iter()
                .map(|step| step.step_type.as_str())
                .collect::<Vec<_>>(),
            ["ensure_primary", "mouse_button"]
        );
        assert_eq!(mouse.steps[1].button.as_deref(), Some("middle"));

        for (name, key) in [
            ("hold-w", "W"),
            ("hold-s", "S"),
            ("hold-a", "A"),
            ("hold-d", "D"),
        ] {
            let workflow = workflows
                .iter()
                .find(|workflow| workflow.name == name)
                .unwrap_or_else(|| panic!("missing default workflow {name}"));
            assert_eq!(
                workflow
                    .steps
                    .iter()
                    .map(|step| step.step_type.as_str())
                    .collect::<Vec<_>>(),
                ["ensure_primary", "mouse_button", "hold_key"]
            );
            assert_eq!(workflow.steps[1].button.as_deref(), Some("middle"));
            assert_eq!(workflow.steps[2].key.as_deref(), Some(key));
            assert_eq!(workflow.steps[2].hold_seconds_arg, Some(1));
        }

        let control = workflows
            .iter()
            .find(|workflow| workflow.name == "press-control")
            .expect("missing default press-control workflow");
        assert_eq!(control.commands, ["C"]);
        assert!(!control.allow_args);
        assert_eq!(
            control
                .steps
                .iter()
                .map(|step| step.step_type.as_str())
                .collect::<Vec<_>>(),
            ["ensure_primary", "key"]
        );
        assert_eq!(control.steps[1].key.as_deref(), Some("Ctrl"));

        for (name, command, key) in [
            ("control-hold-w", "CW", "W"),
            ("control-hold-s", "CS", "S"),
            ("control-hold-a", "CA", "A"),
            ("control-hold-d", "CD", "D"),
        ] {
            let workflow = workflows
                .iter()
                .find(|workflow| workflow.name == name)
                .unwrap_or_else(|| panic!("missing default workflow {name}"));
            assert_eq!(workflow.commands, [command]);
            assert!(workflow.allow_args);
            assert_eq!(workflow.message_types, ["pink"]);
            assert_eq!(
                workflow
                    .steps
                    .iter()
                    .map(|step| step.step_type.as_str())
                    .collect::<Vec<_>>(),
                ["ensure_primary", "key", "mouse_button", "hold_key", "key"]
            );
            assert_eq!(workflow.steps[1].key.as_deref(), Some("Ctrl"));
            assert_eq!(workflow.steps[2].button.as_deref(), Some("middle"));
            assert_eq!(workflow.steps[3].key.as_deref(), Some(key));
            assert_eq!(workflow.steps[3].hold_seconds_arg, Some(1));
            assert_eq!(workflow.steps[4].key.as_deref(), Some("Ctrl"));
        }
    }

    #[test]
    fn startup_validation_rejects_an_empty_target_process() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.window.target_process = " \t ".to_string();

        let error = config
            .validate()
            .expect_err("an empty target process must fail before runtime startup");

        assert!(error.to_string().contains("window.target_process"));
    }

    #[test]
    fn startup_validation_rejects_a_zero_ai_request_timeout() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.timing.external.ai_request_timeout_ms = 0;

        let error = config
            .validate()
            .expect_err("a zero AI timeout must fail before runtime startup");

        assert!(
            error
                .to_string()
                .contains("timing.external.ai_request_timeout_ms")
        );
    }

    #[test]
    fn startup_validation_rejects_zero_runtime_intervals_timeouts_and_retries() {
        let invalid_fields: [ConfigMutation; 14] = [
            ("timing.watchdog_restart_ms", |config| {
                config.timing.watchdog_restart_ms = 0;
            }),
            ("timing.loop_idle_ms", |config| {
                config.timing.loop_idle_ms = 0;
            }),
            ("timing.chat_scan.fallback_ms", |config| {
                config.timing.chat_scan.fallback_ms = 0;
            }),
            ("timing.command.ui_timeout_ms", |config| {
                config.timing.command.ui_timeout_ms = 0;
            }),
            ("timing.workflow.default_timeout_ms", |config| {
                config.timing.workflow.default_timeout_ms = 0;
            }),
            ("timing.workflow.default_poll_ms", |config| {
                config.timing.workflow.default_poll_ms = 0;
            }),
            ("timing.hall.ocr_sample_interval_ms", |config| {
                config.timing.hall.ocr_sample_interval_ms = 0;
            }),
            ("timing.playback.status_poll_ms", |config| {
                config.timing.playback.status_poll_ms = 0;
            }),
            ("timing.playback.status_retries", |config| {
                config.timing.playback.status_retries = 0;
            }),
            ("timing.playback.monitor_tick_ms", |config| {
                config.timing.playback.monitor_tick_ms = 0;
            }),
            ("timing.decision.timeout_ms", |config| {
                config.timing.decision.timeout_ms = 0;
            }),
            ("timing.decision.poll_ms", |config| {
                config.timing.decision.poll_ms = 0;
            }),
            ("timing.external.feeluown_rpc_timeout_ms", |config| {
                config.timing.external.feeluown_rpc_timeout_ms = 0;
            }),
            ("timing.external.ai_request_timeout_ms", |config| {
                config.timing.external.ai_request_timeout_ms = 0;
            }),
        ];

        for (field, invalidate) in invalid_fields {
            let mut config: AppConfig =
                serde_yaml::from_str(bundled_config_yaml()).expect("default config");
            invalidate(&mut config);

            let error = config
                .validate()
                .expect_err("zero runtime control value must fail before startup");

            assert!(
                error.to_string().contains(field),
                "field={field} error={error}"
            );
        }
    }

    #[test]
    fn startup_validation_rejects_invalid_required_runtime_resources() {
        let invalid_fields: [ConfigMutation; 13] = [
            ("ocr.det_model", |config| {
                config.ocr.det_model = Some(PathBuf::new());
            }),
            ("ocr.backend_priority", |config| {
                config.ocr.backend_priority = vec!["metal".to_string()];
            }),
            ("templates.friend", |config| {
                config.templates.friend = PathBuf::new();
            }),
            ("feeluown.host", |config| {
                config.feeluown.host.clear();
            }),
            ("feeluown.port", |config| {
                config.feeluown.port = 0;
            }),
            ("http.port", |config| {
                config.http.port = 0;
            }),
            ("logging.dir", |config| {
                config.logging.dir = PathBuf::new();
            }),
            ("logging.level", |config| {
                config.logging.level = "verbose".to_string();
            }),
            ("tui.refresh_ms", |config| {
                config.tui.refresh_ms = 0;
            }),
            ("tui.log_lines", |config| {
                config.tui.log_lines = 0;
            }),
            ("state.queue_path", |config| {
                config.state.queue_path = PathBuf::new();
            }),
            ("hotkeys.pause_key", |config| {
                config.hotkeys.pause_key.clear();
            }),
            ("window.content_width", |config| {
                config.window.content_width -= 1;
            }),
        ];

        for (field, invalidate) in invalid_fields {
            let mut config: AppConfig =
                serde_yaml::from_str(bundled_config_yaml()).expect("default config");
            invalidate(&mut config);

            let error = config
                .validate()
                .expect_err("invalid runtime resource must fail before startup");

            assert!(
                error.to_string().contains(field),
                "field={field} error={error}"
            );
        }
    }

    #[test]
    fn startup_validation_requires_openvino_ir_paths_when_selected() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.ocr.backend_priority = vec!["openvino".to_string()];
        config.ocr.det_model = None;
        config.ocr.rec_model = None;

        let error = config
            .validate()
            .expect_err("OpenVINO selection without IR paths must fail");
        assert!(error.to_string().contains("ocr.openvino.det_model"));

        config.ocr.openvino.det_model = Some(PathBuf::from("det.xml"));
        config.ocr.openvino.det_weights = Some(PathBuf::from("det.bin"));
        config.ocr.openvino.rec_model = Some(PathBuf::from("rec.xml"));
        config.ocr.openvino.rec_weights = Some(PathBuf::from("rec.bin"));
        config
            .validate()
            .expect("complete OpenVINO IR configuration should validate");
    }

    #[test]
    fn startup_validation_requires_mnn_models_only_for_mnn_backends() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.ocr.backend_priority = vec!["openvino".to_string()];
        config.ocr.det_model = None;
        config.ocr.rec_model = None;
        config.ocr.openvino.det_model = Some(PathBuf::from("det.xml"));
        config.ocr.openvino.det_weights = Some(PathBuf::from("det.bin"));
        config.ocr.openvino.rec_model = Some(PathBuf::from("rec.xml"));
        config.ocr.openvino.rec_weights = Some(PathBuf::from("rec.bin"));
        config
            .validate()
            .expect("OpenVINO-only configuration must not require MNN models");

        config.ocr.backend_priority = vec!["openvino".to_string(), "cpu".to_string()];
        let error = config
            .validate()
            .expect_err("a mixed OpenVINO/MNN configuration must require MNN models");
        assert!(error.to_string().contains("ocr.det_model"));
    }

    #[test]
    fn startup_validation_rejects_ui_geometry_outside_the_normalized_canvas() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.invite.friend_chat_region.x = config.screen.expected_width as i32;

        let error = config
            .validate()
            .expect_err("out-of-canvas UI region must fail before startup");

        assert!(error.to_string().contains("invite.friend_chat_region"));
    }

    #[test]
    fn startup_validation_rejects_an_empty_screen_region() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.screen.chat_rect.width = 0;

        let error = config
            .validate()
            .expect_err("an empty chat region must fail before runtime startup");

        assert!(error.to_string().contains("screen.chat_rect"));
    }

    #[test]
    fn startup_validation_rejects_a_zero_startup_poll_interval() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.startup.poll_ms = 0;

        let error = config
            .validate()
            .expect_err("a zero startup poll interval must fail before runtime startup");

        assert!(error.to_string().contains("startup.poll_ms"));
    }

    #[test]
    fn startup_validation_rejects_invalid_thresholds_and_queue_capacity() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");

        config.templates.marker_threshold = 1.1;
        assert!(config.validate().is_err());

        config.templates.marker_threshold = 0.9;
        config.queue.max_size = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn startup_validation_rejects_cross_field_feature_invariants() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config");
        config.undercover.enabled = true;
        config.undercover.min_players = 10;
        config.undercover.max_players = 8;

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("undercover.min_players"));
    }

    #[test]
    fn current_config_requires_every_top_level_module_section() {
        for section in [
            "stability",
            "song_dedup",
            "idiom_chain",
            "landlord",
            "undercover",
            "turtle_soup",
            "song_review",
            "friend_delivery",
        ] {
            let mut value: serde_yaml::Value =
                serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
            value
                .as_mapping_mut()
                .expect("root mapping")
                .remove(serde_yaml::Value::String(section.to_string()));

            let error = serde_yaml::from_value::<AppConfig>(value)
                .expect_err("current top-level section must be required");

            assert!(
                error.to_string().contains(section),
                "section={section} error={error}"
            );
        }
    }

    #[test]
    fn current_config_requires_every_explicit_field() {
        for path in [
            "stability.default_count",
            "window.focus_point",
            "screen.secondary_back_rect",
            "templates.secondary_back",
            "http.access_token",
            "logging.rotate_daily",
            "logging.retain_days",
            "friend_delivery.auto_retry_count",
            "custom_workflows.wait_template_absent_stable_default",
            "custom_workflows.max_hold_key_seconds",
            "invite.friend_name_stable_count",
            "invite.friend_chat_region",
            "timing.playback.uri_stable_samples",
            "timing.playback.transport_stable_samples",
            "timing.playback.stale_timeout_ms",
            "queue.external_playback_protect_after_seconds",
            "song_dedup.enabled",
            "idiom_chain.enabled",
            "landlord.enabled",
            "undercover.enabled",
            "song_review.policy_prompt",
            "song_review.provider.extra_body",
            "ai.extra_body",
            "turtle_soup.batch_max_parts",
            "turtle_soup.ai.extra_body",
            "startup.wonderland_home_retries",
            "startup.wonderland_home_retry_ms",
            "startup.wonderland_card_retries",
            "startup.wonderland_card_retry_ms",
            "startup.wonderland_confirm_absent_timeout_ms",
            "startup.wonderland_confirm_stable_timeout_ms",
            "startup.wonderland_enter_button_threshold",
            "startup.wonderland_enter_button_region",
            "startup.templates.wonderland_enter_button",
        ] {
            let mut value: serde_yaml::Value =
                serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
            let segments = path.split('.').collect::<Vec<_>>();
            let mut parent = &mut value;
            for segment in &segments[..segments.len() - 1] {
                parent = parent
                    .as_mapping_mut()
                    .expect("configuration path mapping")
                    .get_mut(serde_yaml::Value::String((*segment).to_string()))
                    .expect("configuration path segment");
            }
            let field = segments.last().expect("configuration field");
            parent
                .as_mapping_mut()
                .expect("configuration field parent")
                .remove(serde_yaml::Value::String((*field).to_string()));

            let error = serde_yaml::from_value::<AppConfig>(value)
                .expect_err("current configuration field must be required");

            assert!(
                error.to_string().contains(field),
                "path={path} error={error}"
            );
        }
    }

    #[test]
    fn http_proxy_fields_default_when_omitted_from_existing_config() {
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
        for path in [
            "song_review.provider.http_proxy",
            "ai.http_proxy",
            "turtle_soup.ai.http_proxy",
        ] {
            let segments = path.split('.').collect::<Vec<_>>();
            let mut parent = &mut value;
            for segment in &segments[..segments.len() - 1] {
                parent = parent
                    .as_mapping_mut()
                    .expect("configuration path mapping")
                    .get_mut(serde_yaml::Value::String((*segment).to_string()))
                    .expect("configuration path segment");
            }
            let field = segments.last().expect("configuration field");
            parent
                .as_mapping_mut()
                .expect("configuration field parent")
                .remove(serde_yaml::Value::String((*field).to_string()));
        }

        let config: AppConfig = serde_yaml::from_value(value)
            .expect("proxy fields are optional for existing configurations");

        assert!(config.ai.http_proxy.is_empty());
        assert!(config.song_review.provider.http_proxy.is_empty());
        assert!(config.turtle_soup.ai.http_proxy.is_empty());
    }

    #[test]
    fn player_fast_observation_interval_stays_below_low_normal_intervals() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default app config");

        for (normal_ms, expected_fast) in [
            (300, Duration::from_millis(150)),
            (50, Duration::from_millis(25)),
            (1, Duration::from_micros(500)),
        ] {
            config.timing.playback.monitor_status_ms = normal_ms;
            let runtime = config
                .player_runtime_config()
                .expect("low intervals remain valid");
            assert_eq!(runtime.fast_observation_interval, expected_fast);
            assert!(runtime.fast_observation_interval < runtime.normal_observation_interval);
        }
    }

    #[test]
    fn stability_count_uses_local_then_global_then_builtin_default() {
        assert_eq!(resolve_stability_count(4, 3), 4);
        assert_eq!(resolve_stability_count(1, 3), 3);
        assert_eq!(resolve_stability_count(0, 3), 3);
        assert_eq!(resolve_stability_count(1, 1), 2);

        let config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("default app config");
        assert_eq!(config.stability.ui_state_count, 0);
        assert_eq!(config.stability.secondary_hall_count, 0);
        assert_eq!(
            config.resolve_stability_count(config.stability.ui_state_count),
            config.stability.default_count
        );
        assert_eq!(
            config.resolve_stability_count(config.stability.secondary_hall_count),
            config.stability.default_count
        );
    }

    #[test]
    fn removed_configuration_names_are_rejected() {
        let screen = serde_yaml::from_str::<ScreenConfig>(
            r#"
expected_width: 1920
expected_height: 1080
warn_on_size_mismatch: true
chat_rect: { x: 0, y: 0, width: 1, height: 1 }
friend_rect: { x: 0, y: 0, width: 1, height: 1 }
enter_rect: { x: 0, y: 0, width: 1, height: 1 }
secondary_back_rect: { x: 0, y: 0, width: 1, height: 1 }
secondary_hall_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_name_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_time_rect: { x: 0, y: 0, width: 1, height: 1 }
"#,
        )
        .expect_err("removed screen alias must be rejected");
        assert!(screen.to_string().contains("enter_rect"));

        let templates = serde_yaml::from_str::<TemplateConfig>(
            r#"
blue_marker: blue.png
yellow_marker: yellow.png
pink_marker: pink.png
friend: friend.png
enter: old-primary.png
secondary_back: back.png
secondary_hall: hall.png
invite_view_star: view.png
invite_goto_hall: goto.png
invite_enter_hall: invite.png
friend_panel: panel.png
friend_search_panel: search.png
friend_more_settings: more.png
friend_block_chat: block.png
friend_blacklist: blacklist.png
friend_confirm: confirm.png
marker_threshold: 0.9
"#,
        )
        .expect_err("removed template alias must be rejected");
        assert!(templates.to_string().contains("enter"));
    }
}
