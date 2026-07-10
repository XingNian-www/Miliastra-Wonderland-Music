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
    #[serde(default)]
    pub song_dedup: SongDedupConfig,
    pub ai: AiConfig,
    #[serde(default)]
    pub song_review: SongReviewConfig,
    pub matching: MatchConfig,
    pub hotkeys: HotkeyConfig,
    pub startup: StartupConfig,
    pub invite: InviteConfig,
    pub custom_workflows: CustomWorkflowConfig,
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
    pub enter: PathBuf,
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
    pub protect_current_song_until_finished: bool,
    pub ignore_external_playback: bool,
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
    pub templates: HashMap<String, PathBuf>,
    pub workflows: Vec<CustomWorkflowDefinition>,
}

fn default_wait_template_absent_stable() -> bool {
    true
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
    pub stable_after_absent: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
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
    pub friend_list_region: RectConfig,
    pub confirm_list_region: RectConfig,
    pub view_star_region: RectConfig,
    pub goto_hall_region: RectConfig,
    pub enter_hall_region: RectConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

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
