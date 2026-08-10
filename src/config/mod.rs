use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

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
    /// 游戏区域坐标可拆到独立文件（`screen_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub screen: ScreenConfig,
    pub stability: StabilityConfig,
    pub timing: TimingConfig,
    /// OCR 配置可从主配置拆出到独立文件（`ocr_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub ocr: OcrConfig,
    /// 独立 OCR 配置文件（相对 EXE 根目录）。一般不改动的 OCR 参数可单独放
    /// 一个文件；存在时覆盖内嵌 `ocr` 段，缺失时回退内嵌段。
    #[serde(default)]
    pub ocr_config_path: Option<PathBuf>,
    /// 独立游戏区域坐标文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub screen_config_path: Option<PathBuf>,
    /// 独立模板图片路径文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub templates_config_path: Option<PathBuf>,
    /// 独立管理（拉黑/屏蔽）区域文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub moderation_config_path: Option<PathBuf>,
    /// 独立启动流程文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub startup_config_path: Option<PathBuf>,
    /// 独立邀请流程区域文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub invite_config_path: Option<PathBuf>,
    /// 独立播放器配置（凭据/登录助手/酷狗 API/音频缓存）文件（相对 EXE 根目录）。
    /// 用法同 `ocr_config_path`。
    #[serde(default)]
    pub playback_config_path: Option<PathBuf>,
    /// 独立点歌链路配置（队列/同歌去重/审核/匹配）文件（相对 EXE 根目录）。
    /// 一个文件可含 queue、song_dedup、song_review、matching 多个顶层段。
    #[serde(default)]
    pub song_config_path: Option<PathBuf>,
    /// 独立点歌 AI 配置文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub ai_config_path: Option<PathBuf>,
    /// 独立娱乐配置（成语接龙/斗地主/谁是卧底/海龟汤）文件（相对 EXE 根目录）。
    /// 一个文件可含 idiom_chain、landlord、undercover、turtle_soup 多个顶层段。
    #[serde(default)]
    pub entertainment_config_path: Option<PathBuf>,
    /// 独立全局热键配置文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub hotkeys_config_path: Option<PathBuf>,
    /// 独立好友投递配置文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub friend_delivery_config_path: Option<PathBuf>,
    /// 独立自定义流程配置文件（相对 EXE 根目录）。用法同 `ocr_config_path`。
    #[serde(default)]
    pub custom_workflows_config_path: Option<PathBuf>,
    /// 模板图片路径可拆到独立文件（`templates_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub templates: TemplateConfig,
    pub output: OutputConfig,
    /// 管理（拉黑/屏蔽）区域可拆到独立文件（`moderation_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub moderation: ModerationConfig,
    /// 播放器配置可拆到独立文件（`playback_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub playback: PlaybackConfig,
    pub http: HttpConfig,
    pub logging: LoggingConfig,
    pub tui: TuiConfig,
    pub state: StateConfig,
    /// 点歌队列可拆到独立文件（`song_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub queue: QueueConfig,
    /// 同歌去重可拆到独立文件（`song_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub song_dedup: SongDedupConfig,
    /// 成语接龙可拆到独立文件（`entertainment_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub idiom_chain: IdiomChainConfig,
    /// 斗地主可拆到独立文件（`entertainment_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub landlord: LandlordConfig,
    /// 谁是卧底可拆到独立文件（`entertainment_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub undercover: UndercoverConfig,
    /// 海龟汤可拆到独立文件（`entertainment_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub turtle_soup: TurtleSoupConfig,
    /// 点歌 AI 可拆到独立文件（`ai_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub ai: AiConfig,
    /// 歌曲审核可拆到独立文件（`song_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub song_review: SongReviewConfig,
    /// 歌名/歌手匹配可拆到独立文件（`song_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub matching: MatchConfig,
    /// 全局热键可拆到独立文件（`hotkeys_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub hotkeys: HotkeyConfig,
    /// 启动流程可拆到独立文件（`startup_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub startup: StartupConfig,
    /// 邀请流程区域可拆到独立文件（`invite_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub invite: InviteConfig,
    /// 好友投递可拆到独立文件（`friend_delivery_config_path`）；内嵌段可省略。
    #[serde(default)]
    pub friend_delivery: FriendDeliveryConfig,
    /// 自定义流程可拆到独立文件（`custom_workflows_config_path`）；内嵌段可省略。
    #[serde(default)]
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
                "读取配置失败: {}。请将发布包中的 config.yaml 放在主程序 EXE 所在目录",
                path.display()
            )
        })?;
        serde_yaml::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))
    }

    pub(crate) fn load_from_root(path: &Path, executable_root: &Path) -> Result<Self> {
        let mut config = Self::load(path)?;
        config.resolve_runtime_paths(executable_root);
        config.load_external_ocr(executable_root)?;
        config.load_external_screen(executable_root)?;
        config.load_external_templates(executable_root)?;
        config.load_external_moderation(executable_root)?;
        config.load_external_startup(executable_root)?;
        config.load_external_invite(executable_root)?;
        config.load_external_playback(executable_root)?;
        config.load_external_song(executable_root)?;
        config.load_external_ai(executable_root)?;
        config.load_external_entertainment(executable_root)?;
        config.load_external_hotkeys(executable_root)?;
        config.load_external_friend_delivery(executable_root)?;
        config.load_external_custom_workflows(executable_root)?;
        Ok(config)
    }

    fn resolve_runtime_paths(&mut self, executable_root: &Path) {
        resolve_optional_path(executable_root, &mut self.ocr_config_path);
        resolve_optional_path(executable_root, &mut self.screen_config_path);
        resolve_optional_path(executable_root, &mut self.templates_config_path);
        resolve_optional_path(executable_root, &mut self.moderation_config_path);
        resolve_optional_path(executable_root, &mut self.startup_config_path);
        resolve_optional_path(executable_root, &mut self.invite_config_path);
        resolve_optional_path(executable_root, &mut self.playback_config_path);
        resolve_optional_path(executable_root, &mut self.song_config_path);
        resolve_optional_path(executable_root, &mut self.ai_config_path);
        resolve_optional_path(executable_root, &mut self.entertainment_config_path);
        resolve_optional_path(executable_root, &mut self.hotkeys_config_path);
        resolve_optional_path(executable_root, &mut self.friend_delivery_config_path);
        resolve_optional_path(executable_root, &mut self.custom_workflows_config_path);
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
            &mut self.playback.kugou_api_executable,
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

    /// 加载独立 OCR 配置文件（`ocr_config_path` 指向的外部文件）。
    /// 以当前（内嵌）OCR 配置为基础，外部文件只写需要覆盖的字段；
    /// 文件不存在时静默回退内嵌配置。
    fn load_external_ocr(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.ocr_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取 OCR 配置文件失败: {}", path.display()))?;
        let overlay: serde_yaml::Value = serde_yaml::from_str(&text)
            .with_context(|| format!("解析 OCR 配置文件失败: {}", path.display()))?;
        let mut merged = serde_yaml::to_value(&self.ocr)
            .map_err(|error| anyhow::anyhow!("序列化内嵌 OCR 配置失败: {error}"))?;
        merge_yaml_mapping(&mut merged, overlay)?;
        let mut external: OcrConfig = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并 OCR 配置无效: {error}"))?;
        // 外部文件里的路径同样相对 EXE 根目录解析。
        resolve_optional_path(executable_root, &mut external.det_model);
        resolve_optional_path(executable_root, &mut external.rec_model);
        resolve_path(executable_root, &mut external.charset);
        resolve_optional_path(executable_root, &mut external.openvino.det_model);
        resolve_optional_path(executable_root, &mut external.openvino.det_weights);
        resolve_optional_path(executable_root, &mut external.openvino.rec_model);
        resolve_optional_path(executable_root, &mut external.openvino.rec_weights);
        resolve_optional_path(executable_root, &mut external.openvino.cache_dir);
        self.ocr = external;
        Ok(())
    }

    /// 加载独立游戏区域坐标文件（`screen_config_path` 指向的外部文件）。
    /// 以当前（内嵌）配置为基础，外部文件只写需要覆盖的字段；
    /// 文件不存在时静默回退内嵌配置。
    fn load_external_screen(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.screen_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取区域坐标配置文件失败: {}", path.display()))?;
        let overlay: serde_yaml::Value = serde_yaml::from_str(&text)
            .with_context(|| format!("解析区域坐标配置文件失败: {}", path.display()))?;
        let mut merged = serde_yaml::to_value(&self.screen)
            .map_err(|error| anyhow::anyhow!("序列化内嵌区域坐标配置失败: {error}"))?;
        merge_yaml_mapping(&mut merged, overlay)?;
        self.screen = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并区域坐标配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立模板图片路径文件（`templates_config_path`）。
    fn load_external_templates(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.templates_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.templates, "模板")?;
        let mut external: TemplateConfig = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并模板配置无效: {error}"))?;
        for path in [
            &mut external.blue_marker,
            &mut external.yellow_marker,
            &mut external.pink_marker,
            &mut external.friend,
            &mut external.secondary_back,
            &mut external.secondary_hall,
            &mut external.invite_view_star,
            &mut external.invite_goto_hall,
            &mut external.invite_enter_hall,
            &mut external.friend_panel,
            &mut external.friend_search_panel,
            &mut external.friend_more_settings,
            &mut external.friend_block_chat,
            &mut external.friend_blacklist,
            &mut external.friend_confirm,
        ] {
            resolve_path(executable_root, path);
        }
        self.templates = external;
        Ok(())
    }

    /// 加载独立管理（拉黑/屏蔽）区域文件（`moderation_config_path`）。
    fn load_external_moderation(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.moderation_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.moderation, "管理区域")?;
        self.moderation = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并管理区域配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立启动流程文件（`startup_config_path`）。
    fn load_external_startup(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.startup_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.startup, "启动流程")?;
        let mut external: StartupConfig = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并启动流程配置无效: {error}"))?;
        resolve_path(executable_root, &mut external.exe_path);
        resolve_path(executable_root, &mut external.templates.wonderland_map_star);
        resolve_path(executable_root, &mut external.templates.wonderland_confirm);
        resolve_path(executable_root, &mut external.templates.paimon_menu);
        self.startup = external;
        Ok(())
    }

    /// 加载独立邀请流程区域文件（`invite_config_path`）。
    fn load_external_invite(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.invite_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.invite, "邀请区域")?;
        self.invite = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并邀请区域配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立播放器配置文件（`playback_config_path`）。
    fn load_external_playback(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.playback_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.playback, "播放器")?;
        let mut external: PlaybackConfig = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并播放器配置无效: {error}"))?;
        resolve_path(executable_root, &mut external.credential_directory);
        resolve_path(executable_root, &mut external.login_helper_executable);
        resolve_path(executable_root, &mut external.kugou_api_executable);
        if let Some(audio_cache) = &mut external.audio_cache {
            resolve_path(executable_root, &mut audio_cache.directory);
        }
        self.playback = external;
        Ok(())
    }

    /// 加载独立点歌链路配置文件（`song_config_path`）；文件可含
    /// queue、song_dedup、song_review、matching 多个顶层段，缺失的段保持内嵌。
    fn load_external_song(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.song_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let sections = read_external_overlay_sections(
            &path,
            &[
                ("queue", serde_yaml::to_value(&self.queue)?),
                ("song_dedup", serde_yaml::to_value(&self.song_dedup)?),
                ("song_review", serde_yaml::to_value(&self.song_review)?),
                ("matching", serde_yaml::to_value(&self.matching)?),
            ],
            "点歌",
        )?;
        for (name, merged) in sections {
            match name.as_str() {
                "queue" => {
                    self.queue = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并点歌队列配置无效: {error}"))?;
                }
                "song_dedup" => {
                    let mut external: SongDedupConfig = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并同歌去重配置无效: {error}"))?;
                    resolve_path(executable_root, &mut external.history_path);
                    self.song_dedup = external;
                }
                "song_review" => {
                    self.song_review = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并歌曲审核配置无效: {error}"))?;
                }
                "matching" => {
                    self.matching = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并歌曲匹配配置无效: {error}"))?;
                }
                _ => unreachable!("read_external_overlay_sections 只返回请求的段"),
            }
        }
        Ok(())
    }

    /// 加载独立点歌 AI 配置文件（`ai_config_path`）。
    fn load_external_ai(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.ai_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.ai, "点歌 AI")?;
        self.ai = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并点歌 AI 配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立娱乐配置文件（`entertainment_config_path`）；文件可含
    /// idiom_chain、landlord、undercover、turtle_soup 多个顶层段，缺失的段保持内嵌。
    fn load_external_entertainment(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.entertainment_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let sections = read_external_overlay_sections(
            &path,
            &[
                ("idiom_chain", serde_yaml::to_value(&self.idiom_chain)?),
                ("landlord", serde_yaml::to_value(&self.landlord)?),
                ("undercover", serde_yaml::to_value(&self.undercover)?),
                ("turtle_soup", serde_yaml::to_value(&self.turtle_soup)?),
            ],
            "娱乐",
        )?;
        for (name, merged) in sections {
            match name.as_str() {
                "idiom_chain" => {
                    let mut external: IdiomChainConfig = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并成语接龙配置无效: {error}"))?;
                    resolve_path(executable_root, &mut external.lexicon_path);
                    self.idiom_chain = external;
                }
                "landlord" => {
                    self.landlord = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并斗地主配置无效: {error}"))?;
                }
                "undercover" => {
                    let mut external: UndercoverConfig = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并谁是卧底配置无效: {error}"))?;
                    resolve_path(executable_root, &mut external.word_bank_path);
                    resolve_path(executable_root, &mut external.used_state_path);
                    self.undercover = external;
                }
                "turtle_soup" => {
                    let mut external: TurtleSoupConfig = serde_yaml::from_value(merged)
                        .map_err(|error| anyhow::anyhow!("合并海龟汤配置无效: {error}"))?;
                    resolve_path(executable_root, &mut external.question_bank_path);
                    resolve_path(executable_root, &mut external.used_state_path);
                    self.turtle_soup = external;
                }
                _ => unreachable!("read_external_overlay_sections 只返回请求的段"),
            }
        }
        Ok(())
    }

    /// 加载独立全局热键配置文件（`hotkeys_config_path`）。
    fn load_external_hotkeys(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.hotkeys_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.hotkeys, "全局热键")?;
        self.hotkeys = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并全局热键配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立好友投递配置文件（`friend_delivery_config_path`）。
    fn load_external_friend_delivery(&mut self, _executable_root: &Path) -> Result<()> {
        let Some(path) = self.friend_delivery_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.friend_delivery, "好友投递")?;
        self.friend_delivery = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并好友投递配置无效: {error}"))?;
        Ok(())
    }

    /// 加载独立自定义流程配置文件（`custom_workflows_config_path`）。
    fn load_external_custom_workflows(&mut self, executable_root: &Path) -> Result<()> {
        let Some(path) = self.custom_workflows_config_path.clone() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let merged = read_external_overlay(&path, &self.custom_workflows, "自定义流程")?;
        let mut external: CustomWorkflowConfig = serde_yaml::from_value(merged)
            .map_err(|error| anyhow::anyhow!("合并自定义流程配置无效: {error}"))?;
        for path in external.templates.values_mut() {
            resolve_path(executable_root, path);
        }
        for workflow in &mut external.workflows {
            for step in &mut workflow.steps {
                let Some(template) = &mut step.template else {
                    continue;
                };
                if external.templates.contains_key(template.as_str()) {
                    continue;
                }
                let mut path = PathBuf::from(&*template);
                resolve_path(executable_root, &mut path);
                *template = path.to_string_lossy().into_owned();
            }
        }
        self.custom_workflows = external;
        Ok(())
    }
}

/// 读取外部配置文件，与内嵌配置合并（外部字段覆盖，同名嵌套映射递归合并）。
fn read_external_overlay(
    path: &std::path::Path,
    inline: &impl serde::Serialize,
    context: &str,
) -> Result<serde_yaml::Value> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取{context}配置文件失败: {}", path.display()))?;
    let overlay: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("解析{context}配置文件失败: {}", path.display()))?;
    let mut merged = serde_yaml::to_value(inline)
        .map_err(|error| anyhow::anyhow!("序列化内嵌{context}配置失败: {error}"))?;
    merge_yaml_mapping(&mut merged, overlay)?;
    Ok(merged)
}

/// 读取外部配置文件中的若干顶层段，逐段与内嵌配置合并（外部字段覆盖）。
/// 文件中缺失的段保持内嵌；文件顶层必须是映射。
fn read_external_overlay_sections(
    path: &std::path::Path,
    inline: &[(&str, serde_yaml::Value)],
    context: &str,
) -> Result<Vec<(String, serde_yaml::Value)>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取{context}配置文件失败: {}", path.display()))?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("解析{context}配置文件失败: {}", path.display()))?;
    let Some(mapping) = doc.as_mapping() else {
        bail!("{context}配置文件顶层必须是映射");
    };
    let mut merged_sections = Vec::new();
    for (name, value) in inline {
        let Some(overlay) = mapping.get(serde_yaml::Value::String(name.to_string())) else {
            continue;
        };
        let mut merged = value.clone();
        merge_yaml_mapping(&mut merged, overlay.clone())?;
        merged_sections.push((name.to_string(), merged));
    }
    Ok(merged_sections)
}

/// 将 `overlay` 映射合并进 `base` 映射；同名嵌套映射递归合并，其余字段覆盖。
fn merge_yaml_mapping(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) -> Result<()> {
    let Some(base_map) = base.as_mapping_mut() else {
        bail!("内嵌 OCR 配置结构异常");
    };
    let Some(overlay_map) = overlay.as_mapping() else {
        bail!("OCR 配置文件必须是键值映射");
    };
    for (key, value) in overlay_map {
        if value.is_mapping() && base_map.get(key).is_some_and(serde_yaml::Value::is_mapping) {
            let nested = base_map.get_mut(key).expect("checked mapping key");
            merge_yaml_mapping(nested, value.clone())?;
        } else {
            base_map.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn resolve_path(root: &Path, path: &mut PathBuf) {
    if !path.as_os_str().is_empty() && path.is_relative() {
        *path = root.join(&*path);
    }
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

#[cfg(test)]
fn bundled_config_yaml() -> &'static str {
    // 完整配置模板（含全部功能段）固定在测试夹具中；
    // 仓库与发布包的 config.yaml 是精简版（核心段 + 外部文件引用）。
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
        let rect = RectConfig {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        Self {
            expected_width: 0,
            expected_height: 0,
            warn_on_size_mismatch: true,
            chat_rect: rect,
            friend_rect: rect,
            secondary_back_rect: rect,
            secondary_hall_rect: rect,
            hall_name_rect: rect,
            hall_member_count_rect: rect,
            hall_time_rect: rect,
            hall_member_list_rect: rect,
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
            det_model: None,
            det_weights: None,
            rec_model: None,
            rec_weights: None,
            device: default_openvino_device(),
            cache_dir: default_openvino_cache_dir(),
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            det_model: None,
            rec_model: None,
            charset: PathBuf::new(),
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
        Self {
            blue_marker: PathBuf::new(),
            yellow_marker: PathBuf::new(),
            pink_marker: PathBuf::new(),
            friend: PathBuf::new(),
            secondary_back: PathBuf::new(),
            secondary_hall: PathBuf::new(),
            invite_view_star: PathBuf::new(),
            invite_goto_hall: PathBuf::new(),
            invite_enter_hall: PathBuf::new(),
            friend_panel: PathBuf::new(),
            friend_search_panel: PathBuf::new(),
            friend_more_settings: PathBuf::new(),
            friend_block_chat: PathBuf::new(),
            friend_blacklist: PathBuf::new(),
            friend_confirm: PathBuf::new(),
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackConfig {
    pub credential_directory: PathBuf,
    pub login_helper_executable: PathBuf,
    pub kugou_api_executable: PathBuf,
    pub login_timeout_ms: u64,
    pub kugou_api_base_url: String,
    /// 音频数据缓存配置；缺失或 enabled=false 时不启用。
    pub audio_cache: Option<AudioCacheFileConfig>,
}

/// 音频数据缓存（本地代理 + 磁盘缓存）的文件配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCacheFileConfig {
    pub enabled: bool,
    /// 缓存根目录；为空时使用系统临时目录。
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
            login_helper_executable: PathBuf::from("miliastra-login-helper.exe"),
            kugou_api_executable: PathBuf::from("kugou-api.exe"),
            login_timeout_ms: 180000,
            kugou_api_base_url: "http://127.0.0.1:3000".to_string(),
            audio_cache: None,
        }
    }
}

impl PlaybackConfig {
    fn validate(&self) -> Result<()> {
        if self.credential_directory.as_os_str().is_empty() {
            bail!("playback.credential_directory 不能为空");
        }
        if self.login_helper_executable.as_os_str().is_empty() {
            bail!("playback.login_helper_executable 不能为空");
        }
        if self.kugou_api_executable.as_os_str().is_empty() {
            bail!("playback.kugou_api_executable 不能为空");
        }
        if self.login_timeout_ms == 0 {
            bail!("playback.login_timeout_ms 必须大于 0");
        }
        let base_url = self.kugou_api_base_url.trim();
        if base_url.is_empty() {
            bail!("playback.kugou_api_base_url 不能为空");
        }
        let url =
            Url::parse(base_url).with_context(|| "playback.kugou_api_base_url 必须是有效 URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("playback.kugou_api_base_url 必须使用 http 或 https URL");
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
    pub(crate) fn audio_cache_runtime_config(
        &self,
    ) -> Option<miliastra_playback::AudioCacheConfig> {
        let file = self.audio_cache.as_ref()?;
        if !file.enabled {
            return None;
        }
        Some(miliastra_playback::AudioCacheConfig {
            enabled: true,
            directory: file.directory.clone(),
            max_bytes: file.max_bytes_mb.saturating_mul(1024 * 1024),
            max_concurrent_downloads: file.max_concurrent_downloads,
            request_timeout: Duration::from_millis(file.request_timeout_ms),
            seek_wait_timeout: Duration::from_millis(file.seek_wait_timeout_ms),
            max_registry_entries: 16,
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
    pub executed_commands_log_path: PathBuf,
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
            &config.playback.kugou_api_executable,
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
        let invalid_fields: [ConfigMutation; 16] = [
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
            ("playback.kugou_api_executable", |config| {
                config.playback.kugou_api_executable = PathBuf::new();
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
            ("playback.kugou_api_base_url", |config| {
                config.playback.kugou_api_base_url.clear();
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
        // 精简 config.yaml 只保留核心段；这些段删掉后解析必须失败。
        for section in [
            "stability",
            "window",
            "timing",
            "output",
            "http",
            "logging",
            "state",
        ] {
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

    #[test]
    fn external_ocr_config_file_overrides_inline_section_and_resolves_paths() {
        let directory = std::env::temp_dir().join(format!("config-ocr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        // 主配置：ocr_config_path 指向外部文件；内嵌 ocr 段保留为回退。
        let mut main_config = bundled_config_yaml().to_string();
        main_config.push_str("ocr_config_path: ocr.yaml\n");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 外部 OCR 配置：模型路径改写为可辨识值。文件顶层即 OcrConfig。
        let ocr_yaml = r#"
det_model: models/external-det.mnn
rec_model: models/external-rec.mnn
charset: models/external-chars.txt
det_max_side_len: 1280
"#;
        std::fs::write(directory.join("ocr.yaml"), ocr_yaml).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        let expected_det = directory.join("models/external-det.mnn");
        let expected_rec = directory.join("models/external-rec.mnn");
        assert_eq!(
            config.ocr.det_model.as_deref(),
            Some(expected_det.as_path())
        );
        assert_eq!(
            config.ocr.rec_model.as_deref(),
            Some(expected_rec.as_path())
        );
        assert_eq!(
            config.ocr.charset,
            directory.join("models/external-chars.txt")
        );
        assert_eq!(config.ocr.det_max_side_len, 1280);
        // 未覆盖的字段继承内嵌配置。
        assert_eq!(config.ocr.threads, 4);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn missing_external_ocr_config_falls_back_to_inline_section() {
        let directory = std::env::temp_dir().join(format!("config-ocr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut main_config = bundled_config_yaml().to_string();
        main_config.push_str("ocr_config_path: does-not-exist.yaml\n");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        let expected = directory.join("models/PP-OCRv6_small_det.mnn");
        assert_eq!(config.ocr.det_model.as_deref(), Some(expected.as_path()));

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_ocr_section_is_optional_when_external_file_provides_full_config() {
        let directory = std::env::temp_dir().join(format!("config-ocr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        // 发布形态：主配置不含 ocr 段，只引用外部文件。
        let bundled = bundled_config_yaml();
        let (ocr_start, ocr_end) = ocr_section_span(bundled).expect("bundled ocr section");
        let without_ocr = format!("{}{}", &bundled[..ocr_start], &bundled[ocr_end..]);
        let main_config = format!("{without_ocr}ocr_config_path: ocr.yaml\n");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 外部文件为完整 OCR 配置（同发布包 deps/ocr.yaml）。
        let ocr_section = bundled[ocr_start + 4..ocr_end]
            .trim_start_matches('\n')
            .to_string();
        std::fs::write(directory.join("ocr.yaml"), ocr_section).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        let expected = directory.join("models/PP-OCRv6_small_det.mnn");
        assert_eq!(config.ocr.det_model.as_deref(), Some(expected.as_path()));
        assert_eq!(config.ocr.threads, 4);
        assert_eq!(config.ocr.backend_priority, vec!["cpu".to_string()]);
        assert_eq!(config.ocr.det_max_side_len, 960);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// 返回内嵌 `ocr:` 段（含 `templates:` 行）在配置文本中的区间。
    fn ocr_section_span(source: &str) -> Option<(usize, usize)> {
        let start = source.find("\nocr:")? + 1;
        let tail = &source[start + 4..];
        let templates = tail.find("\ntemplates:")?;
        Some((start, start + 4 + templates + 1))
    }

    #[test]
    fn external_screen_config_overrides_inline_section() {
        let directory = std::env::temp_dir().join(format!("config-scr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let mut main_config = bundled_config_yaml().to_string();
        main_config.push_str("screen_config_path: screen.yaml\n");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 外部文件只覆盖聊天区域，其余继承内嵌配置。
        let screen_yaml = r#"
chat_rect:
  x: 100
  y: 200
  width: 300
  height: 400
"#;
        std::fs::write(directory.join("screen.yaml"), screen_yaml).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(config.screen.chat_rect.x, 100);
        assert_eq!(config.screen.chat_rect.y, 200);
        assert_eq!(config.screen.chat_rect.width, 300);
        assert_eq!(config.screen.chat_rect.height, 400);
        // 未覆盖字段继承内嵌配置。
        assert_eq!(config.screen.expected_width, 1920);
        assert_eq!(config.screen.hall_name_rect.x, 75);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_screen_section_is_optional_when_external_file_provides_full_config() {
        let directory = std::env::temp_dir().join(format!("config-scr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let bundled = bundled_config_yaml();
        let (ocr_start, ocr_end) = ocr_section_span(bundled).expect("bundled ocr section");
        let without_ocr_and_screen = format!("{}{}", &bundled[..ocr_start], &bundled[ocr_end..]);
        let main_config = format!("{without_ocr_and_screen}screen_config_path: screen.yaml\n");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 完整 screen 配置（同发布包 deps/screen.yaml）。
        let screen_start = without_ocr_and_screen
            .find("\nscreen:")
            .expect("bundled screen section")
            + 1;
        let screen_tail = &without_ocr_and_screen[screen_start + 7..];
        let screen_end =
            screen_tail.find("\nstability:").expect("stability follows") + screen_start + 7;
        // 跳过 `screen:` 包装行：外部文件顶层即 ScreenConfig。
        let screen_section = without_ocr_and_screen[screen_start + 7..screen_end].to_string();
        std::fs::write(directory.join("screen.yaml"), screen_section).unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(config.screen.expected_width, 1920);
        assert_eq!(config.screen.chat_rect.x, 39);
        assert_eq!(config.screen.hall_member_list_rect.width, 560);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn external_section_configs_override_inline_sections() {
        let directory = std::env::temp_dir().join(format!("config-sec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let mut main_config = bundled_config_yaml().to_string();
        main_config.push_str(
            "templates_config_path: templates.yaml\n\
             moderation_config_path: moderation.yaml\n\
             startup_config_path: startup.yaml\n\
             invite_config_path: invite.yaml\n",
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 每个外部文件只覆盖一个可辨识字段，其余继承内嵌配置。
        std::fs::write(directory.join("templates.yaml"), "marker_threshold: 0.8\n").unwrap();
        std::fs::write(
            directory.join("moderation.yaml"),
            "friend_panel_region:\n  x: 100\n  y: 200\n  width: 300\n  height: 400\n",
        )
        .unwrap();
        std::fs::write(directory.join("startup.yaml"), "enabled: true\n").unwrap();
        std::fs::write(
            directory.join("invite.yaml"),
            "friend_list_region:\n  x: 11\n  y: 22\n  width: 33\n  height: 44\n",
        )
        .unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(config.templates.marker_threshold, 0.8);
        assert_eq!(
            config.templates.blue_marker,
            directory.join("assets/chat-marker-blue.png")
        );
        assert_eq!(config.moderation.friend_panel_region.x, 100);
        assert_eq!(config.moderation.confirm_region.x, 900);
        assert!(config.startup.enabled);
        assert_eq!(config.startup.poll_ms, 1000);
        assert_eq!(config.invite.friend_list_region.x, 11);
        assert_eq!(config.invite.enter_hall_region.width, 500);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn inline_sections_optional_when_external_files_provide_full_configs() {
        let directory = std::env::temp_dir().join(format!("config-sec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let bundled = bundled_config_yaml();
        // 去掉 templates/moderation/startup/invite 四个内嵌段（发布形态）。
        let mut text = bundled.to_string();
        for (section, next_section) in [
            ("templates:", "output:"),
            ("moderation:", "playback:"),
            ("startup:", "invite:"),
            ("invite:", "friend_delivery:"),
        ] {
            let start = text.find(&format!("\n{section}")).expect(section) + 1;
            let tail = &text[start..];
            let next = tail.find(&format!("\n{next_section}")).expect(next_section);
            text = format!("{}{}", &text[..start], &tail[next..]);
        }
        let main_config = format!(
            "{text}ocr_config_path: ocr.yaml\n\
             screen_config_path: screen.yaml\n\
             templates_config_path: templates.yaml\n\
             moderation_config_path: moderation.yaml\n\
             startup_config_path: startup.yaml\n\
             invite_config_path: invite.yaml\n"
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 四个外部文件用完整内嵌段内容（顶层去缩进）。
        for (section, next_section, file) in [
            ("templates:", "output:", "templates.yaml"),
            ("moderation:", "playback:", "moderation.yaml"),
            ("startup:", "invite:", "startup.yaml"),
            ("invite:", "friend_delivery:", "invite.yaml"),
        ] {
            let start = bundled.find(&format!("\n{section}")).expect(section) + 1 + section.len();
            let tail = &bundled[start..];
            let next = tail.find(&format!("\n{next_section}")).expect(next_section);
            let content = tail[..next]
                .lines()
                .map(|line| {
                    if let Some(rest) = line.strip_prefix("  ") {
                        rest
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(directory.join(file), content).unwrap();
        }

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(
            config.templates.blue_marker,
            directory.join("assets/chat-marker-blue.png")
        );
        assert_eq!(config.moderation.friend_panel_region.x, 770);
        assert!(!config.startup.enabled);
        assert_eq!(config.startup.enter_game_text_region.x, 900);
        assert_eq!(config.invite.friend_list_region.width, 170);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// 从完整模板提取一段的完整内容并去掉顶层 2 空格缩进。
    fn extract_section_dedented(source: &str, section: &str, next_section: &str) -> String {
        let start = source.find(&format!("\n{section}")).expect(section) + 1 + section.len();
        let tail = &source[start..];
        let next = tail.find(&format!("\n{next_section}")).expect(next_section);
        tail[..next]
            .lines()
            .map(|line| {
                if let Some(rest) = line.strip_prefix("  ") {
                    rest
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 从完整模板提取一段的完整内容（保留原缩进，供多段文件使用）。
    fn extract_section_as_is(source: &str, section: &str, next_section: &str) -> String {
        let start = source.find(&format!("\n{section}")).expect(section) + 1 + section.len();
        let tail = &source[start..];
        let next = tail.find(&format!("\n{next_section}")).expect(next_section);
        tail[..next].trim_end().to_string()
    }

    #[test]
    fn external_feature_configs_override_inline_sections() {
        let directory = std::env::temp_dir().join(format!("config-feat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let mut main_config = bundled_config_yaml().to_string();
        main_config.push_str(
            "playback_config_path: playback.yaml\n\
             song_config_path: song.yaml\n\
             ai_config_path: ai.yaml\n\
             entertainment_config_path: entertainment.yaml\n\
             hotkeys_config_path: hotkeys.yaml\n\
             friend_delivery_config_path: friend_delivery.yaml\n\
             custom_workflows_config_path: custom_workflows.yaml\n",
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 每个外部文件只覆盖一个可辨识字段，其余继承内嵌配置。
        std::fs::write(
            directory.join("playback.yaml"),
            "kugou_api_base_url: http://127.0.0.1:3999\n",
        )
        .unwrap();
        std::fs::write(directory.join("song.yaml"), "queue:\n  max_size: 7\n").unwrap();
        std::fs::write(directory.join("ai.yaml"), "provider: deepseek\n").unwrap();
        std::fs::write(
            directory.join("entertainment.yaml"),
            "landlord:\n  enabled: false\n",
        )
        .unwrap();
        std::fs::write(directory.join("hotkeys.yaml"), "pause_key: F8\n").unwrap();
        std::fs::write(
            directory.join("friend_delivery.yaml"),
            "auto_retry_count: 3\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("custom_workflows.yaml"),
            "default_threshold: 0.85\n",
        )
        .unwrap();

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(config.playback.kugou_api_base_url, "http://127.0.0.1:3999");
        assert_eq!(config.playback.login_timeout_ms, 180000);
        assert_eq!(
            config.playback.credential_directory,
            directory.join("data/credentials")
        );
        assert_eq!(config.queue.max_size, 7);
        assert_eq!(config.queue.pool_max_size, 200);
        assert!(config.song_dedup.enabled);
        assert_eq!(config.ai.provider, "deepseek");
        assert_eq!(config.ai.model, "gpt-5.6-mini");
        assert!(!config.landlord.enabled);
        assert!(config.idiom_chain.enabled);
        assert_eq!(config.hotkeys.pause_key, "F8");
        assert_eq!(config.friend_delivery.auto_retry_count, 3);
        assert_eq!(config.custom_workflows.default_threshold, 0.85);
        assert_eq!(config.custom_workflows.workflows.len(), 13);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn all_splittable_sections_optional_when_external_files_provide_full_configs() {
        let directory = std::env::temp_dir().join(format!("config-all-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        // 发布形态：主配置只含核心段 + 全部 13 个外部引用。
        let bundled = bundled_config_yaml();
        let mut text = bundled.to_string();
        let removal_pairs = [
            ("screen:", "stability:"),
            ("ocr:", "templates:"),
            ("templates:", "output:"),
            ("moderation:", "playback:"),
            ("playback:", "http:"),
            ("queue:", "song_dedup:"),
            ("song_dedup:", "idiom_chain:"),
            ("idiom_chain:", "landlord:"),
            ("landlord:", "undercover:"),
            ("undercover:", "turtle_soup:"),
            ("turtle_soup:", "ai:"),
            ("ai:", "song_review:"),
            ("song_review:", "matching:"),
            ("matching:", "hotkeys:"),
            ("hotkeys:", "startup:"),
            ("startup:", "invite:"),
            ("invite:", "friend_delivery:"),
            ("friend_delivery:", "custom_workflows:"),
        ];
        for (section, next_section) in removal_pairs {
            let start = text.find(&format!("\n{section}")).expect(section) + 1;
            let tail = &text[start..];
            let next = tail.find(&format!("\n{next_section}")).expect(next_section);
            text = format!("{}{}", &text[..start], &tail[next..]);
        }
        // custom_workflows 是末段，删到文件尾。
        let start = text.find("\ncustom_workflows:").expect("custom_workflows") + 1;
        text.truncate(start);
        let main_config = format!(
            "{text}\
             ocr_config_path: ocr.yaml\n\
             screen_config_path: screen.yaml\n\
             templates_config_path: templates.yaml\n\
             moderation_config_path: moderation.yaml\n\
             startup_config_path: startup.yaml\n\
             invite_config_path: invite.yaml\n\
             playback_config_path: playback.yaml\n\
             song_config_path: song.yaml\n\
             ai_config_path: ai.yaml\n\
             entertainment_config_path: entertainment.yaml\n\
             hotkeys_config_path: hotkeys.yaml\n\
             friend_delivery_config_path: friend_delivery.yaml\n\
             custom_workflows_config_path: custom_workflows.yaml\n"
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, main_config).unwrap();

        // 外部文件提供各段完整内容。
        let single_files = [
            ("screen:", "stability:", "screen.yaml"),
            ("ocr:", "templates:", "ocr.yaml"),
            ("templates:", "output:", "templates.yaml"),
            ("moderation:", "playback:", "moderation.yaml"),
            ("playback:", "http:", "playback.yaml"),
            ("ai:", "song_review:", "ai.yaml"),
            ("hotkeys:", "startup:", "hotkeys.yaml"),
            (
                "friend_delivery:",
                "custom_workflows:",
                "friend_delivery.yaml",
            ),
        ];
        for (section, next_section, file) in single_files {
            std::fs::write(
                directory.join(file),
                extract_section_dedented(bundled, section, next_section),
            )
            .unwrap();
        }
        std::fs::write(
            directory.join("song.yaml"),
            [
                format!(
                    "queue:\n{}",
                    extract_section_as_is(bundled, "queue:", "song_dedup:")
                ),
                format!(
                    "song_dedup:\n{}",
                    extract_section_as_is(bundled, "song_dedup:", "idiom_chain:")
                ),
                format!(
                    "song_review:\n{}",
                    extract_section_as_is(bundled, "song_review:", "matching:")
                ),
                format!(
                    "matching:\n{}",
                    extract_section_as_is(bundled, "matching:", "hotkeys:")
                ),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            directory.join("entertainment.yaml"),
            [
                format!(
                    "idiom_chain:\n{}",
                    extract_section_as_is(bundled, "idiom_chain:", "landlord:")
                ),
                format!(
                    "landlord:\n{}",
                    extract_section_as_is(bundled, "landlord:", "undercover:")
                ),
                format!(
                    "undercover:\n{}",
                    extract_section_as_is(bundled, "undercover:", "turtle_soup:")
                ),
                format!(
                    "turtle_soup:\n{}",
                    extract_section_as_is(bundled, "turtle_soup:", "ai:")
                ),
            ]
            .join("\n"),
        )
        .unwrap();
        // custom_workflows 是末段：提取到文件尾，去缩进为无段名单段文件。
        let workflows = bundled
            .find("\ncustom_workflows:")
            .expect("custom_workflows")
            + 1
            + "custom_workflows:".len();
        let workflows_content = bundled[workflows..]
            .lines()
            .map(|line| {
                if let Some(rest) = line.strip_prefix("  ") {
                    rest
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            directory.join("custom_workflows.yaml"),
            workflows_content.trim_end(),
        )
        .unwrap();
        let startup_files = [
            ("startup:", "invite:", "startup.yaml"),
            ("invite:", "friend_delivery:", "invite.yaml"),
        ];
        for (section, next_section, file) in startup_files {
            std::fs::write(
                directory.join(file),
                extract_section_dedented(bundled, section, next_section),
            )
            .unwrap();
        }

        let config = AppConfig::load_from_root(&config_path, &directory).expect("load config");
        assert_eq!(
            config.playback.credential_directory,
            directory.join("data/credentials")
        );
        assert_eq!(
            config.playback.login_helper_executable,
            directory.join("miliastra-login-helper.exe")
        );
        assert_eq!(config.playback.kugou_api_base_url, "http://127.0.0.1:3000");
        assert_eq!(config.queue.max_size, 5);
        assert_eq!(
            config.song_dedup.history_path,
            directory.join("data/song-dedup-history.json")
        );
        assert!(!config.song_review.enabled);
        assert_eq!(config.matching.min_song_name_score, 0.5);
        assert_eq!(config.ai.provider, "openai");
        assert_eq!(
            config.idiom_chain.lexicon_path,
            directory.join("assets/idioms.txt")
        );
        assert!(config.landlord.enabled);
        assert!(!config.undercover.enabled);
        assert!(!config.turtle_soup.enabled);
        assert_eq!(config.hotkeys.pause_key, "F7");
        assert_eq!(config.friend_delivery.auto_retry_count, 0);
        assert_eq!(config.custom_workflows.workflows.len(), 13);
        assert!(!config.custom_workflows.workflows[0].steps.is_empty());
        // 核心段仍来自主配置。
        assert_eq!(config.http.port, 18888);
        assert_eq!(
            config.window.target_process,
            "yuanshen.exe,GenshinImpact.exe"
        );
        assert!(config.validate().is_ok());

        std::fs::remove_dir_all(&directory).unwrap();
    }
}
