use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

use self::migration::CURRENT_CONFIG_VERSION;
use crate::features::card_games::LandlordConfig;
use crate::features::idiom_chain::IdiomChainConfig;
use crate::features::turtle_soup::TurtleSoupConfig;
use crate::features::undercover::UndercoverConfig;
use crate::runtime::player::PlayerObservationConfig;
use crate::runtime::player_io::{PlayerRuntimeConfig, PlayerRuntimeConfigError};

mod migration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub config_version: u32,
    pub window: WindowConfig,
    pub screen: ScreenConfig,
    #[serde(default)]
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
    #[serde(default)]
    pub song_dedup: SongDedupConfig,
    #[serde(default)]
    pub idiom_chain: IdiomChainConfig,
    #[serde(default)]
    pub landlord: LandlordConfig,
    #[serde(default)]
    pub undercover: UndercoverConfig,
    #[serde(default)]
    pub turtle_soup: TurtleSoupConfig,
    pub ai: AiConfig,
    #[serde(default)]
    pub song_review: SongReviewConfig,
    pub matching: MatchConfig,
    pub hotkeys: HotkeyConfig,
    pub startup: StartupConfig,
    pub invite: InviteConfig,
    pub custom_workflows: CustomWorkflowConfig,
}

const BUILTIN_STABILITY_COUNT: u32 = 2;
const PLAYER_FAST_OBSERVATION_INTERVAL: Duration = Duration::from_millis(300);
const PLAYER_OBSERVATION_COMMAND_CAPACITY: usize = 16;
const PLAYER_ACTIVE_FAST_DEMAND_CAPACITY: usize = 16;
const PLAYER_CONTROL_QUEUE_CAPACITY: usize = 16;
const PLAYER_SEARCH_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StabilityConfig {
    pub default_count: u32,
}

impl Default for StabilityConfig {
    fn default() -> Self {
        Self {
            default_count: BUILTIN_STABILITY_COUNT,
        }
    }
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
pub struct WindowConfig {
    /// 支持逗号、分号、竖线或空白分隔的多个进程名。
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
    #[serde(default = "default_window_focus_point")]
    pub focus_point: PointConfig,
}

fn default_window_focus_point() -> PointConfig {
    PointConfig::new(1919, 1000)
}

impl AppConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        self.player_runtime_config()
            .context("校验播放器运行时配置")?;
        if self.window.content_width == 0 || self.window.content_height == 0 {
            bail!("window.content_width 和 window.content_height 必须大于 0");
        }
        if self.screen.expected_width == 0 || self.screen.expected_height == 0 {
            bail!("screen.expected_width 和 screen.expected_height 必须大于 0");
        }
        if self.queue.max_size == 0 {
            bail!("queue.max_size 必须大于 0");
        }
        if self.tui.enabled && self.tui.refresh_ms == 0 {
            bail!("tui.refresh_ms 必须大于 0");
        }
        validate_unit_interval(
            self.templates.marker_threshold,
            "templates.marker_threshold",
        )?;
        validate_unit_interval(
            self.custom_workflows.default_threshold,
            "custom_workflows.default_threshold",
        )?;
        validate_unit_interval(
            self.startup.template_threshold,
            "startup.template_threshold",
        )?;
        validate_unit_interval(
            self.startup.wonderland_enter_button_threshold,
            "startup.wonderland_enter_button_threshold",
        )?;
        if self.http.enabled && self.http.host.trim().is_empty() {
            bail!("http.host 不能为空");
        }
        if self.http.enabled
            && !matches!(
                self.http.host.trim().to_ascii_lowercase().as_str(),
                "127.0.0.1" | "localhost" | "::1"
            )
            && self.http.access_token.trim().is_empty()
        {
            bail!("HTTP 监听非本机地址时必须设置 http.access_token");
        }
        if self.turtle_soup.enabled {
            if self.turtle_soup.ai.endpoint.trim().is_empty() {
                bail!("turtle_soup.ai.endpoint 未配置");
            }
            if self.turtle_soup.ai.api_key.trim().is_empty() {
                bail!("turtle_soup.ai.api_key 未配置");
            }
            if self.turtle_soup.ai.model.trim().is_empty() {
                bail!("turtle_soup.ai.model 未配置");
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

    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            if let Some(report) = migration::migrate_config_text(&text, default_config_yaml())? {
                let config: Self = serde_yaml::from_str(&report.text)
                    .with_context(|| format!("validate migrated config {}", path.display()))?;
                let backup_path = migration::backup_path(path);
                fs::write(&backup_path, &text)
                    .with_context(|| format!("write config backup {}", backup_path.display()))?;
                fs::write(path, &report.text)
                    .with_context(|| format!("write migrated config {}", path.display()))?;
                eprintln!(
                    "配置已自动迁移: {} -> version {}，备份: {}，迁移 {} 项",
                    report
                        .old_version
                        .map_or_else(|| "未标记".to_string(), |version| version.to_string()),
                    CURRENT_CONFIG_VERSION,
                    backup_path.display(),
                    report.migrated_count
                );
                if !report.unmigrated.is_empty() {
                    eprintln!(
                        "有 {} 项配置未自动迁移，已追加到配置文件末尾的注释区，不影响运行",
                        report.unmigrated.len()
                    );
                    for item in &report.unmigrated {
                        eprintln!("未迁移配置: {} ({})", item.path, item.reason);
                    }
                }
                return Ok(config);
            }
            return serde_yaml::from_str(&text)
                .with_context(|| format!("parse config {}", path.display()));
        }

        bail!(
            "配置文件不存在: {}。请将发布包中的 config.yaml 放在程序工作目录",
            path.display()
        )
    }
}

fn validate_unit_interval(value: f32, field: &str) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{} 必须是 0 到 1 之间的有限小数", field);
    }
    Ok(())
}

fn default_config_yaml() -> &'static str {
    include_str!("../../config.yaml")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RectConfig {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
pub struct ScreenConfig {
    pub expected_width: u32,
    pub expected_height: u32,
    pub warn_on_size_mismatch: bool,
    pub chat_rect: RectConfig,
    #[serde(alias = "enter_rect")]
    pub friend_rect: RectConfig,
    #[serde(default = "default_secondary_back_rect")]
    pub secondary_back_rect: RectConfig,
    pub secondary_hall_rect: RectConfig,
    pub hall_name_rect: RectConfig,
    pub hall_time_rect: RectConfig,
}

fn default_secondary_back_rect() -> RectConfig {
    RectConfig {
        x: 15,
        y: 15,
        width: 65,
        height: 65,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatScanTimingConfig {
    pub fallback_ms: u64,
    pub change_debounce_ms: u64,
    pub change_cooldown_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandTimingConfig {
    pub ui_timeout_ms: u64,
    pub return_retry_ms: u64,
    pub post_settle_ms: u64,
    pub help_batch_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputTimingConfig {
    pub after_activate_ms: u64,
    pub focus_ms: u64,
    pub open_chat_ms: u64,
    pub click_ms: u64,
    pub text_ms: u64,
    pub send_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowTimingConfig {
    pub default_timeout_ms: u64,
    pub default_poll_ms: u64,
    pub default_step_wait_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HallTimingConfig {
    pub page_settle_ms: u64,
    pub ocr_sample_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InviteTimingConfig {
    pub open_chat_ms: u64,
    pub step_ms: u64,
    pub confirm_timeout_ms: u64,
    pub confirm_poll_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModerationTimingConfig {
    pub vote_timeout_ms: u64,
    pub vote_poll_ms: u64,
    pub search_result_timeout_ms: u64,
    pub confirm_wait_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackTimingConfig {
    pub search_settle_ms: u64,
    pub status_poll_ms: u64,
    pub status_retries: u32,
    pub skip_status_initial_ms: u64,
    pub skip_status_poll_ms: u64,
    pub skip_status_retries: u32,
    pub monitor_tick_ms: u64,
    pub monitor_status_ms: u64,
    #[serde(default = "default_player_stability_samples")]
    pub uri_stable_samples: u32,
    #[serde(default = "default_player_stability_samples")]
    pub transport_stable_samples: u32,
    #[serde(
        default = "default_player_stale_timeout_ms",
        deserialize_with = "deserialize_positive_u64"
    )]
    pub stale_timeout_ms: u64,
}

fn default_player_stability_samples() -> u32 {
    0
}

fn default_player_stale_timeout_ms() -> u64 {
    5000
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("value must be a positive integer"));
    }
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionTimingConfig {
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalTimingConfig {
    pub feeluown_rpc_timeout_ms: u64,
    pub volume_smooth_step_ms: u64,
    pub ai_request_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrConfig {
    pub det_model: PathBuf,
    pub rec_model: PathBuf,
    pub charset: PathBuf,
    pub min_confidence: f32,
    pub threads: i32,
    pub backend_priority: Vec<String>,
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
pub struct TemplateConfig {
    pub blue_marker: PathBuf,
    pub yellow_marker: PathBuf,
    pub pink_marker: PathBuf,
    #[serde(alias = "enter")]
    pub friend: PathBuf,
    #[serde(default = "default_secondary_back_template")]
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

fn default_secondary_back_template() -> PathBuf {
    PathBuf::from("assets/ui-secondary-back.png")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModerationConfig {
    pub stable_vote_samples: u32,
    pub required_vote_margin: i32,
    pub friend_panel_region: RectConfig,
    pub search_panel_region: RectConfig,
    pub search_input_point: PointConfig,
    pub search_button_point: PointConfig,
    pub more_settings_region: RectConfig,
    pub block_chat_region: RectConfig,
    pub blacklist_region: RectConfig,
    pub confirm_region: RectConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputConfig {
    pub send_enabled: bool,
    pub focus_point: PointConfig,
    pub chat_click_2: PointConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeelUOwnConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    #[serde(default)]
    pub access_token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
    #[serde(default = "default_log_rotate_daily")]
    pub rotate_daily: bool,
    #[serde(default = "default_log_retain_days")]
    pub retain_days: u32,
}

fn default_log_rotate_daily() -> bool {
    true
}

fn default_log_retain_days() -> u32 {
    7
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiConfig {
    pub enabled: bool,
    pub refresh_ms: u64,
    pub log_lines: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateConfig {
    pub runtime_state_path: PathBuf,
    pub queue_path: PathBuf,
    pub executed_commands_log_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueConfig {
    pub max_size: usize,
    pub auto_advance_seconds: u64,
    pub protect_current_song_until_finished: bool,
    #[serde(default = "default_external_playback_protect_after_seconds")]
    pub external_playback_protect_after_seconds: u64,
}

fn default_external_playback_protect_after_seconds() -> u64 {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SongDedupConfig {
    pub enabled: bool,
    pub window_seconds: u64,
    pub max_count: u32,
    pub console_bypass: bool,
    pub history_path: PathBuf,
}

impl Default for SongDedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_seconds: 3600,
            max_count: 1,
            console_bypass: true,
            history_path: PathBuf::from("data/song-dedup-history.json"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomWorkflowConfig {
    pub enabled: bool,
    pub default_threshold: f32,
    #[serde(default = "default_wait_template_absent_stable")]
    pub wait_template_absent_stable_default: bool,
    #[serde(default = "default_max_hold_key_seconds")]
    pub max_hold_key_seconds: u64,
    pub templates: HashMap<String, PathBuf>,
    pub workflows: Vec<CustomWorkflowDefinition>,
}

fn default_wait_template_absent_stable() -> bool {
    true
}

fn default_max_hold_key_seconds() -> u64 {
    10
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomWorkflowDefinition {
    pub enabled: bool,
    pub name: String,
    pub commands: Vec<String>,
    pub allow_args: bool,
    pub message_types: Vec<String>,
    pub confirm_before_run: bool,
    pub confirm_message: String,
    pub confirm_message_types: Vec<String>,
    pub confirm_timeout_ms: Option<u64>,
    pub confirm_poll_ms: Option<u64>,
    pub steps: Vec<CustomWorkflowStep>,
    pub success_message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomWorkflowStep {
    #[serde(rename = "type")]
    pub step_type: String,
    pub template: Option<String>,
    pub region: Option<RectConfig>,
    pub point: Option<PointConfig>,
    pub click_offset: Option<PointConfig>,
    pub key: Option<String>,
    pub target: Option<String>,
    pub text: Option<String>,
    pub message: Option<String>,
    pub threshold: Option<f32>,
    pub timeout_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub hold_seconds_arg: Option<usize>,
    pub stable_after_absent: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub extra_body: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SongReviewConfig {
    pub enabled: bool,
    pub max_allowed_level: u8,
    pub failure_policy: SongReviewFailurePolicy,
    pub retry_count: u32,
    pub retry_delay_ms: u64,
    pub reply_reason_max_chars: usize,
    #[serde(default = "default_song_review_policy_prompt")]
    pub policy_prompt: String,
    pub custom_prompt: String,
    pub provider: SongReviewProviderConfig,
}

impl Default for SongReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_allowed_level: 4,
            failure_policy: SongReviewFailurePolicy::Reject,
            retry_count: 2,
            retry_delay_ms: 500,
            reply_reason_max_chars: 40,
            policy_prompt: default_song_review_policy_prompt(),
            custom_prompt: String::new(),
            provider: SongReviewProviderConfig::default(),
        }
    }
}

fn default_song_review_policy_prompt() -> String {
    [
        "审核目标：只通过整体听感偏舒缓、柔和、轻松、安静、治愈、抒情、慢节奏或中低强度的歌曲。",
        "拒绝明显炸场、吵闹、压迫感强、节奏过快、情绪过激、强烈电子噪音、重金属、硬核、鬼畜、洗脑循环、尖锐喊叫、强烈攻击性或明显破坏房间氛围的歌曲。",
        "请尽量使用联网搜索得到的曲风、歌词摘要、歌曲介绍和公开听感描述判断。",
        "如果信息不足，请保守判断；不确定时应给较高强度等级，而不是因为歌曲热门、用户喜欢或歌手知名就放宽标准。",
    ]
    .join("\n")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SongReviewFailurePolicy {
    Reject,
    Allow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SongReviewProviderConfig {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub extra_body: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchConfig {
    pub min_song_name_score: f64,
    pub short_chinese_song_max_miss: usize,
    pub long_chinese_song_min_score: f64,
    pub max_ocr_noise_chars: usize,
    pub enable_fuzzy_singer: bool,
    pub short_chinese_singer_max_miss: usize,
    pub long_chinese_singer_min_score: f64,
    pub en_max_edit_fraction: f64,
    pub en_singer_max_edit_fraction: f64,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            min_song_name_score: 0.5,
            short_chinese_song_max_miss: 1,
            long_chinese_song_min_score: 0.5,
            max_ocr_noise_chars: 1,
            enable_fuzzy_singer: true,
            short_chinese_singer_max_miss: 1,
            long_chinese_singer_min_score: 0.8,
            en_max_edit_fraction: 0.3,
            en_singer_max_edit_fraction: 0.35,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub pause_key: String,
    pub exit_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartupConfig {
    pub enabled: bool,
    pub launch_game: bool,
    pub enter_game: bool,
    pub enter_wonderland: bool,
    pub exe_path: PathBuf,
    pub game_args: String,
    pub launch_wait_ms: u64,
    pub launch_retries: u32,
    pub enter_game_timeout_ms: u64,
    pub enter_wonderland_timeout_ms: u64,
    #[serde(default = "default_wonderland_home_retries")]
    pub wonderland_home_retries: u32,
    #[serde(default = "default_wonderland_home_retry_ms")]
    pub wonderland_home_retry_ms: u64,
    #[serde(default = "default_wonderland_card_retries")]
    pub wonderland_card_retries: u32,
    #[serde(default = "default_wonderland_card_retry_ms")]
    pub wonderland_card_retry_ms: u64,
    #[serde(default = "default_wonderland_confirm_absent_timeout_ms")]
    pub wonderland_confirm_absent_timeout_ms: u64,
    #[serde(default = "default_wonderland_confirm_stable_timeout_ms")]
    pub wonderland_confirm_stable_timeout_ms: u64,
    pub final_primary_timeout_ms: u64,
    pub poll_ms: u64,
    pub stable_mean_threshold: f32,
    pub stable_changed_ratio_threshold: f32,
    pub template_threshold: f32,
    #[serde(default = "default_wonderland_enter_button_threshold")]
    pub wonderland_enter_button_threshold: f32,
    pub templates: StartupTemplateConfig,
    pub enter_game_text_region: RectConfig,
    #[serde(default = "default_wonderland_enter_button_region")]
    pub wonderland_enter_button_region: RectConfig,
    pub main_ui_region: RectConfig,
    pub wonderland_close_region: RectConfig,
    pub wonderland_card_point: PointConfig,
}

fn default_wonderland_home_retries() -> u32 {
    120
}

fn default_wonderland_home_retry_ms() -> u64 {
    2500
}

fn default_wonderland_card_retries() -> u32 {
    90
}

fn default_wonderland_card_retry_ms() -> u64 {
    2000
}

fn default_wonderland_confirm_absent_timeout_ms() -> u64 {
    60000
}

fn default_wonderland_confirm_stable_timeout_ms() -> u64 {
    60000
}

fn default_wonderland_enter_button_threshold() -> f32 {
    0.9
}

fn default_wonderland_enter_button_region() -> RectConfig {
    RectConfig {
        x: 1400,
        y: 850,
        width: 360,
        height: 150,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartupTemplateConfig {
    #[serde(default = "default_wonderland_enter_button_template")]
    pub wonderland_enter_button: PathBuf,
    pub paimon_menu: PathBuf,
    pub wonderland_close: PathBuf,
}

fn default_wonderland_enter_button_template() -> PathBuf {
    PathBuf::from("assets/startup-confirm-black.png")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InviteConfig {
    #[serde(default = "default_friend_name_stable_count")]
    pub friend_name_stable_count: u32,
    pub friend_list_region: RectConfig,
    #[serde(default = "default_friend_chat_region")]
    pub friend_chat_region: RectConfig,
    pub confirm_list_region: RectConfig,
    pub view_star_region: RectConfig,
    pub goto_hall_region: RectConfig,
    pub enter_hall_region: RectConfig,
}

fn default_friend_name_stable_count() -> u32 {
    0
}

fn default_friend_chat_region() -> RectConfig {
    RectConfig {
        x: 260,
        y: 100,
        width: 920,
        height: 850,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_timing(yaml: &str) -> PlaybackTimingConfig {
        serde_yaml::from_str(yaml).expect("valid playback timing config")
    }

    #[test]
    fn playback_observation_fields_default_to_global_inheritance_and_five_seconds() {
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
            serde_yaml::from_str(default_config_yaml()).expect("default app config");
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
            serde_yaml::from_str(default_config_yaml()).expect("default config");

        config.validate().expect("default config is valid");
    }

    #[test]
    fn startup_validation_rejects_invalid_thresholds_and_queue_capacity() {
        let mut config: AppConfig =
            serde_yaml::from_str(default_config_yaml()).expect("default config");

        config.templates.marker_threshold = 1.1;
        assert!(config.validate().is_err());

        config.templates.marker_threshold = 0.9;
        config.queue.max_size = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn player_fast_observation_interval_stays_below_low_normal_intervals() {
        let mut config: AppConfig =
            serde_yaml::from_str(default_config_yaml()).expect("default app config");

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
    }

    #[test]
    fn old_primary_anchor_config_names_deserialize_to_friend_names() {
        let screen: ScreenConfig = serde_yaml::from_str(
            r#"
expected_width: 1920
expected_height: 1080
warn_on_size_mismatch: true
chat_rect: { x: 0, y: 0, width: 1, height: 1 }
enter_rect: { x: 115, y: 1018, width: 50, height: 40 }
secondary_hall_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_name_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_time_rect: { x: 0, y: 0, width: 1, height: 1 }
"#,
        )
        .expect("old screen config");
        assert_eq!(screen.friend_rect.x, 115);
        let serialized_screen = serde_yaml::to_string(&screen).expect("serialize screen config");
        assert!(serialized_screen.contains("friend_rect:"));
        assert!(!serialized_screen.contains("enter_rect:"));

        let templates: TemplateConfig = serde_yaml::from_str(
            r#"
blue_marker: blue.png
yellow_marker: yellow.png
pink_marker: pink.png
enter: old-primary.png
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
        .expect("old template config");
        assert_eq!(templates.friend, PathBuf::from("old-primary.png"));
        let serialized_templates =
            serde_yaml::to_string(&templates).expect("serialize template config");
        assert!(serialized_templates.contains("friend: old-primary.png"));
        assert!(!serialized_templates.contains("enter: old-primary.png"));
    }

    #[test]
    fn old_startup_confirm_fields_do_not_override_wonderland_enter_button_defaults() {
        let startup: StartupConfig = serde_yaml::from_str(
            r#"
enabled: true
launch_game: true
enter_game: true
enter_wonderland: true
exe_path: ""
game_args: ""
launch_wait_ms: 5000
launch_retries: 12
enter_game_timeout_ms: 60000
enter_wonderland_timeout_ms: 300000
entered_wonderland_confirm_timeout_ms: 20000
final_primary_timeout_ms: 120000
poll_ms: 1000
f6_retry_ms: 2500
stable_timeout_ms: 3000
stable_mean_threshold: 2.0
stable_changed_ratio_threshold: 0.01
template_threshold: 0.8
templates:
  confirm_black: assets/old-confirm-black.png
  paimon_menu: assets/startup-paimon-menu.png
  wonderland_close: assets/startup-wonderland-close.png
enter_game_text_region:
  x: 900
  y: 1000
  width: 130
  height: 40
prompt_confirm_text_region:
  x: 1400
  y: 900
  width: 100
  height: 100
entered_wonderland_confirm_region:
  x: 1100
  y: 900
  width: 100
  height: 100
main_ui_region:
  x: 0
  y: 0
  width: 480
  height: 270
wonderland_close_region:
  x: 1780
  y: 0
  width: 140
  height: 90
wonderland_card_point:
  x: 680
  y: 310
"#,
        )
        .expect("old startup config should load with new defaults");

        assert_eq!(startup.template_threshold, 0.8);
        assert_eq!(startup.wonderland_enter_button_threshold, 0.9);
        assert_eq!(
            startup.templates.wonderland_enter_button,
            PathBuf::from("assets/startup-confirm-black.png")
        );
        assert_eq!(startup.wonderland_enter_button_region.x, 1400);
        assert_eq!(startup.wonderland_enter_button_region.y, 850);
        assert_eq!(startup.wonderland_enter_button_region.width, 360);
        assert_eq!(startup.wonderland_enter_button_region.height, 150);
    }
}
