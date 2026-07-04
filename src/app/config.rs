use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::config_migration::{self, CURRENT_CONFIG_VERSION};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub config_version: u32,
    pub window: WindowConfig,
    pub screen: ScreenConfig,
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
    pub ai: AiConfig,
    pub matching: MatchConfig,
    pub hotkeys: HotkeyConfig,
    pub invite: InviteConfig,
    pub custom_workflows: CustomWorkflowConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowConfig {
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            if let Some(report) =
                config_migration::migrate_config_text(&text, default_config_yaml())?
            {
                let config: Self = serde_yaml::from_str(&report.text)
                    .with_context(|| format!("validate migrated config {}", path.display()))?;
                let backup_path = config_migration::backup_path(path);
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
            "配置文件不存在: {}。请将发布包中的 config.yaml 放在程序工作目录，或使用 --config 指定配置文件",
            path.display()
        )
    }
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
    pub enter_rect: RectConfig,
    pub secondary_hall_rect: RectConfig,
    pub hall_name_rect: RectConfig,
    pub hall_time_rect: RectConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimingConfig {
    pub watchdog_restart_ms: u64,
    pub scan_loop_idle_ms: u64,
    pub chat_scan_fallback_ms: u64,
    pub chat_change_debounce_ms: u64,
    pub chat_change_cooldown_ms: u64,
    pub command_ui_timeout_ms: u64,
    pub return_to_primary_retry_ms: u64,
    pub output_focus_ms: u64,
    pub output_open_chat_ms: u64,
    pub output_click_ms: u64,
    pub output_input_ms: u64,
    pub output_send_ms: u64,
    pub post_command_settle_ms: u64,
    pub help_batch_ms: u64,
    pub hall_page_settle_ms: u64,
    pub hall_ocr_sample_interval_ms: u64,
    pub invite_open_chat_ms: u64,
    pub invite_step_ms: u64,
    pub invite_confirm_timeout_ms: u64,
    pub invite_confirm_poll_ms: u64,
    pub play_search_settle_ms: u64,
    pub play_status_poll_ms: u64,
    pub play_status_retries: u32,
    pub skip_status_initial_ms: u64,
    pub skip_status_poll_ms: u64,
    pub skip_status_retries: u32,
    pub decision_timeout_ms: u64,
    pub decision_poll_ms: u64,
    pub feeluown_rpc_timeout_ms: u64,
    pub volume_smooth_step_ms: u64,
    pub active_after_activate_ms: u64,
    pub ai_request_timeout_ms: u64,
    pub playback_monitor_tick_ms: u64,
    pub playback_monitor_status_ms: u64,
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
    pub enter: PathBuf,
    pub dating: PathBuf,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModerationConfig {
    pub vote_timeout_ms: u64,
    pub vote_poll_ms: u64,
    pub stable_vote_samples: u32,
    pub required_vote_margin: i32,
    pub friend_panel_region: RectConfig,
    pub search_panel_region: RectConfig,
    pub search_input_point: PointConfig,
    pub search_button_point: PointConfig,
    pub search_result_timeout_ms: u64,
    pub more_settings_region: RectConfig,
    pub block_chat_region: RectConfig,
    pub blacklist_region: RectConfig,
    pub confirm_region: RectConfig,
    pub confirm_wait_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputConfig {
    pub send_enabled: bool,
    pub focus_point: PointConfig,
    pub chat_click_1: PointConfig,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
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
    pub protect_auto_played_songs: bool,
    pub protect_current_song_until_finished: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomWorkflowConfig {
    pub enabled: bool,
    pub default_threshold: f32,
    pub default_timeout_ms: u64,
    pub default_poll_ms: u64,
    pub default_step_wait_ms: u64,
    pub templates: HashMap<String, PathBuf>,
    pub workflows: Vec<CustomWorkflowDefinition>,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
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
pub struct InviteConfig {
    pub friend_list_region: RectConfig,
    pub confirm_list_region: RectConfig,
    pub view_star_region: RectConfig,
    pub goto_hall_region: RectConfig,
    pub enter_hall_region: RectConfig,
}
