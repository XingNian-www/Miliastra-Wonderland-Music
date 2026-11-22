//! `AppConfig` / `BootstrapConfig`：完整业务配置的聚合入口与默认值。
//!
//! `AppConfig` 的每个顶层段都是独立结构体（见同目录各业务域模块），这里只负责
//! 聚合、给出能通过 [`AppConfig::validate`] 的默认值，以及启动引导的严格解析。
//! 段的顺序就是配置库 JSON 的键顺序，也是面板段的默认顺序，不要随意调整。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adapters::file_store::write_atomic;

use crate::config::ai::AiConfig;
use crate::features::ai_chat::AiChatConfig;
use crate::features::card_games::LandlordConfig;
use crate::features::custom_workflow::CustomWorkflowConfig;
use crate::features::identity::IdentityConfig;
use crate::features::idiom_chain::IdiomChainConfig;
use crate::features::invite::InviteConfig;
use crate::features::moderation::ModerationConfig;
use crate::features::number_bomb::NumberBombConfig;
use crate::features::playback::{MatchConfig, QueueConfig, SongDedupConfig};
use crate::features::song_request::{SongAiConfig, SongReviewConfig};
use crate::features::startup::{StartupConfig, StartupTemplateConfig};
use crate::features::tts::TtsConfig;
use crate::features::turtle_soup::TurtleSoupConfig;
use crate::features::undercover::UndercoverConfig;
use crate::runtime::player::PlayerObservationConfig;
use crate::runtime::player_io::{PlayerRuntimeConfig, PlayerRuntimeConfigError};

use super::general::{
    HotkeyConfig, OutputConfig, PointConfig, RectConfig, ScreenConfig, StabilityConfig,
    StateConfig, WindowConfig, resolve_stability_count,
};
use super::paths::{
    resolve_optional_path, resolve_path, validate_point_in_canvas, validate_rect_in_canvas,
};
use super::playback::PlaybackConfig;
use super::system::{
    DiagnosticsConfig, FriendDeliveryConfig, HttpConfig, KugouDeviceConfig, LoggingConfig,
    TuiConfig,
};
use super::ui::{ChatSourceConfig, OcrConfig, TemplateConfig, TimingConfig};
use crate::config::visual;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]

pub struct AppConfig {
    #[serde(default)]
    pub visual: visual::VisualConfig,
    #[serde(default)]
    pub command_access: crate::features::command_access::CommandAccessConfig,
    /// 外部聊天源（Miliastra-Wonderland-ChatBridge WS）；段可省略。
    #[serde(default)]
    pub chat_source: ChatSourceConfig,
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
    /// UI 诊断存档；内嵌段可省略（默认关闭）。
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
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
    /// 数字炸弹；内嵌段可省略。
    #[serde(default)]
    pub number_bomb: NumberBombConfig,
    /// 海龟汤；内嵌段可省略。
    #[serde(default)]
    pub turtle_soup: TurtleSoupConfig,
    /// 统一 AI 连接配置（点歌、歌曲审核、海龟汤共用）；内嵌段可省略。
    #[serde(default)]
    pub ai: AiConfig,
    /// 点歌 AI 的提示词与模型覆盖；内嵌段可省略。
    #[serde(default)]
    pub song_ai: SongAiConfig,
    /// AI 聊天与轻量记忆；内嵌段可省略。
    #[serde(default)]
    pub ai_chat: AiChatConfig,
    /// TTS 语音（fish.audio）；内嵌段可省略。
    #[serde(default)]
    pub tts: TtsConfig,
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
    /// 昵称身份映射；内嵌段可省略。
    #[serde(default)]
    pub identity: IdentityConfig,
    /// 二级聊天表情；内嵌段可省略。
    #[serde(default)]
    pub emoji: crate::features::emoji::EmojiConfig,
    /// 酷狗设备信息（由用户填写，程序不再自动生成）；内嵌段可省略。
    #[serde(default)]
    pub kugou: KugouDeviceConfig,
}

/// 单条回复文案的字符上限。
///
/// 游戏聊天框宽度有限，超长文案会被截断或被 OCR 切成多行，
/// 反而比内置短文案更不可靠，因此这里主动设上限。
///
/// 命令面板与业务校验共用这一个来源，避免两个入口给出不同限制。
pub const MAX_REPLY_OVERRIDE_CHARS: usize = 300;

impl Default for AppConfig {
    /// 默认配置必须能通过 [`AppConfig::validate`]：SQLite 配置中心会用默认值建库，
    /// 校验失败会阻断初始化。screen/ocr/templates 的默认值见各段 Default；
    /// invite/moderation/startup 的段 Default 含 0 值/空路径（位于各自功能模块），
    /// 无法通过校验，因此这里显式给出与完整配置模板一致的有效值。
    /// 区域坐标沿用 src/config/test.full.yaml 的 1920x1080 布局，
    /// 路径类默认使用发布布局（deps/models、deps/templates、deps/data）。
    fn default() -> Self {
        Self {
            visual: visual::VisualConfig::default(),
            emoji: crate::features::emoji::EmojiConfig::default(),
            command_access: crate::features::command_access::CommandAccessConfig::default(),
            chat_source: ChatSourceConfig::default(),
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
                    // 纯文字「搜索」按钮：实测命中左上角恒为 (1657,126)
                    // （1920×1080 夹具与 1440p 实机归一化后一致），四周各留 12px
                    // 定位余量；旧 240×90 会把右侧无关按钮卷进来。
                    x: 1645,
                    y: 114,
                    width: 74,
                    height: 49,
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
                    // 纯文字「屏蔽聊天」：x 固定在 (≈1412)，y 随好友卡/列表行与
                    // UI 缩放变化（1920 夹具 374、1440p 实机归一化 438）。区域按
                    // 实测两档覆盖，横向只保留 12px 余量；原点与旧区域同余 4，
                    // 保证 4× 下采样的采样相位不变。
                    x: 1400,
                    y: 362,
                    width: 124,
                    height: 112,
                },
                blacklist_region: RectConfig {
                    // 纯文字「加入黑名单」：位于「屏蔽聊天」下方一行，同样随
                    // 好友卡/UI 缩放上下移动；实测两档左上角 (1400,506) 与 (1400,573)。
                    x: 1388,
                    y: 494,
                    width: 146,
                    height: 117,
                },
                confirm_button_region: RectConfig {
                    // 通用「确认」按钮：实测命中左上角恒为 (1148,740)（8 帧一致），
                    // 区域只留 12px 余量；旧 500×100 会把按钮两侧的弹窗内容卷进来。
                    // 原点与旧区域同余 4，4× 采样相位不变。
                    x: 1140,
                    y: 728,
                    width: 81,
                    height: 58,
                },
                confirm_message_region: RectConfig {
                    // 确认弹窗提示文案行：2× 下采样实测命中左上角分别为
                    // 「随机公共大厅」(850,488)、「返回提瓦特」(996,488)、
                    // 「屏蔽吗」(1050,498)，右边界最大 1146、下边界 534。
                    // 横放宽到 650–1270，覆盖玩家名变长导致关键词右移；
                    // 纵留 460–550 的整行余量。
                    x: 650,
                    y: 460,
                    width: 620,
                    height: 90,
                },
            },
            playback: PlaybackConfig::default(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
            tui: TuiConfig::default(),
            state: StateConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            queue: QueueConfig::default(),
            song_dedup: SongDedupConfig::default(),
            idiom_chain: IdiomChainConfig::default(),
            identity: IdentityConfig::default(),
            kugou: KugouDeviceConfig::default(),
            landlord: LandlordConfig::default(),
            undercover: UndercoverConfig::default(),
            number_bomb: NumberBombConfig::default(),
            turtle_soup: TurtleSoupConfig::default(),
            ai: AiConfig::default(),
            song_ai: SongAiConfig::default(),
            ai_chat: AiChatConfig::default(),
            tts: TtsConfig::default(),
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
                // 默认预算按"每个阶段都不超过一分钟"分配（用户明确要求）：
                //   启动游戏 ≤ 60s、进入游戏 ≤ 45s、进入千星 ≤ 51s。
                // 原来的默认值最坏要 15 分钟（星标 120×2.5s + 大厅 90×2s +
                // 三处 60s 过渡 + 120s 主界面），卡在某个阶段时用户完全不知道
                // 还要等多久，因此把每个阶段的**上限**都收到一分钟以内。
                // 超时只是放弃等待并如实报错，不会误点错位置；
                // 真机慢到超过一分钟的用户可以在配置中心自行调大。
                launch_wait_ms: 3000,
                launch_retries: 20,
                enter_game_timeout_ms: 45000,
                enter_wonderland_timeout_ms: 60000,
                wonderland_map_star_retries: 6,
                wonderland_map_star_retry_ms: 2500,
                wonderland_hall_retries: 6,
                wonderland_hall_retry_ms: 2500,
                wonderland_transition_timeout_ms: 4000,
                wonderland_confirm_stable_timeout_ms: 3000,
                final_primary_timeout_ms: 10000,
                // 默认 60s：正常冷启动进世界一般 15~40s，超过一分钟还停在加载
                // 界面基本就是卡死，此时重启游戏比继续等更有效。
                loading_restart_timeout_ms: 60_000,
                restart_game_on_loading: false,
                poll_ms: 1000,
                stable_mean_threshold: 2.0,
                stable_changed_ratio_threshold: 0.01,
                template_threshold: 0.9,
                wonderland_confirm_threshold: 0.9,
                templates: StartupTemplateConfig {
                    wonderland_map_star: PathBuf::from(
                        "deps/assets/startup-wonderland-map-star.png",
                    ),
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
                fuzzy_friend_match: false,
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
                    // 纯文字「查看千星奇域档案」：x 固定在 (≈508)（头像菜单总在
                    // 聊天头像右侧同一列），横向收窄到文字宽度 + 12px；**纵向必须
                    // 保留**整个聊天列，因为菜单跟着被点击的头像上下移动（实测
                    // 1920 夹具 724、1440p 实机归一化 186，1654 帧实机扫描只出现
                    // 这两档位置，两端都覆盖）。
                    x: 498,
                    y: 80,
                    width: 217,
                    height: 860,
                },
                goto_hall_region: RectConfig {
                    // 同时服务「前往其大厅」(4×) 与「查看资料」(4×)：实测新左上角
                    // 前往其大厅 (1012,572)、查看资料 (1024,628) 与 (788,568)。
                    // 旧 500×300 是整块卡片区域；现在只覆盖右侧按钮列的两行按钮，
                    // 横向仍留 236px 跨度以覆盖两种卡片版式；原点保持 4 的倍数。
                    x: 780,
                    y: 560,
                    width: 372,
                    height: 112,
                },
                enter_hall_region: RectConfig {
                    // 纯文字「进入该大厅」：大厅卡实测命中左上角 (906,740)，
                    // 区域收窄到文字宽度 + 12px 余量；原点与旧区域同奇偶（2× 相位不变）。
                    x: 896,
                    y: 730,
                    width: 164,
                    height: 54,
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

/// 发布打包的初始化子命令（`miliastra-wonderland-music --ensure-config-db <path>`）。
///
/// 它只在构建机上生成空配置库结构，**不能**把构建机的面板令牌写进
/// config.yaml 随包分发（否则所有用户共用同一令牌）。
const INIT_ONLY_FLAG: &str = "--ensure-config-db";
/// 显式开关：任何调用方（含未改动的入口）都可以用它进入「只建库、不发令牌」模式。
const INIT_ONLY_ENV: &str = "MILIASTRA_INIT_ONLY";
/// 初始化模式的内存占位令牌。
///
/// 空令牌会让完整校验失败（`http.access_token 不能为空`），所以初始化模式用这个
/// 明显不可用的占位值走完校验；它**不写 config.yaml、也不落配置库**（http 段不落库），
/// 进程退出即丢弃。正式启动仍会生成真正的随机令牌并持久化。
const INIT_ONLY_PLACEHOLDER_TOKEN: &str = "init-only-placeholder-not-a-runtime-token";

/// 引导配置加载模式：正式启动 / 打包初始化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    /// 正式启动：空令牌自动生成并回写 config.yaml。
    Runtime,
    /// 打包初始化（`--ensure-config-db`）：只建库 + 校验，不回写 config.yaml。
    InitOnly,
}

impl BootstrapMode {
    fn detect() -> Self {
        if init_only_requested(
            std::env::var_os(INIT_ONLY_ENV).is_some(),
            std::env::args_os().skip(1),
        ) {
            Self::InitOnly
        } else {
            Self::Runtime
        }
    }
}

/// 初始化模式判定（纯函数，便于单测）。
///
/// 入口在 `main.rs`：`--ensure-config-db` 分支只做建库/校验，不启动运行时。
/// 本层不改入口，因此同时识别该子命令参数与显式环境变量；两者任一成立即初始化模式。
fn init_only_requested(
    env_switch: bool,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    env_switch
        || args
            .into_iter()
            .any(|arg| arg.to_str() == Some(INIT_ONLY_FLAG))
}

impl BootstrapConfig {
    /// 从 config.yaml 加载最小启动配置。
    ///
    /// 严格解析（[`deny_unknown_fields`]）：只允许 database_path、http、logging 三个引导段。
    /// 正式启动时 access_token 缺失会自动生成并持久化；打包初始化
    /// （`--ensure-config-db` / `MILIASTRA_INIT_ONLY`）只校验、不回写。
    /// database_path 为相对路径时相对 `executable_root`（EXE 所在目录）解析为绝对路径。
    pub fn load(path: &Path, executable_root: &Path) -> Result<Self> {
        Self::load_with_mode(path, executable_root, BootstrapMode::detect())
    }

    fn load_with_mode(path: &Path, executable_root: &Path, mode: BootstrapMode) -> Result<Self> {
        const BOOTSTRAP_ONLY_HINT: &str = "config.yaml 只允许 database_path、http、logging 三个引导段，其余配置请通过 Web 面板「配置中心」管理";
        let text = fs::read_to_string(path)
            .with_context(|| format!("读取启动配置失败: {}", path.display()))?;
        let mut bootstrap: BootstrapConfig = serde_norway::from_str(&text).with_context(|| {
            format!(
                "解析启动配置失败: {}。{BOOTSTRAP_ONLY_HINT}",
                path.display()
            )
        })?;
        if bootstrap.database_path.as_os_str().is_empty() {
            bail!("config.yaml 的 database_path 不能为空。{BOOTSTRAP_ONLY_HINT}");
        }
        if bootstrap.http.access_token.trim().is_empty() {
            match mode {
                BootstrapMode::Runtime => {
                    bootstrap.http.access_token = uuid::Uuid::new_v4().simple().to_string();
                    let serialized =
                        serde_norway::to_string(&bootstrap).context("序列化启动配置失败")?;
                    write_atomic(path, serialized.as_bytes(), "启动配置").with_context(|| {
                        format!("写入自动生成的访问令牌失败: {}", path.display())
                    })?;
                }
                BootstrapMode::InitOnly => {
                    bootstrap.http.access_token = INIT_ONLY_PLACEHOLDER_TOKEN.to_string();
                }
            }
        }
        if bootstrap.database_path.is_relative() {
            bootstrap.database_path = executable_root.join(&bootstrap.database_path);
        }
        Ok(bootstrap)
    }
}

#[cfg(test)]
mod bootstrap_token_tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{BootstrapConfig, BootstrapMode, INIT_ONLY_FLAG, init_only_requested};

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bootstrap-token-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("创建临时目录");
        root
    }

    fn write_bootstrap_config(root: &Path) -> PathBuf {
        let path = root.join("config.yaml");
        let text = r#"database_path: deps/data/playback.sqlite3
http:
  host: 127.0.0.1
  port: 18888
  enabled: true
  access_token: ""
logging:
  dir: deps/logs
  level: info
  rotate_daily: true
  retain_days: 7
"#;
        fs::write(&path, text).expect("写 config.yaml");
        path
    }

    /// B11-P1-1：发布打包的初始化路径不得生成/回写面板令牌。
    #[test]
    fn packaging_init_mode_neither_generates_nor_rewrites_the_access_token() {
        let root = temp_root("init");
        let path = write_bootstrap_config(&root);
        let before = fs::read(&path).expect("读取原始 config.yaml");

        let bootstrap = BootstrapConfig::load_with_mode(&path, &root, BootstrapMode::InitOnly)
            .expect("初始化模式必须能加载并通过引导校验");
        // 完整校验要求令牌非空，初始化模式只能用「明显不可用」的内存占位值；
        // 它既没有写进 config.yaml，也不会落库（http 段不落库），进程结束即丢弃。
        assert_eq!(
            bootstrap.http.access_token,
            super::INIT_ONLY_PLACEHOLDER_TOKEN,
            "初始化模式不得生成真实访问令牌（只允许内存占位）"
        );

        let after = fs::read(&path).expect("读取初始化后的 config.yaml");
        assert_eq!(before, after, "初始化模式不得改写 config.yaml");
        fs::remove_dir_all(root).ok();
    }

    /// 初始化路径的完整链路：建库 + 完整校验必须成功，且库文件里不得出现令牌。
    #[test]
    fn packaging_init_mode_builds_a_valid_database_without_any_token() {
        let root = temp_root("full-init");
        let path = write_bootstrap_config(&root);
        let before = fs::read(&path).expect("读取原始 config.yaml");

        let bootstrap = BootstrapConfig::load_with_mode(&path, &root, BootstrapMode::InitOnly)
            .expect("初始化模式必须能加载引导配置");
        let store =
            crate::config::ConfigStore::open(&bootstrap.database_path, &root, bootstrap.clone())
                .expect("初始化模式必须能建库");
        let config = store.load_full().expect("初始化模式必须通过完整配置校验");
        assert_eq!(
            config.http.access_token,
            super::INIT_ONLY_PLACEHOLDER_TOKEN,
            "内存占位值必须能通过完整校验"
        );
        drop(store);

        // config.yaml 原样保留，库文件里也没有任何真实/占位令牌字样。
        assert_eq!(
            fs::read(&path).expect("读取初始化后的 config.yaml"),
            before,
            "初始化模式不得改写 config.yaml"
        );
        let placeholder = super::INIT_ONLY_PLACEHOLDER_TOKEN.as_bytes();
        let database = fs::read(&bootstrap.database_path).expect("读取初始化后的配置库");
        assert!(
            !database
                .windows(placeholder.len())
                .any(|window| window == placeholder),
            "配置库不得写入访问令牌（http 段不落库）"
        );
        fs::remove_dir_all(root).ok();
    }

    /// 回归另一半：正式启动仍要在首次运行生成并持久化随机令牌。
    #[test]
    fn runtime_mode_still_generates_and_persists_the_access_token() {
        let root = temp_root("runtime");
        let path = write_bootstrap_config(&root);

        let bootstrap = BootstrapConfig::load_with_mode(&path, &root, BootstrapMode::Runtime)
            .expect("正式启动必须能加载配置");
        let token = bootstrap.http.access_token.clone();
        assert_eq!(token.len(), 32, "必须是 uuid v4 simple 形式的令牌");
        assert_ne!(token, super::INIT_ONLY_PLACEHOLDER_TOKEN);

        // 第二次加载必须读到同一令牌，且文件内容确实被回写。
        let persisted = BootstrapConfig::load_with_mode(&path, &root, BootstrapMode::InitOnly)
            .expect("回写后的配置必须可再次加载");
        assert_eq!(persisted.http.access_token, token);
        let text = fs::read_to_string(&path).expect("读取回写后的 config.yaml");
        assert!(text.contains(&token), "config.yaml 必须持久化访问令牌");
        fs::remove_dir_all(root).ok();
    }

    /// 初始化模式判定同时覆盖显式环境变量与发布打包子命令。
    #[test]
    fn init_mode_is_detected_from_the_env_switch_or_the_packaging_flag() {
        assert!(init_only_requested(true, std::iter::empty()));
        assert!(init_only_requested(
            false,
            [
                OsString::from(INIT_ONLY_FLAG),
                OsString::from("config.yaml")
            ]
        ));
        assert!(!init_only_requested(
            false,
            [OsString::from("--help"), OsString::from("config.yaml")]
        ));
        assert!(!init_only_requested(false, std::iter::empty()));
    }
}

const PLAYER_OBSERVATION_COMMAND_CAPACITY: usize = 16;
const PLAYER_ACTIVE_FAST_DEMAND_CAPACITY: usize = 16;
const PLAYER_CONTROL_QUEUE_CAPACITY: usize = 16;
const PLAYER_SEARCH_QUEUE_CAPACITY: usize = 16;

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
        self.visual.validate()?;
        self.command_access.validate()?;
        self.chat_source.validate()?;
        self.screen.validate()?;
        self.ocr.validate()?;
        self.templates.validate()?;
        self.playback.validate()?;
        self.http.validate()?;
        self.logging.validate()?;
        self.tui.validate()?;
        self.state.validate()?;
        self.diagnostics.validate()?;
        self.hotkeys.validate()?;
        self.queue.validate()?;
        self.song_dedup.validate()?;
        self.matching.validate()?;
        self.idiom_chain.validate()?;
        self.landlord.validate()?;
        self.undercover.validate()?;
        self.number_bomb.validate()?;
        self.invite.validate(&self.timing.invite)?;
        self.moderation.validate(&self.timing.moderation)?;
        self.custom_workflows.validate()?;
        self.startup.validate()?;
        self.ai.validate()?;
        self.song_ai.validate()?;
        self.ai_chat.validate()?;
        self.tts.validate()?;
        self.song_review.validate()?;
        self.turtle_soup.validate()?;
        self.identity.validate()?;
        self.emoji.validate()?;
        self.kugou.validate()?;
        self.validate_ui_geometry()?;
        Ok(())
    }

    pub(crate) fn validate_ui_geometry(&self) -> Result<()> {
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
            (
                self.moderation.confirm_button_region,
                "moderation.confirm_button_region",
            ),
            // 确认弹窗关键词行与确认按钮成对使用（双命中判定子类型），
            // 越界同样不能落库：ModerationConfig::validate 只看宽高非零，看不到画布。
            (
                self.moderation.confirm_message_region,
                "moderation.confirm_message_region",
            ),
            (
                self.startup.enter_game_text_region,
                "startup.enter_game_text_region",
            ),
            (
                self.startup.wonderland_hall_ocr_region,
                "startup.wonderland_hall_ocr_region",
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
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| {
            format!(
                "读取配置失败: {}。请将发布包中的 config.yaml 放在主程序 EXE 所在目录",
                path.display()
            )
        })?;
        serde_norway::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))
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
            &mut self.templates.secondary_back,
            &mut self.templates.secondary_hall,
            &mut self.templates.secondary_chat_submit,
            // 千星主菜单锚点与 F2 大厅两个按钮：与其它模板同一套解析规则。
            // 漏解析时 CWD≠exe 目录会让相对路径原样传给模板加载，运行期才静默失败。
            &mut self.templates.main_menu,
            &mut self.templates.qianxing_f2_hall_list,
            &mut self.templates.qianxing_f2_invite_player,
            &mut self.templates.qianxing_exit_menu,
            &mut self.templates.qianxing_map_label,
            &mut self.templates.qianxing_profile_page_wishlist,
            &mut self.templates.qianxing_profile_page_wardrobe,
            &mut self.templates.teyvat_profile_adventure_rank,
            &mut self.templates.teyvat_profile_world_level,
            &mut self.templates.teyvat_profile_add_friend,
            &mut self.templates.qianxing_profile_domain_level,
            &mut self.templates.qianxing_profile_add_friend,
            &mut self.templates.qianxing_profile_view_profile,
            &mut self.templates.world_wish,
            &mut self.templates.invite_view_star,
            &mut self.templates.invite_goto_hall,
            &mut self.templates.invite_enter_hall,
            &mut self.templates.friend_panel,
            &mut self.templates.friend_search_panel,
            &mut self.templates.friend_more_settings,
            &mut self.templates.friend_block_chat,
            &mut self.templates.friend_blacklist,
            &mut self.templates.confirm_button,
            &mut self.templates.confirm_message_random_hall,
            &mut self.templates.confirm_message_return_teyvat,
            &mut self.templates.confirm_message_block,
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

#[cfg(test)]
mod runtime_path_and_geometry_tests {
    use std::path::{Path, PathBuf};

    use super::AppConfig;
    use crate::config::RectConfig;

    /// 模板路径必须**全部**走 `resolve_runtime_paths`：漏一个，CWD≠exe 目录时
    /// 相对路径会原样传给模板加载，运行期才静默失败。
    ///
    /// 这里用「相对路径 → 必须变成 root 下的绝对路径」逐字段验证，不抄手写清单
    /// （缺陷正是清单漏项：`main_menu` / `qianxing_f2_hall_list` /
    /// `qianxing_f2_invite_player` 三个字段在结构体与 schema 里都有，却不在解析清单里）。
    #[test]
    fn every_template_path_is_resolved_against_the_executable_root() {
        let mut config = AppConfig::default();
        // 全部置成同一个相对名：解析成功的字段会变成 root.join(name)。
        let relative = PathBuf::from("assets/probe.png");
        config.templates.blue_marker = relative.clone();
        config.templates.yellow_marker = relative.clone();
        config.templates.pink_marker = relative.clone();
        config.templates.secondary_back = relative.clone();
        config.templates.secondary_hall = relative.clone();
        config.templates.secondary_chat_submit = relative.clone();
        config.templates.main_menu = relative.clone();
        config.templates.world_wish = relative.clone();
        config.templates.invite_view_star = relative.clone();
        config.templates.invite_goto_hall = relative.clone();
        config.templates.invite_enter_hall = relative.clone();
        config.templates.friend_panel = relative.clone();
        config.templates.friend_search_panel = relative.clone();
        config.templates.friend_more_settings = relative.clone();
        config.templates.friend_block_chat = relative.clone();
        config.templates.friend_blacklist = relative.clone();
        config.templates.confirm_button = relative.clone();
        config.templates.confirm_message_random_hall = relative.clone();
        config.templates.confirm_message_return_teyvat = relative.clone();
        config.templates.confirm_message_block = relative.clone();
        config.templates.qianxing_f2_hall_list = relative.clone();
        config.templates.qianxing_f2_invite_player = relative.clone();
        config.templates.qianxing_exit_menu = relative.clone();
        config.templates.qianxing_map_label = relative.clone();
        config.templates.qianxing_profile_page_wishlist = relative.clone();
        config.templates.qianxing_profile_page_wardrobe = relative.clone();
        config.templates.teyvat_profile_adventure_rank = relative.clone();
        config.templates.teyvat_profile_world_level = relative.clone();
        config.templates.teyvat_profile_add_friend = relative.clone();
        config.templates.qianxing_profile_domain_level = relative.clone();
        config.templates.qianxing_profile_add_friend = relative.clone();
        config.templates.qianxing_profile_view_profile = relative.clone();

        let root = Path::new("/probe/executable/root");
        config.resolve_runtime_paths(root);

        // 逐字段断言，失败时点名是哪个字段没被解析。
        let fields: Vec<(&str, &Path)> = vec![
            ("templates.blue_marker", &config.templates.blue_marker),
            ("templates.yellow_marker", &config.templates.yellow_marker),
            ("templates.pink_marker", &config.templates.pink_marker),
            ("templates.secondary_back", &config.templates.secondary_back),
            ("templates.secondary_hall", &config.templates.secondary_hall),
            (
                "templates.secondary_chat_submit",
                &config.templates.secondary_chat_submit,
            ),
            ("templates.main_menu", &config.templates.main_menu),
            ("templates.world_wish", &config.templates.world_wish),
            (
                "templates.invite_view_star",
                &config.templates.invite_view_star,
            ),
            (
                "templates.invite_goto_hall",
                &config.templates.invite_goto_hall,
            ),
            (
                "templates.invite_enter_hall",
                &config.templates.invite_enter_hall,
            ),
            ("templates.friend_panel", &config.templates.friend_panel),
            (
                "templates.friend_search_panel",
                &config.templates.friend_search_panel,
            ),
            (
                "templates.friend_more_settings",
                &config.templates.friend_more_settings,
            ),
            (
                "templates.friend_block_chat",
                &config.templates.friend_block_chat,
            ),
            (
                "templates.friend_blacklist",
                &config.templates.friend_blacklist,
            ),
            ("templates.confirm_button", &config.templates.confirm_button),
            (
                "templates.confirm_message_random_hall",
                &config.templates.confirm_message_random_hall,
            ),
            (
                "templates.confirm_message_return_teyvat",
                &config.templates.confirm_message_return_teyvat,
            ),
            (
                "templates.confirm_message_block",
                &config.templates.confirm_message_block,
            ),
            (
                "templates.qianxing_f2_hall_list",
                &config.templates.qianxing_f2_hall_list,
            ),
            (
                "templates.qianxing_f2_invite_player",
                &config.templates.qianxing_f2_invite_player,
            ),
            (
                "templates.qianxing_exit_menu",
                &config.templates.qianxing_exit_menu,
            ),
            (
                "templates.qianxing_map_label",
                &config.templates.qianxing_map_label,
            ),
            (
                "templates.qianxing_profile_page_wishlist",
                &config.templates.qianxing_profile_page_wishlist,
            ),
            (
                "templates.qianxing_profile_page_wardrobe",
                &config.templates.qianxing_profile_page_wardrobe,
            ),
            (
                "templates.teyvat_profile_adventure_rank",
                &config.templates.teyvat_profile_adventure_rank,
            ),
            (
                "templates.teyvat_profile_world_level",
                &config.templates.teyvat_profile_world_level,
            ),
            (
                "templates.teyvat_profile_add_friend",
                &config.templates.teyvat_profile_add_friend,
            ),
            (
                "templates.qianxing_profile_domain_level",
                &config.templates.qianxing_profile_domain_level,
            ),
            (
                "templates.qianxing_profile_add_friend",
                &config.templates.qianxing_profile_add_friend,
            ),
            (
                "templates.qianxing_profile_view_profile",
                &config.templates.qianxing_profile_view_profile,
            ),
        ];
        for (field, resolved) in fields {
            assert!(
                resolved.is_absolute(),
                "{field} 必须被解析成绝对路径，实际：{}",
                resolved.display()
            );
            assert!(
                resolved.ends_with(&relative),
                "{field} 必须挂在 exe 根下，实际：{}",
                resolved.display()
            );
        }
    }

    /// 画布越界校验必须覆盖 `moderation.confirm_message_region`：
    /// 它与 `confirm_button_region` 成对参与「确认按钮 + 文案」双命中判定，
    /// 只校验按钮会让文案行越界值落库，运行期 OCR 区域跑到画布外。
    #[test]
    fn confirm_message_region_is_bounds_checked_against_the_canvas() {
        // 默认值必须合法（不能因为补校验把默认配置打死）。
        AppConfig::default()
            .validate_ui_geometry()
            .expect("默认配置的确认文案区域必须合法");

        for (x, y, width, height) in [
            (-1, 460, 620, 90),   // 负坐标
            (650, -1, 620, 90),   // 负坐标
            (1920, 460, 620, 90), // 右越界
            (650, 1080, 620, 90), // 下越界
        ] {
            let mut config = AppConfig::default();
            config.moderation.confirm_message_region = RectConfig {
                x,
                y,
                width,
                height,
            };
            let error = config
                .validate_ui_geometry()
                .expect_err("越界的确认文案区域必须被拒绝")
                .to_string();
            assert!(
                error.contains("moderation.confirm_message_region"),
                "错误信息必须点名 moderation.confirm_message_region，实际：{error}"
            );
        }
    }
}
