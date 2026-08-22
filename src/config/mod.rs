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
use crate::features::startup::{StartupConfig, StartupTemplateConfig};
use crate::features::turtle_soup::TurtleSoupConfig;
use crate::features::undercover::UndercoverConfig;
use crate::runtime::player::PlayerObservationConfig;
use crate::runtime::player_io::{PlayerRuntimeConfig, PlayerRuntimeConfigError};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub window: WindowConfig,
    /// 游戏区域坐标；内嵌段可省略。
    #[serde(default)]
    pub screen: ScreenConfig,
    pub stability: StabilityConfig,
    pub timing: TimingConfig,
    /// OCR 配置；内嵌段可省略。
    #[serde(default)]
    pub ocr: OcrConfig,
    /// 模板图片路径；内嵌段可省略。
    #[serde(default)]
    pub templates: TemplateConfig,
    pub output: OutputConfig,
    /// 管理（拉黑/屏蔽）区域；内嵌段可省略。
    #[serde(default)]
    pub moderation: ModerationConfig,
    /// 播放器配置；内嵌段可省略。
    #[serde(default)]
    pub playback: PlaybackConfig,
    /// HTTP 段由启动引导（BootstrapConfig）提供，配置库 JSON 中可缺失。
    #[serde(default)]
    pub http: HttpConfig,
    /// 日志段由启动引导（BootstrapConfig）提供，配置库 JSON 中可缺失。
    #[serde(default)]
    pub logging: LoggingConfig,
    pub tui: TuiConfig,
    /// 状态段由启动引导（BootstrapConfig）提供，配置库 JSON 中可缺失。
    #[serde(default)]
    pub state: StateConfig,
    /// 点歌队列；内嵌段可省略。
    #[serde(default)]
    pub queue: QueueConfig,
    /// 同歌去重；内嵌段可省略。
    #[serde(default)]
    pub song_dedup: SongDedupConfig,
    /// 成语接龙；内嵌段可省略。
    #[serde(default)]
    pub idiom_chain: IdiomChainConfig,
    /// 斗地主；内嵌段可省略。
    #[serde(default)]
    pub landlord: LandlordConfig,
    /// 谁是卧底；内嵌段可省略。
    #[serde(default)]
    pub undercover: UndercoverConfig,
    /// 海龟汤；内嵌段可省略。
    #[serde(default)]
    pub turtle_soup: TurtleSoupConfig,
    /// 点歌 AI；内嵌段可省略。
    #[serde(default)]
    pub ai: AiConfig,
    /// 歌曲审核；内嵌段可省略。
    #[serde(default)]
    pub song_review: SongReviewConfig,
    /// 歌名/歌手匹配；内嵌段可省略。
    #[serde(default)]
    pub matching: MatchConfig,
    /// 全局热键；内嵌段可省略。
    #[serde(default)]
    pub hotkeys: HotkeyConfig,
    /// 启动流程；内嵌段可省略。
    #[serde(default)]
    pub startup: StartupConfig,
    /// 邀请流程区域；内嵌段可省略。
    #[serde(default)]
    pub invite: InviteConfig,
    /// 好友投递；内嵌段可省略。
    #[serde(default)]
    pub friend_delivery: FriendDeliveryConfig,
    /// 自定义流程；内嵌段可省略。
    #[serde(default)]
    pub custom_workflows: CustomWorkflowConfig,
}

impl Default for AppConfig {
    /// 默认配置必须能通过 [`AppConfig::validate`]：SQLite 配置中心会用默认值建库，
    /// 校验失败会阻断初始化。screen/ocr/templates 的默认值见各段 Default；
    /// invite/moderation/startup 的段 Default 含 0 值/空路径（位于各自功能模块），
    /// 无法通过校验，因此这里显式给出与完整配置模板一致的有效值。
    /// 区域坐标沿用 tests/fixtures/config.full.yaml 的 1920x1080 布局，
    /// 路径类默认使用发布布局（deps/models、deps/templates、deps/data）。
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            screen: ScreenConfig::default(),
            stability: StabilityConfig::default(),
            timing: TimingConfig::default(),
            ocr: OcrConfig::default(),
            templates: TemplateConfig::default(),
            output: OutputConfig::default(),
            moderation: ModerationConfig {
                stable_vote_samples: 3,
                required_vote_margin: 3,
                friend_panel_region: RectConfig {
                    x: 770,
                    y: 20,
                    width: 75,
                    height: 50,
                },
                search_panel_region: RectConfig {
                    x: 1600,
                    y: 100,
                    width: 240,
                    height: 90,
                },
                search_input_point: PointConfig::new(1180, 135),
                search_button_point: PointConfig::new(1680, 135),
                more_settings_region: RectConfig {
                    x: 1140,
                    y: 180,
                    width: 80,
                    height: 70,
                },
                block_chat_region: RectConfig {
                    x: 1240,
                    y: 130,
                    width: 440,
                    height: 505,
                },
                blacklist_region: RectConfig {
                    x: 1240,
                    y: 130,
                    width: 440,
                    height: 505,
                },
                confirm_region: RectConfig {
                    x: 900,
                    y: 700,
                    width: 500,
                    height: 100,
                },
            },
            playback: PlaybackConfig::default(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
            tui: TuiConfig::default(),
            state: StateConfig::default(),
            queue: QueueConfig::default(),
            song_dedup: SongDedupConfig::default(),
            idiom_chain: IdiomChainConfig::default(),
            landlord: LandlordConfig::default(),
            undercover: UndercoverConfig::default(),
            turtle_soup: TurtleSoupConfig::default(),
            ai: AiConfig::default(),
            song_review: SongReviewConfig::default(),
            matching: MatchConfig::default(),
            hotkeys: HotkeyConfig::default(),
            startup: StartupConfig {
                enabled: false,
                launch_game: false,
                enter_game: false,
                enter_wonderland: false,
                exe_path: PathBuf::new(),
                game_args: String::new(),
                launch_wait_ms: 5000,
                launch_retries: 12,
                enter_game_timeout_ms: 60000,
                enter_wonderland_timeout_ms: 300000,
                wonderland_map_star_retries: 120,
                wonderland_map_star_retry_ms: 2500,
                wonderland_hall_retries: 90,
                wonderland_hall_retry_ms: 2000,
                wonderland_transition_timeout_ms: 60000,
                wonderland_confirm_stable_timeout_ms: 60000,
                final_primary_timeout_ms: 120000,
                poll_ms: 1000,
                stable_mean_threshold: 2.0,
                stable_changed_ratio_threshold: 0.01,
                template_threshold: 0.9,
                wonderland_confirm_threshold: 0.9,
                templates: StartupTemplateConfig {
                    wonderland_map_star: PathBuf::from(
                        "deps/assets/startup-wonderland-map-star.png",
                    ),
                    wonderland_confirm: PathBuf::from("deps/assets/startup-wonderland-confirm.png"),
                    paimon_menu: PathBuf::from("deps/assets/startup-paimon-menu.png"),
                },
                enter_game_text_region: RectConfig {
                    x: 900,
                    y: 1000,
                    width: 130,
                    height: 40,
                },
                wonderland_hall_ocr_region: RectConfig {
                    x: 1280,
                    y: 0,
                    width: 640,
                    height: 1040,
                },
                wonderland_confirm_region: RectConfig {
                    x: 900,
                    y: 650,
                    width: 500,
                    height: 250,
                },
                main_ui_region: RectConfig {
                    x: 0,
                    y: 0,
                    width: 480,
                    height: 270,
                },
                wonderland_map_star_region: RectConfig {
                    x: 1700,
                    y: 850,
                    width: 220,
                    height: 230,
                },
            },
            invite: InviteConfig {
                friend_name_stable_count: 0,
                friend_list_region: RectConfig {
                    x: 80,
                    y: 280,
                    width: 170,
                    height: 600,
                },
                friend_chat_region: RectConfig {
                    x: 260,
                    y: 100,
                    width: 920,
                    height: 850,
                },
                view_star_region: RectConfig {
                    x: 400,
                    y: 80,
                    width: 440,
                    height: 860,
                },
                goto_hall_region: RectConfig {
                    x: 700,
                    y: 560,
                    width: 500,
                    height: 300,
                },
                enter_hall_region: RectConfig {
                    x: 700,
                    y: 700,
                    width: 500,
                    height: 100,
                },
            },
            friend_delivery: FriendDeliveryConfig::default(),
            custom_workflows: CustomWorkflowConfig::default(),
        }
    }
}

/// 最小启动配置：仅含启动阶段必需的三个字段，从 config.yaml 读取。
/// 完整业务配置（AppConfig）由 SQLite 配置中心（ConfigStore）管理，
/// http/logging/state.playback_state_path 三个注入项由此结构提供。
/// 解析严格拒绝未知段，详见 [`BootstrapConfig::load`]。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    /// 统一数据库路径（相对 EXE 根目录解析为绝对路径）。
    pub database_path: PathBuf,
    pub http: HttpConfig,
    pub logging: LoggingConfig,
}

impl BootstrapConfig {
    /// 从 config.yaml 加载最小启动配置。
    ///
    /// 严格解析（[`deny_unknown_fields`]）：只允许 database_path、http、logging 三个引导段。
    /// 字段缺失或为空时明确报错；database_path 为相对路径时相对
    /// `executable_root`（EXE 所在目录）解析为绝对路径。
    pub fn load(path: &Path, executable_root: &Path) -> Result<Self> {
        const BOOTSTRAP_ONLY_HINT: &str = "config.yaml 只允许 database_path、http、logging 三个引导段，其余配置请通过 Web 面板「配置中心」管理";
        let text = fs::read_to_string(path)
            .with_context(|| format!("读取启动配置失败: {}", path.display()))?;
        let mut bootstrap: BootstrapConfig = serde_yaml::from_str(&text).with_context(|| {
            format!(
                "解析启动配置失败: {}。{BOOTSTRAP_ONLY_HINT}",
                path.display()
            )
        })?;
        if bootstrap.database_path.as_os_str().is_empty() {
            bail!("config.yaml 的 database_path 不能为空。{BOOTSTRAP_ONLY_HINT}");
        }
        if bootstrap.database_path.is_relative() {
            bootstrap.database_path = executable_root.join(&bootstrap.database_path);
        }
        Ok(bootstrap)
    }
}

const BUILTIN_STABILITY_COUNT: u32 = 2;
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

impl Default for StabilityConfig {
    fn default() -> Self {
        Self {
            default_count: 2,
            ui_state_count: 0,
            secondary_hall_count: 0,
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
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    /// 支持逗号、分号、竖线或空白分隔的多个进程名。
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
    pub focus_point: PointConfig,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            target_process: "yuanshen.exe,GenshinImpact.exe".to_string(),
            content_width: 1920,
            content_height: 1080,
            auto_activate_window: false,
            focus_point: PointConfig::new(1919, 1000),
        }
    }
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
        self.playback.validate()?;
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
                self.startup.wonderland_hall_ocr_region,
                "startup.wonderland_hall_ocr_region",
            ),
            (
                self.startup.wonderland_confirm_region,
                "startup.wonderland_confirm_region",
            ),
            (self.startup.main_ui_region, "startup.main_ui_region"),
            (
                self.startup.wonderland_map_star_region,
                "startup.wonderland_map_star_region",
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
            PlayerRuntimeConfig::fast_observation_interval_for(normal_observation_interval);
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

    /// 仅测试：从完整 YAML 解析配置（不解析相对路径）。
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| {
            format!(
                "读取配置失败: {}。请将发布包中的 config.yaml 放在主程序 EXE 所在目录",
                path.display()
            )
        })?;
        serde_yaml::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))
    }

    /// 仅测试：从完整 YAML 加载并解析相对路径。
    #[cfg(test)]
    pub(crate) fn load_from_root(path: &Path, executable_root: &Path) -> Result<Self> {
        let mut config = Self::load(path)?;
        config.resolve_runtime_paths(executable_root);
        config.playback.normalize_audio_cache_paths(executable_root);
        Ok(config)
    }

    pub(crate) fn resolve_runtime_paths(&mut self, executable_root: &Path) {
        resolve_optional_path(executable_root, &mut self.ocr.det_model);
        resolve_optional_path(executable_root, &mut self.ocr.rec_model);
        resolve_path(executable_root, &mut self.ocr.charset);
        resolve_optional_path(executable_root, &mut self.ocr.openvino.det_model);
        resolve_optional_path(executable_root, &mut self.ocr.openvino.det_weights);
        resolve_optional_path(executable_root, &mut self.ocr.openvino.rec_model);
        resolve_optional_path(executable_root, &mut self.ocr.openvino.rec_weights);
        resolve_optional_path(executable_root, &mut self.ocr.openvino.cache_dir);

        for path in [
            &mut self.templates.blue_marker,
            &mut self.templates.yellow_marker,
            &mut self.templates.pink_marker,
            &mut self.templates.friend,
            &mut self.templates.secondary_back,
            &mut self.templates.secondary_hall,
            &mut self.templates.invite_view_star,
            &mut self.templates.invite_goto_hall,
            &mut self.templates.invite_enter_hall,
            &mut self.templates.friend_panel,
            &mut self.templates.friend_search_panel,
            &mut self.templates.friend_more_settings,
            &mut self.templates.friend_block_chat,
            &mut self.templates.friend_blacklist,
            &mut self.templates.friend_confirm,
            &mut self.playback.credential_directory,
            &mut self.playback.login_helper_executable,
            &mut self.logging.dir,
            &mut self.state.playback_state_path,
            &mut self.state.hall_state_path,
            &mut self.state.executed_commands_log_path,
            &mut self.song_dedup.history_path,
            &mut self.idiom_chain.lexicon_path,
            &mut self.startup.exe_path,
            &mut self.startup.templates.wonderland_map_star,
            &mut self.startup.templates.wonderland_confirm,
            &mut self.startup.templates.paimon_menu,
            &mut self.turtle_soup.question_bank_path,
            &mut self.turtle_soup.used_state_path,
            &mut self.undercover.word_bank_path,
            &mut self.undercover.used_state_path,
        ] {
            resolve_path(executable_root, path);
        }
        for path in self.custom_workflows.templates.values_mut() {
            resolve_path(executable_root, path);
        }
        for workflow in &mut self.custom_workflows.workflows {
            for step in &mut workflow.steps {
                let Some(template) = &mut step.template else {
                    continue;
                };
                if self
                    .custom_workflows
                    .templates
                    .contains_key(template.as_str())
                {
                    continue;
                }
                let mut path = PathBuf::from(&*template);
                resolve_path(executable_root, &mut path);
                *template = path.to_string_lossy().into_owned();
            }
        }
    }
}

fn resolve_path(root: &Path, path: &mut PathBuf) {
    if !path.as_os_str().is_empty() && path.is_relative() {
        *path = root.join(&*path);
    }
}

fn resolve_path_with_default(root: &Path, path: &mut PathBuf, default: &str) {
    if path.as_os_str().is_empty() {
        *path = PathBuf::from(default);
    }
    resolve_path(root, path);
}

fn resolve_optional_path(root: &Path, path: &mut Option<PathBuf>) {
    if let Some(path) = path {
        resolve_path(root, path);
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

/// 配置 schema 声明（阶段 4a）：Web 配置页按此生成表单。
mod schema;
pub(crate) use schema::*;

/// SQLite 配置中心：配置持久化存储（阶段 2），阶段 3 起由启动流程消费。
mod store;
pub(crate) use store::*;

/// 运行时可热更新配置的共享句柄集合（阶段 7）：保存成功后由 HTTP 层 apply，
/// 消费方在运行态读取共享值，使 schema 中标 Live 的字段真正生效。
mod live;
pub(crate) use live::*;

#[cfg(test)]
fn bundled_config_yaml() -> &'static str {
    // 完整配置模板（含全部功能段）固定在测试夹具中；
    // 仓库与发布包的 config.yaml 已精简为最小启动配置（仅 bootstrap 字段）。
    include_str!("../../tests/fixtures/config.full.yaml")
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RectConfig {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
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
    pub hall_member_count_rect: RectConfig,
    pub hall_time_rect: RectConfig,
    pub hall_member_list_rect: RectConfig,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        // 默认值与 WindowConfig 的 1920x1080 一致，区域沿用完整配置模板
        // （tests/fixtures/config.full.yaml）的坐标，保证 AppConfig::default()
        // 能通过 validate() 的尺寸一致性和画布内校验。
        Self {
            expected_width: 1920,
            expected_height: 1080,
            warn_on_size_mismatch: true,
            chat_rect: RectConfig {
                x: 39,
                y: 879,
                width: 416,
                height: 143,
            },
            friend_rect: RectConfig {
                x: 170,
                y: 1018,
                width: 50,
                height: 40,
            },
            secondary_back_rect: RectConfig {
                x: 15,
                y: 15,
                width: 65,
                height: 65,
            },
            secondary_hall_rect: RectConfig {
                x: 10,
                y: 190,
                width: 65,
                height: 55,
            },
            hall_name_rect: RectConfig {
                x: 75,
                y: 425,
                width: 325,
                height: 40,
            },
            hall_member_count_rect: RectConfig {
                x: 75,
                y: 470,
                width: 450,
                height: 50,
            },
            hall_time_rect: RectConfig {
                x: 430,
                y: 520,
                width: 110,
                height: 40,
            },
            hall_member_list_rect: RectConfig {
                x: 1280,
                y: 110,
                width: 560,
                height: 850,
            },
        }
    }
}

impl ScreenConfig {
    fn validate(&self) -> Result<()> {
        for (rect, field) in [
            (self.chat_rect, "screen.chat_rect"),
            (self.friend_rect, "screen.friend_rect"),
            (self.secondary_back_rect, "screen.secondary_back_rect"),
            (self.secondary_hall_rect, "screen.secondary_hall_rect"),
            (self.hall_name_rect, "screen.hall_name_rect"),
            (self.hall_member_count_rect, "screen.hall_member_count_rect"),
            (self.hall_time_rect, "screen.hall_time_rect"),
            (self.hall_member_list_rect, "screen.hall_member_list_rect"),
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

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            watchdog_restart_ms: 2000,
            loop_idle_ms: 60,
            chat_scan: ChatScanTimingConfig::default(),
            command: CommandTimingConfig::default(),
            input: InputTimingConfig::default(),
            workflow: WorkflowTimingConfig::default(),
            hall: HallTimingConfig::default(),
            invite: InviteTimingConfig::default(),
            moderation: ModerationTimingConfig::default(),
            playback: PlaybackTimingConfig::default(),
            decision: DecisionTimingConfig::default(),
            external: ExternalTimingConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatScanTimingConfig {
    pub fallback_ms: u64,
    pub change_debounce_ms: u64,
    pub change_cooldown_ms: u64,
}

impl Default for ChatScanTimingConfig {
    fn default() -> Self {
        Self {
            fallback_ms: 2000,
            change_debounce_ms: 120,
            change_cooldown_ms: 250,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTimingConfig {
    pub ui_timeout_ms: u64,
    pub return_retry_ms: u64,
    pub post_settle_ms: u64,
    pub help_batch_ms: u64,
}

impl Default for CommandTimingConfig {
    fn default() -> Self {
        Self {
            ui_timeout_ms: 15000,
            return_retry_ms: 1000,
            post_settle_ms: 500,
            help_batch_ms: 500,
        }
    }
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

impl Default for InputTimingConfig {
    fn default() -> Self {
        Self {
            after_activate_ms: 200,
            focus_ms: 300,
            open_chat_ms: 300,
            click_ms: 150,
            text_ms: 250,
            send_ms: 300,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HallTimingConfig {
    pub page_settle_ms: u64,
    pub ocr_sample_interval_ms: u64,
}

impl Default for HallTimingConfig {
    fn default() -> Self {
        Self {
            page_settle_ms: 800,
            ocr_sample_interval_ms: 120,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTimingConfig {
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

impl Default for DecisionTimingConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 20000,
            poll_ms: 2000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTimingConfig {
    pub volume_smooth_step_ms: u64,
    pub ai_request_timeout_ms: u64,
}

impl Default for ExternalTimingConfig {
    fn default() -> Self {
        Self {
            volume_smooth_step_ms: 300,
            ai_request_timeout_ms: 35000,
        }
    }
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
    pub request_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
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
    /// Persistent OpenVINO plugin/model cache. Set to null to disable caching.
    #[serde(default = "default_openvino_cache_dir")]
    pub cache_dir: Option<PathBuf>,
}

impl Default for OpenVinoConfig {
    fn default() -> Self {
        Self {
            det_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_det.xml")),
            det_weights: Some(PathBuf::from("deps/models/PP-OCRv6_small_det.bin")),
            rec_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_rec.xml")),
            rec_weights: Some(PathBuf::from("deps/models/PP-OCRv6_small_rec.bin")),
            device: default_openvino_device(),
            cache_dir: default_openvino_cache_dir(),
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        // CPU/MNN 后端要求 det_model/rec_model/charset 非空；
        // 默认路径使用发布布局 deps/models/，与 config.full.yaml 的文件名一致。
        Self {
            det_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_det.mnn")),
            rec_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_rec.mnn")),
            charset: PathBuf::from("deps/models/ppocr_keys_v6_small.txt"),
            min_confidence: 0.9,
            threads: 4,
            request_timeout_ms: 10_000,
            shutdown_timeout_ms: 5_000,
            backend_priority: vec!["cpu".to_string()],
            openvino: OpenVinoConfig::default(),
            det_max_side_len: 960,
            det_score_threshold: 0.3,
            det_unclip_ratio: 2.0,
            det_min_area: 9,
            det_box_border: 0,
            change_mean_threshold: 6.0,
            change_pixel_threshold: 0.03,
            text_left_gap: 8,
            block_top_padding: 2,
            block_bottom_padding: 2,
            max_block_height: 120,
            same_line_y_tolerance: 10,
            marker_dedupe_x: 8,
            marker_dedupe_y: 8,
            next_marker_min_gap: 12,
            right_padding: 4,
            batch_recognize: false,
        }
    }
}

fn default_openvino_device() -> String {
    "CPU".to_string()
}

fn default_openvino_cache_dir() -> Option<PathBuf> {
    Some(PathBuf::from("deps/data/openvino-cache"))
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
        if let Some(path) = &self.cache_dir {
            validate_nonempty_path(path, "ocr.openvino.cache_dir")?;
        }
        Ok(())
    }
}

impl OcrConfig {
    fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.request_timeout_ms, "ocr.request_timeout_ms"),
            (self.shutdown_timeout_ms, "ocr.shutdown_timeout_ms"),
        ] {
            if value == 0 {
                bail!("{} 必须大于 0", field);
            }
        }
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

impl Default for TemplateConfig {
    fn default() -> Self {
        // 模板路径默认使用发布布局 deps/assets/，文件名与
        // tests/fixtures/config.full.yaml 及 assets/ 目录一致；
        // 全部非空才能通过 validate() 的路径校验。
        Self {
            blue_marker: PathBuf::from("deps/assets/chat-marker-blue.png"),
            yellow_marker: PathBuf::from("deps/assets/chat-marker-yellow.png"),
            pink_marker: PathBuf::from("deps/assets/chat-marker-pink.png"),
            friend: PathBuf::from("deps/assets/ui-primary-friend.png"),
            secondary_back: PathBuf::from("deps/assets/ui-secondary-back.png"),
            secondary_hall: PathBuf::from("deps/assets/ui-secondary-hall.png"),
            invite_view_star: PathBuf::from("deps/assets/invite-view-star.png"),
            invite_goto_hall: PathBuf::from("deps/assets/invite-goto-hall.png"),
            invite_enter_hall: PathBuf::from("deps/assets/invite-enter-hall.png"),
            friend_panel: PathBuf::from("deps/assets/friend-panel.png"),
            friend_search_panel: PathBuf::from("deps/assets/friend-search-panel.png"),
            friend_more_settings: PathBuf::from("deps/assets/friend-more-settings.png"),
            friend_block_chat: PathBuf::from("deps/assets/friend-block-chat.png"),
            friend_blacklist: PathBuf::from("deps/assets/friend-blacklist.png"),
            friend_confirm: PathBuf::from("deps/assets/friend-confirm.png"),
            marker_threshold: 0.9,
        }
    }
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

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            send_enabled: true,
            focus_point: PointConfig::new(1919, 1000),
            chat_click_2: PointConfig::new(600, 1013),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackConfig {
    pub credential_directory: PathBuf,
    pub login_helper_executable: PathBuf,
    pub login_timeout_ms: u64,
    /// 音频数据缓存配置；缺失或 enabled=false 时不启用。
    pub audio_cache: Option<AudioCacheFileConfig>,
}

/// 音频数据缓存（本地代理 + 磁盘缓存）的文件配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCacheFileConfig {
    pub enabled: bool,
    /// 音频缓存文件目录。
    pub directory: PathBuf,
    /// 磁盘占用上限，单位 MiB。
    pub max_bytes_mb: u64,
    /// 同时进行的源站下载任务上限。
    pub max_concurrent_downloads: usize,
    /// 源站连接/响应超时，单位毫秒。
    pub request_timeout_ms: u64,
    /// 请求尚未下载完成的位置时，等待下载推进的最长时间，单位毫秒。
    pub seek_wait_timeout_ms: u64,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            credential_directory: PathBuf::from("deps/data/credentials"),
            login_helper_executable: PathBuf::from("deps/bin/miliastra-login-helper.exe"),
            login_timeout_ms: 180000,
            audio_cache: None,
        }
    }
}

impl PlaybackConfig {
    pub(crate) fn normalize_audio_cache_paths(&mut self, executable_root: &Path) {
        let Some(audio_cache) = &mut self.audio_cache else {
            return;
        };
        resolve_path_with_default(
            executable_root,
            &mut audio_cache.directory,
            "deps/cache/audio",
        );
    }

    fn validate(&self) -> Result<()> {
        if self.credential_directory.as_os_str().is_empty() {
            bail!("playback.credential_directory 不能为空");
        }
        if self.login_helper_executable.as_os_str().is_empty() {
            bail!("playback.login_helper_executable 不能为空");
        }
        if self.login_timeout_ms == 0 {
            bail!("playback.login_timeout_ms 必须大于 0");
        }
        if let Some(audio_cache) = &self.audio_cache {
            if !audio_cache.enabled {
                return Ok(());
            }
            if audio_cache.max_bytes_mb == 0 {
                bail!("playback.audio_cache.max_bytes_mb 必须大于 0");
            }
            if audio_cache.max_concurrent_downloads == 0 {
                bail!("playback.audio_cache.max_concurrent_downloads 必须大于 0");
            }
            if audio_cache.request_timeout_ms == 0 {
                bail!("playback.audio_cache.request_timeout_ms 必须大于 0");
            }
            if audio_cache.seek_wait_timeout_ms == 0 {
                bail!("playback.audio_cache.seek_wait_timeout_ms 必须大于 0");
            }
        }
        Ok(())
    }

    /// 转换成播放 crate 的运行时缓存配置；未启用时返回 None。
    /// `metadata_directory` 固定为统一数据库所在目录（state.playback_state_path
    /// 的父目录）：项目只有一个数据库，缓存元数据与配置共用 playback.sqlite3。
    pub(crate) fn audio_cache_runtime_config(
        &self,
        metadata_directory: &Path,
    ) -> Option<miliastra_playback::AudioCacheConfig> {
        let file = self.audio_cache.as_ref()?;
        if !file.enabled {
            return None;
        }
        Some(miliastra_playback::AudioCacheConfig {
            enabled: true,
            directory: file.directory.clone(),
            metadata_directory: metadata_directory.to_path_buf(),
            max_bytes: file.max_bytes_mb.saturating_mul(1024 * 1024),
            max_concurrent_downloads: file.max_concurrent_downloads,
            request_timeout: Duration::from_millis(file.request_timeout_ms),
            seek_wait_timeout: Duration::from_millis(file.seek_wait_timeout_ms),
            max_registry_entries: miliastra_playback::DEFAULT_MAX_REGISTRY_ENTRIES,
        })
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

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18888,
            enabled: true,
            access_token: String::new(),
        }
    }
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

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("deps/logs"),
            level: "info".to_string(),
            rotate_daily: true,
            retain_days: 7,
        }
    }
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

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_ms: 100,
            log_lines: 200,
        }
    }
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
    /// 统一数据库路径；由启动引导（BootstrapConfig.database_path）注入，
    /// 配置库 JSON 中不持久化，缺失时用空路径占位（注入前不参与校验）。
    #[serde(default)]
    pub playback_state_path: PathBuf,
    pub hall_state_path: PathBuf,
    pub executed_commands_log_path: PathBuf,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            playback_state_path: PathBuf::from("deps/data/playback.sqlite3"),
            hall_state_path: PathBuf::from("deps/data/hall-state.json"),
            executed_commands_log_path: PathBuf::from("deps/data/executed-commands.log"),
        }
    }
}

impl StateConfig {
    fn validate(&self) -> Result<()> {
        for (path, field) in [
            (&self.playback_state_path, "state.playback_state_path"),
            (&self.hall_state_path, "state.hall_state_path"),
            (
                &self.executed_commands_log_path,
                "state.executed_commands_log_path",
            ),
        ] {
            validate_nonempty_path(path, field)?;
        }
        if self
            .playback_state_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some("playback.sqlite3")
        {
            bail!("state.playback_state_path 文件名必须是 playback.sqlite3");
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

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_key: "F7".to_string(),
            exit_key: "F12".to_string(),
        }
    }
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
status_poll_ms: 1000
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
status_poll_ms: 1000
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
status_poll_ms: 1000
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
    fn default_config_passes_full_validation() {
        // SQLite 配置中心会用 AppConfig::default() 初始化库，默认配置必须通过完整校验。
        AppConfig::default()
            .validate()
            .expect("默认配置必须能通过完整校验");
    }

    #[test]
    fn default_config_survives_json_round_trip() {
        // 模拟 SQLite 存取路径：默认配置序列化为 serde_json::Value，
        // 再反序列化回来，结果必须与原始值相等且仍能通过校验。
        let config = AppConfig::default();
        let value = serde_json::to_value(&config).expect("序列化默认配置");
        let restored: AppConfig = serde_json::from_value(value).expect("反序列化默认配置");

        assert_eq!(
            serde_json::to_value(&restored).expect("序列化还原配置"),
            serde_json::to_value(&config).expect("序列化默认配置"),
            "JSON 往返后的配置必须与默认配置一致"
        );
        restored
            .validate()
            .expect("JSON 往返后的配置必须能通过完整校验");
    }

    /// 读取仓库根 config.yaml 并解析为最小启动配置（BootstrapConfig）。
    fn bootstrap_config_yaml() -> BootstrapConfig {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml");
        let executable_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        BootstrapConfig::load(&config_path, executable_root).expect("加载启动配置")
    }

    #[test]
    fn config_yaml_bootstrap_http_logging_match_defaults() {
        // 仓库根 config.yaml 是最小启动配置：启动时只读 database_path/http/logging
        // 三个引导字段，完整业务配置由 SQLite 配置中心以 AppConfig::default() 建库。
        // config.yaml 的 http/logging 必须与 AppConfig::default() 对应段一致，
        // 保证新库初始化与启动引导同源（http.port 等注入值与默认值不冲突）。
        let bootstrap = bootstrap_config_yaml();
        let defaults = AppConfig::default();

        assert_eq!(
            serde_yaml::to_value(&bootstrap.http).expect("序列化 http 段"),
            serde_yaml::to_value(&defaults.http).expect("序列化默认 http 段"),
            "config.yaml 的 http 段必须与 AppConfig::default() 一致"
        );
        assert_eq!(
            serde_yaml::to_value(&bootstrap.logging).expect("序列化 logging 段"),
            serde_yaml::to_value(&defaults.logging).expect("序列化默认 logging 段"),
            "config.yaml 的 logging 段必须与 AppConfig::default() 一致"
        );
    }

    #[test]
    fn runtime_paths_resolve_against_the_executable_root_and_preserve_absolute_paths() {
        let root = Path::new(r"C:\发布目录");
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("默认配置应可解析");
        config.ocr.openvino.det_model = Some(PathBuf::from("models/det.xml"));
        config.ocr.openvino.det_weights = Some(PathBuf::from("models/det.bin"));
        config.ocr.openvino.rec_model = Some(PathBuf::from("models/rec.xml"));
        config.ocr.openvino.rec_weights = Some(PathBuf::from("models/rec.bin"));
        config.startup.exe_path = PathBuf::from(r"D:\游戏\GenshinImpact.exe");
        config
            .custom_workflows
            .templates
            .insert("按钮".to_string(), PathBuf::from("custom/button.png"));
        let workflow = config
            .custom_workflows
            .workflows
            .first_mut()
            .expect("默认配置应包含自定义流程");
        workflow.steps[0].template = Some("inline/button.png".to_string());
        workflow.steps[1].template = Some("按钮".to_string());

        config.resolve_runtime_paths(root);

        for path in [
            config.ocr.det_model.as_ref().expect("检测模型"),
            config.ocr.rec_model.as_ref().expect("识别模型"),
            &config.ocr.charset,
            config
                .ocr
                .openvino
                .det_model
                .as_ref()
                .expect("OpenVINO 检测模型"),
            config
                .ocr
                .openvino
                .det_weights
                .as_ref()
                .expect("OpenVINO 检测权重"),
            config
                .ocr
                .openvino
                .rec_model
                .as_ref()
                .expect("OpenVINO 识别模型"),
            config
                .ocr
                .openvino
                .rec_weights
                .as_ref()
                .expect("OpenVINO 识别权重"),
            config
                .ocr
                .openvino
                .cache_dir
                .as_ref()
                .expect("OpenVINO 缓存"),
            &config.templates.blue_marker,
            &config.templates.friend_confirm,
            &config.playback.credential_directory,
            &config.playback.login_helper_executable,
            &config.logging.dir,
            &config.state.playback_state_path,
            &config.state.hall_state_path,
            &config.state.executed_commands_log_path,
            &config.song_dedup.history_path,
            &config.startup.templates.wonderland_map_star,
            &config.startup.templates.wonderland_confirm,
            &config.startup.templates.paimon_menu,
            &config.turtle_soup.question_bank_path,
            &config.turtle_soup.used_state_path,
            &config.undercover.word_bank_path,
            &config.undercover.used_state_path,
            config
                .custom_workflows
                .templates
                .get("按钮")
                .expect("自定义模板"),
        ] {
            assert!(
                path.is_absolute(),
                "路径未解析为绝对路径: {}",
                path.display()
            );
            assert!(
                path.starts_with(root),
                "路径未基于 EXE 根目录: {}",
                path.display()
            );
        }
        assert_eq!(
            config.startup.exe_path,
            PathBuf::from(r"D:\游戏\GenshinImpact.exe")
        );
        let workflow = &config.custom_workflows.workflows[0];
        assert_eq!(
            workflow.steps[0].template.as_deref(),
            Some(r"C:\发布目录\inline/button.png")
        );
        assert_eq!(workflow.steps[1].template.as_deref(), Some("按钮"));
    }

    #[test]
    fn empty_optional_runtime_path_stays_empty() {
        let mut config: AppConfig =
            serde_yaml::from_str(bundled_config_yaml()).expect("默认配置应可解析");
        config.startup.exe_path = PathBuf::new();

        config.resolve_runtime_paths(Path::new(r"C:\发布目录"));

        assert!(config.startup.exe_path.as_os_str().is_empty());
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
            ("timing.playback.monitor_tick_ms", |config| {
                config.timing.playback.monitor_tick_ms = 0;
            }),
            ("timing.decision.timeout_ms", |config| {
                config.timing.decision.timeout_ms = 0;
            }),
            ("timing.decision.poll_ms", |config| {
                config.timing.decision.poll_ms = 0;
            }),
            ("timing.external.ai_request_timeout_ms", |config| {
                config.timing.external.ai_request_timeout_ms = 0;
            }),
            ("ocr.request_timeout_ms", |config| {
                config.ocr.request_timeout_ms = 0;
            }),
            ("ocr.shutdown_timeout_ms", |config| {
                config.ocr.shutdown_timeout_ms = 0;
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
        let invalid_fields: [ConfigMutation; 14] = [
            ("ocr.det_model", |config| {
                config.ocr.det_model = Some(PathBuf::new());
            }),
            ("ocr.backend_priority", |config| {
                config.ocr.backend_priority = vec!["metal".to_string()];
            }),
            ("templates.friend", |config| {
                config.templates.friend = PathBuf::new();
            }),
            ("playback.credential_directory", |config| {
                config.playback.credential_directory = PathBuf::new();
            }),
            ("playback.login_helper_executable", |config| {
                config.playback.login_helper_executable = PathBuf::new();
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
            ("playback.login_timeout_ms", |config| {
                config.playback.login_timeout_ms = 0;
            }),
            ("hotkeys.pause_key", |config| {
                config.hotkeys.pause_key.clear();
            }),
            ("window.content_width", |config| {
                config.window.content_width -= 1;
            }),
            ("playback.audio_cache.max_bytes_mb", |config| {
                if let Some(audio_cache) = config.playback.audio_cache.as_mut() {
                    audio_cache.enabled = true;
                    audio_cache.max_bytes_mb = 0;
                }
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
    fn current_config_requires_core_top_level_sections() {
        // 完整模板的核心段（非可拆段）删除后解析必须失败。
        // http/logging/state 由启动引导（BootstrapConfig）提供，允许缺失（走默认值）。
        for section in ["stability", "window", "timing", "output", "tui"] {
            let mut value: serde_yaml::Value =
                serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
            value
                .as_mapping_mut()
                .expect("root mapping")
                .remove(serde_yaml::Value::String(section.to_string()));

            let error = serde_yaml::from_value::<AppConfig>(value)
                .expect_err("core top-level section must be required");

            assert!(
                error.to_string().contains(section),
                "section={section} error={error}"
            );
        }
        // 完整模板必须包含全部可拆段（供发布脚本提取）。
        let full: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
        for section in [
            "screen",
            "ocr",
            "templates",
            "moderation",
            "startup",
            "invite",
            "playback",
            "queue",
            "song_dedup",
            "song_review",
            "matching",
            "idiom_chain",
            "landlord",
            "undercover",
            "turtle_soup",
            "ai",
            "hotkeys",
            "friend_delivery",
            "custom_workflows",
        ] {
            assert!(full.get(section).is_some(), "完整模板缺少顶层段 {section}");
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
            "ocr.request_timeout_ms",
            "ocr.shutdown_timeout_ms",
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
            "startup.wonderland_map_star_retries",
            "startup.wonderland_map_star_retry_ms",
            "startup.wonderland_hall_retries",
            "startup.wonderland_hall_retry_ms",
            "startup.wonderland_transition_timeout_ms",
            "startup.wonderland_confirm_stable_timeout_ms",
            "startup.wonderland_confirm_threshold",
            "startup.wonderland_hall_ocr_region",
            "startup.wonderland_confirm_region",
            "startup.wonderland_map_star_region",
            "startup.templates.wonderland_map_star",
            "startup.templates.wonderland_confirm",
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
hall_member_count_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_time_rect: { x: 0, y: 0, width: 1, height: 1 }
hall_member_list_rect: { x: 0, y: 0, width: 1, height: 1 }
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

        let mut startup_value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("default config value");
        let startup = startup_value
            .get_mut("startup")
            .and_then(serde_yaml::Value::as_mapping_mut)
            .expect("startup mapping");
        startup.remove(serde_yaml::Value::String(
            "wonderland_map_star_retries".to_string(),
        ));
        startup.insert(
            serde_yaml::Value::String("wonderland_home_retries".to_string()),
            serde_yaml::Value::Number(120.into()),
        );
        let startup_error = serde_yaml::from_value::<AppConfig>(startup_value)
            .expect_err("removed Wonderland startup aliases must be rejected");
        assert!(
            startup_error
                .to_string()
                .contains("wonderland_home_retries")
        );
    }

    fn inline_audio_cache_mapping(config: &mut serde_yaml::Value) -> &mut serde_yaml::Mapping {
        config
            .get_mut("playback")
            .and_then(|playback| playback.get_mut("audio_cache"))
            .and_then(serde_yaml::Value::as_mapping_mut)
            .expect("内嵌音频缓存配置")
    }

    #[test]
    fn inline_audio_cache_loads_with_default_directory() {
        // bundled 完整配置中 audio_cache.directory 为空字符串：normalize 后
        // 回退到发布布局默认 deps/cache/audio。
        let directory =
            std::env::temp_dir().join(format!("config-cache-old-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("完整配置");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&value).unwrap()).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("加载配置");
        let audio_cache = config.playback.audio_cache.expect("音频缓存配置");
        assert_eq!(audio_cache.directory, directory.join("deps/cache/audio"));

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_audio_cache_rejects_metadata_directory_field() {
        // 项目只有一个数据库（deps/data/playback.sqlite3），不再允许单独配置
        // 数据库目录；deny_unknown_fields 必须拒绝旧格式的 metadata_directory。
        let directory =
            std::env::temp_dir().join(format!("config-cache-rej-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("完整配置");
        inline_audio_cache_mapping(&mut value).insert(
            serde_yaml::Value::String("metadata_directory".to_string()),
            serde_yaml::Value::String("deps/data".to_string()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&value).unwrap()).unwrap();

        let error =
            AppConfig::load_from_root(&config_path, &directory).expect_err("必须拒绝未知字段");
        assert!(
            format!("{error:#}").contains("metadata_directory"),
            "错误信息应指明未知字段 metadata_directory，实际: {error:#}"
        );

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_audio_cache_relative_paths_resolve_from_executable_root() {
        let directory =
            std::env::temp_dir().join(format!("config-cache-rel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("完整配置");
        let audio_cache = inline_audio_cache_mapping(&mut value);
        audio_cache.insert(
            serde_yaml::Value::String("directory".to_string()),
            serde_yaml::Value::String("cache/audio".to_string()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&value).unwrap()).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("加载配置");
        let audio_cache = config.playback.audio_cache.expect("音频缓存配置");
        assert_eq!(audio_cache.directory, directory.join("cache/audio"));

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_audio_cache_absolute_paths_are_preserved() {
        let directory =
            std::env::temp_dir().join(format!("config-cache-abs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let absolute_root =
            std::env::temp_dir().join(format!("audio-cache-abs-{}", uuid::Uuid::new_v4()));
        let audio_directory = absolute_root.join("audio");
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(bundled_config_yaml()).expect("完整配置");
        let audio_cache = inline_audio_cache_mapping(&mut value);
        audio_cache.insert(
            serde_yaml::Value::String("directory".to_string()),
            serde_yaml::Value::String(audio_directory.to_string_lossy().into_owned()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&value).unwrap()).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("加载配置");
        let audio_cache = config.playback.audio_cache.expect("音频缓存配置");
        assert_eq!(audio_cache.directory, audio_directory);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn default_custom_workflows_come_from_builtin_resource() {
        // 默认工作流编译进程序，新数据库开箱即用，无需发布额外配置文件。
        let config = AppConfig::default();
        assert!(config.custom_workflows.enabled);
        assert!(
            config.custom_workflows.workflows.len() >= 10,
            "内置默认工作流数量不足，实际 {} 个",
            config.custom_workflows.workflows.len()
        );
        assert!(
            config
                .custom_workflows
                .workflows
                .iter()
                .any(|workflow| workflow.name == "鼠标中键")
        );
        assert!(
            config
                .custom_workflows
                .workflows
                .iter()
                .any(|workflow| workflow.name == "notice")
        );
        // example 工作流的步骤必须全部合法（默认配置通过完整校验）。
        config.validate().expect("默认配置必须通过校验");
    }
}
