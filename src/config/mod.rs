use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adapters::file_store::write_atomic;

use crate::config::ai::AiConfig;
use crate::features::ai_chat::AiChatConfig;
use crate::features::card_games::LandlordConfig;
use crate::features::custom_workflow::{CustomWorkflowConfig, WorkflowTimingConfig};
use crate::features::identity::IdentityConfig;
use crate::features::idiom_chain::IdiomChainConfig;
use crate::features::invite::{InviteConfig, InviteTimingConfig};
use crate::features::moderation::{ModerationConfig, ModerationTimingConfig};
use crate::features::playback::{MatchConfig, PlaybackTimingConfig, QueueConfig, SongDedupConfig};
use crate::features::song_request::{SongAiConfig, SongReviewConfig};
use crate::features::startup::{StartupConfig, StartupTemplateConfig};
use crate::features::number_bomb::NumberBombConfig;
use crate::features::turtle_soup::TurtleSoupConfig;
use crate::features::tts::TtsConfig;
use crate::features::undercover::UndercoverConfig;
use crate::runtime::player::PlayerObservationConfig;
use crate::runtime::player_io::{PlayerRuntimeConfig, PlayerRuntimeConfigError};

/// 外部聊天源：用 Miliastra-Wonderland-ChatBridge 的 WS 推送替代 OCR 的聊天内容识别。
///
/// 界面识别（Primary/Secondary/NoChat）仍由模板匹配完成，不受本段影响。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChatSourceConfig {
    /// 是否启用外部聊天源。关闭时完全走 OCR。
    #[serde(default)]
    pub enabled: bool,
    /// WS 地址，如 `ws://127.0.0.1:18889/stream`。
    #[serde(default = "default_chat_source_url")]
    pub url: String,
    /// 鉴权令牌，通常与 `http.access_token` 相同。
    #[serde(default)]
    pub token: String,
    /// 断开后重连间隔。
    #[serde(default = "default_chat_source_reconnect_ms")]
    pub reconnect_ms: u64,
    /// 外部源失联多久后回落到 OCR。
    #[serde(default = "default_chat_source_lease_ms")]
    pub lease_timeout_ms: u64,
    /// 是否由 BOT 负责拉起 Miliastra-Wonderland-ChatBridge（登录器角色）。
    ///
    /// 开启后，BOT 在连接失败时会尝试启动 Miliastra-Wonderland-ChatBridge；关闭则要求用户自己先运行它。
    #[serde(default)]
    pub auto_start: bool,
    /// Miliastra-Wonderland-ChatBridge 可执行文件路径。
    /// 留空则在 BOT 目录及其上一级的 deps/bin、Miliastra-Wonderland-ChatBridge 目录，
    /// 以及带版本号的解压目录里查找 miliastra-wonderland-chatbridge.exe。
    #[serde(default)]
    pub executable: String,
    /// 本机游戏账号的 UID。
    ///
    /// Miliastra-Wonderland-ChatBridge 用它过滤自己发出的消息，必须与游戏里登录的账号一致；
    /// 填错会导致自己的发言被当成玩家命令。
    #[serde(default)]
    pub self_uid: u32,
}

fn default_chat_source_url() -> String {
    "ws://127.0.0.1:18889/stream".to_string()
}

fn default_chat_source_reconnect_ms() -> u64 {
    3_000
}

fn default_chat_source_lease_ms() -> u64 {
    10_000
}

impl Default for ChatSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_chat_source_url(),
            token: String::new(),
            reconnect_ms: default_chat_source_reconnect_ms(),
            lease_timeout_ms: default_chat_source_lease_ms(),
            auto_start: false,
            executable: String::new(),
            self_uid: 0,
        }
    }
}

impl ChatSourceConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.url.trim().is_empty() {
            bail!("chat_source.url 在启用时不能为空");
        }
        if !self.url.starts_with("ws://") && !self.url.starts_with("wss://") {
            bail!("chat_source.url 必须是 ws:// 或 wss:// 地址");
        }
        if self.token.trim().is_empty() {
            // Miliastra-Wonderland-ChatBridge 会用这个令牌校验请求头；留空的结果是"一直连不上 + 401"，
            // 那种失败很难从日志里看出来，所以在保存时就挡住。
            bail!("chat_source.token 在启用时不能为空（应与 http.access_token 相同）");
        }
        if self.auto_start && self.self_uid == 0 {
            // UID 0 会让 chat 侧的"是否自己发的"判定永久为假：BOT 会把自己的
            // 发言当成玩家命令再执行一次，造成指令回环。
            bail!(
                "chat_source.self_uid 在自动启动时必须填写本机账号 UID（不能为 0），\
                 否则 Miliastra-Wonderland-ChatBridge 会把 BOT 自己发的消息当成命令"
            );
        }
        if self.reconnect_ms == 0 {
            bail!("chat_source.reconnect_ms 不能为 0");
        }
        if self.lease_timeout_ms == 0 {
            bail!("chat_source.lease_timeout_ms 不能为 0");
        }
        if self.auto_start && self.self_uid == 0 {
            bail!(
                "chat_source.auto_start 开启时必须填写 self_uid（本机游戏账号 UID），否则 Miliastra-Wonderland-ChatBridge 无法区分自己和玩家的消息"
            );
        }
        Ok(())
    }
}

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

/// 酷狗设备标识：GUID、设备型号与 MAC 由用户提供，用于登录与取流请求。
///
/// 三项必须同时填写；留空表示沿用凭据目录下已有的 kugou-device.json。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KugouDeviceConfig {
    #[serde(default)]
    pub device_guid: String,
    #[serde(default)]
    pub device_dev: String,
    #[serde(default)]
    pub device_mac: String,
}

impl KugouDeviceConfig {
    /// 酷狗设备信息允许整体留空（沿用已有 kugou-device.json），
    /// 但不允许只填一部分，避免半个设备身份进入登录协议。
    pub(crate) fn validate(&self) -> Result<()> {
        if self.is_empty() || self.is_complete() {
            return Ok(());
        }
        bail!("kugou.device_guid、kugou.device_dev、kugou.device_mac 必须同时填写");
    }

    /// 三项是否全部填写。
    pub fn is_complete(&self) -> bool {
        !self.device_guid.trim().is_empty()
            && !self.device_dev.trim().is_empty()
            && !self.device_mac.trim().is_empty()
    }

    /// 是否完全没有填写。
    pub fn is_empty(&self) -> bool {
        self.device_guid.trim().is_empty()
            && self.device_dev.trim().is_empty()
            && self.device_mac.trim().is_empty()
    }
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
            visual: visual::VisualConfig::default(),
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

impl BootstrapConfig {
    /// 从 config.yaml 加载最小启动配置。
    ///
    /// 严格解析（[`deny_unknown_fields`]）：只允许 database_path、http、logging 三个引导段。
    /// access_token 缺失时自动生成并持久化；database_path 为相对路径时相对
    /// `executable_root`（EXE 所在目录）解析为绝对路径。
    pub fn load(path: &Path, executable_root: &Path) -> Result<Self> {
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
            bootstrap.http.access_token = uuid::Uuid::new_v4().simple().to_string();
            let serialized = serde_norway::to_string(&bootstrap).context("序列化启动配置失败")?;
            write_atomic(path, serialized.as_bytes(), "启动配置")
                .with_context(|| format!("写入自动生成的访问令牌失败: {}", path.display()))?;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowCaptureMethod {
    /// 从目标窗口客户区 DC 读取，适合通常的窗口化/无边框运行。
    #[default]
    Gdi,
    /// 请求目标窗口自行绘制客户区；窗口被遮挡时可能有效，但部分游戏不支持。
    PrintWindow,
    /// 从桌面 DC 裁剪目标窗口客户区；要求窗口实际可见且未被遮挡。
    Desktop,
    /// 使用 Windows Graphics Capture 捕获指定 HWND（Windows 10 1903+）。
    Wgc,
}

impl WindowCaptureMethod {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Gdi => "GDI 窗口 DC",
            Self::PrintWindow => "PrintWindow",
            Self::Desktop => "桌面 DC 裁剪",
            Self::Wgc => "Windows Graphics Capture",
        }
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
    /// Windows 截图来源；缺失时维持旧版 GDI 行为。
    #[serde(default)]
    pub capture_method: WindowCaptureMethod,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            target_process: "yuanshen.exe,GenshinImpact.exe".to_string(),
            content_width: 1920,
            content_height: 1080,
            auto_activate_window: false,
            focus_point: PointConfig::new(1919, 1000),
            capture_method: WindowCaptureMethod::default(),
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
        self.kugou.validate()?;
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
            (
                self.moderation.confirm_button_region,
                "moderation.confirm_button_region",
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
    pub fn load(path: &Path) -> Result<Self> {
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

/// 把字符串里的 `/` 与 `\` 统一成给定分隔符。
///
/// 只在**路径类字段**上调用（`FieldKind::Path`），不要拿它处理任意字符串。
/// 保留 Windows 盘符前缀（`C:`）、UNC 前缀（`\\server`）、verbatim 前缀
/// （`\\?\`）；连续分隔符折叠为一个；尾随分隔符去掉；空串与纯文件名不变。
/// 纯函数、与平台无关，便于在 Linux 上直接回归 Windows 行为。
pub(crate) fn normalize_separators_text(value: &str, separator: char) -> String {
    let is_separator = |character: char| character == '/' || character == '\\';
    let characters: Vec<char> = value.chars().collect();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    if characters.len() >= 2 && is_separator(characters[0]) && is_separator(characters[1]) {
        // UNC（`\\server\share`）或 verbatim（`\\?\C:\...`）：前两个分隔符必须保留。
        output.push(separator);
        output.push(separator);
        index = 2;
        if index < characters.len() && characters[index] == '?' {
            output.push('?');
            index += 1;
            if index < characters.len() && is_separator(characters[index]) {
                output.push(separator);
                index += 1;
            }
        }
    } else if characters.len() >= 2 && characters[1] == ':' && characters[0].is_ascii_alphabetic() {
        // Windows 盘符前缀：`C:` 不是分隔符，不能把盘符后的相对路径当绝对路径。
        output.push(characters[0]);
        output.push(':');
        index = 2;
        if index < characters.len() && is_separator(characters[index]) {
            output.push(separator);
            index += 1;
        }
    } else if !characters.is_empty() && is_separator(characters[0]) {
        // 根路径（`/foo` 或 `\foo`）。
        output.push(separator);
        index = 1;
    }
    let mut pending_separator = false;
    for character in &characters[index..] {
        if is_separator(*character) {
            pending_separator = !output.is_empty();
        } else {
            if pending_separator {
                output.push(separator);
                pending_separator = false;
            }
            output.push(*character);
        }
    }
    output
}

/// 运行期路径规范化：Windows 上把 `/` 与 `\` 统一成 `\`；其他平台原样返回
/// （Unix 的反斜杠是合法文件名字符，不能改写）。保留盘符与绝对路径语义。
pub(crate) fn normalize_native_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    #[cfg(windows)]
    {
        PathBuf::from(normalize_separators_text(
            &path.to_string_lossy(),
            std::path::MAIN_SEPARATOR,
        ))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// 字符串形态的 [`normalize_native_path`]（Web 显示/保存复用同一实现）。
pub(crate) fn normalize_native_path_string(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        normalize_native_path(Path::new(value))
            .to_string_lossy()
            .into_owned()
    }
}

fn resolve_path(root: &Path, path: &mut PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    if path.is_relative() {
        *path = root.join(&*path);
    }
    // `PathBuf::join` 只是在根后原样拼接相对组件：相对值里若带 `/`，Windows 上
    // 会得到 `C:\root\deps/assets/x` 这种混合分隔符。这里统一成平台原生形式。
    *path = normalize_native_path(path);
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
pub(crate) mod ai;
/// 配置 schema 平滑迁移：旧版本字段改名/删除按键搬运，替代断代重置。
mod migrations;
mod schema;
pub(crate) use schema::*;

/// SQLite 配置中心：配置持久化存储（阶段 2），阶段 3 起由启动流程消费。
mod store;
pub(crate) use store::*;

/// Unified visual profile import/export over the existing SQLite fields.
pub(crate) mod visual;

/// 运行时可热更新配置的共享句柄集合（阶段 7）：保存成功后由 HTTP 层 apply，
/// 消费方在运行态读取共享值，使 schema 中标 Live 的字段真正生效。
mod live;
pub(crate) use live::*;

#[cfg(test)]
pub(crate) fn bundled_config_yaml() -> &'static str {
    // 仅供测试的完整配置随源码保存，保证干净检出也能编译单元测试。
    // 仓库与发布包的 config.yaml 已精简为最小启动配置（仅 bootstrap 字段）。
    include_str!("test.full.yaml")
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
    /// 二级聊天输入行右侧「发送」圆钮（深色圆底 + 黄色对勾）的搜索区。
    /// 它是**聊天专属锚点**：主菜单、主菜单大世界、千星多人面板、千星地图都没有这个按钮，
    /// 用来把只有左上圆形返回按钮的菜单/淡出过渡帧挡在 Secondary 之外。
    #[serde(default = "default_secondary_chat_submit_rect")]
    pub secondary_chat_submit_rect: RectConfig,
    /// 主菜单「返回提瓦特」按钮的搜索区。实测 (1690,1000) 205x60 可把主菜单
    /// 与其余 58 帧完全分开（见 technical-docs/ui-state-machine.md §35）。
    #[serde(default = "default_main_menu_rect")]
    pub main_menu_rect: RectConfig,
    /// 大厅底栏 👥 O 图标组的搜索区。
    #[serde(default = "default_hall_bar_rect")]
    pub hall_bar_rect: RectConfig,
    /// F2 千星大厅页右下「邀请玩家」按钮的搜索区。
    /// 左下「大厅列表」按钮复用 `hall_bar_rect`（两者都在底栏）；
    /// 只在这个窄区域里找右侧锚点，避免全帧或整条底栏匹配。
    #[serde(default = "default_qianxing_f2_invite_rect")]
    pub qianxing_f2_invite_rect: RectConfig,


    #[serde(default = "default_world_wish_rect")]
    pub world_wish_rect: RectConfig,
    pub hall_name_rect: RectConfig,
    pub hall_member_count_rect: RectConfig,
    pub hall_time_rect: RectConfig,
    pub hall_member_list_rect: RectConfig,
}


fn default_hall_bar_rect() -> RectConfig {
    // 底栏的横向位置会随聊天面板状态移动（实测同一模板在大厅帧上可出现在 x=180 或 x=369），
    // 所以搜索区必须够宽；过窄会把真实大厅帧漏掉（第 47 节踩过）。
    RectConfig {
        // 第 48 节改成"统计这段区域的近白像素占比"，而不是模板匹配。
        // 该量对位移和缩放都不敏感，正好绕开第 47 节栽的 1:1 SAD 限制。
        // 阈值就是在**这个区域**上量的，两边必须一致，不能随便改。
        x: 0,
        y: 985,
        width: 600,
        height: 95,
    }
}

fn default_qianxing_f2_invite_rect() -> RectConfig {
    // 「邀请玩家」纯文字模板（旧 canvas 240x50，现裁到字形 113x28）。
    // 28 帧（1920 与 1440p 归一化）实测命中左上角恒为 (1662,1003)，
    // 因此区域只保留 12px 定位余量，不再覆盖整条底栏右侧；原点与旧区域同奇偶。
    RectConfig {
        x: 1650,
        y: 993,
        width: 140,
        height: 54,
    }
}

fn default_main_menu_rect() -> RectConfig {
    // 按「返回提瓦特」整串文字外扩约 8px：模板本身只有这串字（137x25，不含定位图标），
    // 搜索区必须能容纳整块模板，否则匹配器直接返回空、主菜单又会漏判。
    // 上边界避开左侧图标，下边界停在 UID 行之前。
    RectConfig {
        x: 1668,
        y: 998,
        width: 153,
        height: 44,
    }
}

fn default_secondary_chat_submit_rect() -> RectConfig {
    // 模板是 52×52 的发送圆钮（深色圆底 + 黄色对勾）。实测在 round78-live.png /
    // secondary-full.png / sec-fixture.jpg 上命中位置恒为 (958,984)（三帧完全一致），
    // 因此搜索区只需 ±24px 余量；区域不能贴到画面底边（y+height ≤ 1080）。
    RectConfig {
        x: 930,
        y: 950,
        width: 150,
        height: 124,
    }
}

fn default_world_wish_rect() -> RectConfig {
    RectConfig {
        x: 1300,
        y: 0,
        width: 620,
        height: 110,
    }
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
            secondary_chat_submit_rect: default_secondary_chat_submit_rect(),
            main_menu_rect: default_main_menu_rect(),
            hall_bar_rect: default_hall_bar_rect(),
            qianxing_f2_invite_rect: default_qianxing_f2_invite_rect(),
            world_wish_rect: default_world_wish_rect(),
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
            (
                self.secondary_chat_submit_rect,
                "screen.secondary_chat_submit_rect",
            ),
            (self.main_menu_rect, "screen.main_menu_rect"),
            (self.hall_bar_rect, "screen.hall_bar_rect"),
            (
                self.qianxing_f2_invite_rect,
                "screen.qianxing_f2_invite_rect",
            ),
            (self.world_wish_rect, "screen.world_wish_rect"),
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
            // 门控阈值（6.0/0.03）吸收抖动后，低于阈值的真实变化只能靠这里兜底：2s 即其最大发现延迟。
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
    /// MNN/PaddleOCR 检测模型。仅选择 MNN 后端时必需；仅使用 OpenVINO 时可省略。
    #[serde(default)]
    pub det_model: Option<PathBuf>,
    /// MNN/PaddleOCR 识别模型。仅选择 MNN 后端时必需；仅使用 OpenVINO 时可省略。
    #[serde(default)]
    pub rec_model: Option<PathBuf>,
    pub charset: PathBuf,
    pub min_confidence: f32,
    pub threads: i32,
    pub request_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    /// OCR 后端优先级。默认 `openvino`、`cpu`：OpenVINO 优先，初始化失败时自动回退 MNN CPU。
    pub backend_priority: Vec<String>,
    /// 可选的 OpenVINO IR 模型配置。除非 `backend_priority` 包含 `openvino`，否则忽略。
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
    #[serde(default = "default_marker_search_width")]
    pub marker_search_width: u32,
    #[serde(default = "default_marker_dedupe_min_x")]
    pub marker_dedupe_min_x: i32,
    #[serde(default = "default_marker_dedupe_min_y")]
    pub marker_dedupe_min_y: i32,
    pub marker_dedupe_x: i32,
    pub marker_dedupe_y: i32,
    pub next_marker_min_gap: i32,
    pub right_padding: i32,
    pub batch_recognize: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVinoConfig {
    /// 导出为 OpenVINO IR 的检测模型 XML 文件。
    #[serde(default)]
    pub det_model: Option<PathBuf>,
    /// 与 `det_model` 配套的检测模型权重文件。
    #[serde(default)]
    pub det_weights: Option<PathBuf>,
    /// 导出为 OpenVINO IR 的识别模型 XML 文件。
    #[serde(default)]
    pub rec_model: Option<PathBuf>,
    /// 与 `rec_model` 配套的识别模型权重文件。
    #[serde(default)]
    pub rec_weights: Option<PathBuf>,
    /// 显式选择自训识别模型的固定输入 profile；目前仅支持 48x768、输出 stride 8。
    /// 省略或 null 保持官方模型的等比缩放 + bucket，不根据模型文件名猜测。
    #[serde(default)]
    pub rec_fixed_width: Option<u32>,
    /// OpenVINO 设备名称，通常为 `CPU`（安装相应插件后也可用 `GPU`/`NPU`）。
    #[serde(default = "default_openvino_device")]
    pub device: String,
    /// 持久化 OpenVINO 插件/模型缓存；设为 null 可禁用缓存。
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
            rec_fixed_width: None,
            device: default_openvino_device(),
            cache_dir: default_openvino_cache_dir(),
        }
    }
}

fn default_marker_search_width() -> u32 {
    60
}
fn default_marker_dedupe_min_x() -> i32 {
    22
}
fn default_marker_dedupe_min_y() -> i32 {
    14
}

impl Default for OcrConfig {
    fn default() -> Self {
        // 默认优先使用 OpenVINO（发布包内置运行时与 IR 模型），初始化失败时回退 MNN CPU；
        // CPU/MNN 后端要求 det_model/rec_model/charset 非空，因此两套模型都给出默认路径。
        // 默认路径使用发布布局 deps/models/，与 config.full.yaml 的文件名一致。
        Self {
            det_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_det.mnn")),
            rec_model: Some(PathBuf::from("deps/models/PP-OCRv6_small_rec.mnn")),
            charset: PathBuf::from("deps/models/ppocr_keys_v6_small.txt"),
            min_confidence: 0.9,
            threads: 4,
            request_timeout_ms: 10_000,
            shutdown_timeout_ms: 5_000,
            backend_priority: vec!["openvino".to_string(), "cpu".to_string()],
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
            marker_search_width: default_marker_search_width(),
            marker_dedupe_min_x: default_marker_dedupe_min_x(),
            marker_dedupe_min_y: default_marker_dedupe_min_y(),
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
        if self.rec_fixed_width.is_some_and(|width| width != 768) {
            bail!(
                "ocr.openvino.rec_fixed_width 仅支持 768（自训 48x768、输出 stride 8）；官方 bucket 请省略或设为 null"
            );
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
        if self.marker_search_width == 0 || self.marker_search_width > 10_000 {
            bail!("ocr.marker_search_width 必须在 1 到 10000 之间");
        }
        if !(0..=1000).contains(&self.marker_dedupe_min_x)
            || !(0..=1000).contains(&self.marker_dedupe_min_y)
        {
            bail!("ocr.marker_dedupe_min_x/y 必须在 0 到 1000 之间");
        }
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
        if self.det_max_side_len == 0 || self.det_min_area == 0 {
            bail!("OCR 检测尺寸必须大于 0");
        }
        // 配置 API 和数据库加载也必须执行表单声明的几何边界，不能依赖前端限制。
        for (value, min, max, field) in [
            (i64::from(self.det_box_border), 0, 100, "ocr.det_box_border"),
            (i64::from(self.text_left_gap), 0, 1000, "ocr.text_left_gap"),
            (
                i64::from(self.block_top_padding),
                0,
                1000,
                "ocr.block_top_padding",
            ),
            (
                i64::from(self.block_bottom_padding),
                0,
                1000,
                "ocr.block_bottom_padding",
            ),
            (
                i64::from(self.max_block_height),
                1,
                10_000,
                "ocr.max_block_height",
            ),
            (
                i64::from(self.same_line_y_tolerance),
                0,
                1000,
                "ocr.same_line_y_tolerance",
            ),
            (
                i64::from(self.marker_dedupe_x),
                0,
                1000,
                "ocr.marker_dedupe_x",
            ),
            (
                i64::from(self.marker_dedupe_y),
                0,
                1000,
                "ocr.marker_dedupe_y",
            ),
            (
                i64::from(self.next_marker_min_gap),
                0,
                10_000,
                "ocr.next_marker_min_gap",
            ),
            (i64::from(self.right_padding), 0, 1000, "ocr.right_padding"),
        ] {
            if !(min..=max).contains(&value) {
                bail!("{} 必须在 {} 到 {} 之间", field, min, max);
            }
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

#[cfg(test)]
mod ocr_geometry_validation_tests {
    use super::OcrConfig;

    #[test]
    fn ocr_geometry_controls_reject_negative_and_extreme_values() {
        for field in [
            "text_left_gap",
            "block_top_padding",
            "block_bottom_padding",
            "max_block_height",
            "same_line_y_tolerance",
            "marker_dedupe_x",
            "marker_dedupe_y",
            "next_marker_min_gap",
            "right_padding",
        ] {
            for invalid in [-1, i32::MIN, i32::MAX] {
                let mut value = serde_json::to_value(OcrConfig::default()).unwrap();
                value[field] = serde_json::json!(invalid);
                let config: OcrConfig = serde_json::from_value(value).unwrap();
                let error = config
                    .validate()
                    .expect_err("invalid OCR geometry accepted");
                assert!(
                    error.to_string().contains(&format!("ocr.{field}")),
                    "{error}"
                );
            }
        }
        let config = OcrConfig {
            det_box_border: u32::MAX,
            ..OcrConfig::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("ocr.det_box_border")
        );
    }

    #[test]
    fn ocr_geometry_defaults_and_boundaries_are_valid() {
        OcrConfig::default()
            .validate()
            .expect("default OCR geometry");
        for use_maximum in [false, true] {
            let mut value = serde_json::to_value(OcrConfig::default()).unwrap();
            for (field, minimum, maximum) in [
                ("det_box_border", 0, 100),
                ("text_left_gap", 0, 1000),
                ("block_top_padding", 0, 1000),
                ("block_bottom_padding", 0, 1000),
                ("max_block_height", 1, 10_000),
                ("same_line_y_tolerance", 0, 1000),
                ("marker_dedupe_x", 0, 1000),
                ("marker_dedupe_y", 0, 1000),
                ("next_marker_min_gap", 0, 10_000),
                ("right_padding", 0, 1000),
            ] {
                value[field] = serde_json::json!(if use_maximum { maximum } else { minimum });
            }
            let config: OcrConfig = serde_json::from_value(value).unwrap();
            config.validate().expect("valid OCR geometry boundary");
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateConfig {
    #[serde(default)]
    pub panel_downsample_factors: std::collections::BTreeMap<String, u32>,
    pub blue_marker: PathBuf,
    pub yellow_marker: PathBuf,
    pub pink_marker: PathBuf,
    pub secondary_back: PathBuf,
    pub secondary_hall: PathBuf,
    /// 千星奇域主菜单右下角的「返回提瓦特」按钮。命中即判定当前在奇域主菜单，
    /// 用来挡住 secondary_back 在 ESC 菜单上的误判（见 technical-docs/ui-state-machine.md §44）。
    #[serde(default = "default_main_menu_template")]
    pub main_menu: PathBuf,
    #[serde(default = "default_world_wish_template")]
    pub world_wish: PathBuf,
    pub invite_view_star: PathBuf,
    pub invite_goto_hall: PathBuf,
    pub invite_enter_hall: PathBuf,
    pub friend_panel: PathBuf,
    pub friend_search_panel: PathBuf,
    pub friend_more_settings: PathBuf,
    pub friend_block_chat: PathBuf,
    pub friend_blacklist: PathBuf,
    /// 通用「确认」按钮模板（65×34 纯文字「确认」），好友操作与启动流程共用。
    pub confirm_button: PathBuf,
    /// 确认弹窗文案关键词锚点（进入大厅 / 离开大厅 / 屏蔽玩家）。
    /// 确认弹窗必须「确认按钮 + 文案」双命中；文案只保留关键词，不含整句。
    #[serde(default = "default_confirm_message_random_hall_template")]
    pub confirm_message_random_hall: PathBuf,
    #[serde(default = "default_confirm_message_return_teyvat_template")]
    pub confirm_message_return_teyvat: PathBuf,
    #[serde(default = "default_confirm_message_block_template")]
    pub confirm_message_block: PathBuf,
    /// 二级聊天输入行右侧「发送」圆钮模板。与圆形返回按钮一起判二级：
    /// 只靠返回按钮会把奇域主菜单的淡出过渡帧误判成二级聊天。
    #[serde(default = "default_secondary_chat_submit_template")]
    pub secondary_chat_submit: PathBuf,
    /// F2 千星大厅页左下「大厅列表」按钮（图标 + 四字标签）。
    #[serde(default = "default_qianxing_f2_hall_list_template")]
    pub qianxing_f2_hall_list: PathBuf,
    /// F2 千星大厅页右下「邀请玩家」按钮（+ 图标 + 四字标签）。
    #[serde(default = "default_qianxing_f2_invite_player_template")]
    pub qianxing_f2_invite_player: PathBuf,
    /// 千星退出菜单锚点：只保留第一行「退出至登录界面」的单行纯文字模板。
    #[serde(default = "default_qianxing_exit_menu_template")]
    pub qianxing_exit_menu: PathBuf,
    /// 千星地图右下角锚点：右下角「千星奇域·大厅」文字 + 庙宇图标 + 红色角标。
    /// 只裁这一小段 UI（不含 UID、不含整条背景），是千星地图的**首选**判据；
    /// 整屏暗青比例只在锚点没命中时作为后备。
    #[serde(default = "default_qianxing_map_label_template")]
    pub qianxing_map_label: PathBuf,
    /// 个人资料页右下角「心愿单」三字形透明模板（bbox + 2px）。
    /// 个人资料页必须**两个按钮同时命中**才算，单个按钮不算（避免同位置白色胶囊按钮误判）。
    #[serde(default = "default_qianxing_profile_page_wishlist_template")]
    pub qianxing_profile_page_wishlist: PathBuf,
    /// 个人资料页右下角「衣橱」两字形透明模板（bbox + 2px），与「心愿单」成对使用。
    #[serde(default = "default_qianxing_profile_page_wardrobe_template")]
    pub qianxing_profile_page_wardrobe: PathBuf,
    /// 提瓦特资料卡左页「冒险等阶」纯文字锚点。与「世界等级」**同时命中**才判
    /// 提瓦特资料卡，避免其它米色面板误判。
    #[serde(default = "default_teyvat_profile_adventure_rank_template")]
    pub teyvat_profile_adventure_rank: PathBuf,
    /// 提瓦特资料卡左页「世界等级」纯文字锚点（与「冒险等阶」成对）。
    #[serde(default = "default_teyvat_profile_world_level_template")]
    pub teyvat_profile_world_level: PathBuf,
    /// 提瓦特资料卡右下角「添加好友」（深色按钮 + 白字）：命中即非好友；
    /// 未命中则是好友。
    #[serde(default = "default_teyvat_profile_add_friend_template")]
    pub teyvat_profile_add_friend: PathBuf,
    /// 千星资料卡上部「奇域等级」纯文字锚点：命中即千星资料卡。
    #[serde(default = "default_qianxing_profile_domain_level_template")]
    pub qianxing_profile_domain_level: PathBuf,
    /// 千星资料卡下部白色胶囊「添加好友」（深色字）：命中即非好友。
    #[serde(default = "default_qianxing_profile_add_friend_template")]
    pub qianxing_profile_add_friend: PathBuf,
    /// 千星资料卡下部白色胶囊「查看资料」（深色字）：无「添加好友」但命中它 → 好友。
    #[serde(default = "default_qianxing_profile_view_profile_template")]
    pub qianxing_profile_view_profile: PathBuf,
    pub marker_threshold: f32,
}

fn default_confirm_message_random_hall_template() -> PathBuf {
    PathBuf::from("deps/assets/confirm-message-random-hall.png")
}

fn default_confirm_message_return_teyvat_template() -> PathBuf {
    PathBuf::from("deps/assets/confirm-message-return-teyvat.png")
}

fn default_confirm_message_block_template() -> PathBuf {
    PathBuf::from("deps/assets/confirm-message-block.png")
}

fn default_secondary_chat_submit_template() -> PathBuf {
    PathBuf::from("deps/assets/ui-secondary-chat-submit.png")
}

fn default_qianxing_f2_hall_list_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-f2-hall-list.png")
}

fn default_qianxing_f2_invite_player_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-f2-invite-player.png")
}


fn default_qianxing_exit_menu_template() -> PathBuf { PathBuf::from("deps/assets/qianxing-exit-menu.png") }
fn default_qianxing_map_label_template() -> PathBuf { PathBuf::from("deps/assets/qianxing-map-label.png") }
fn default_qianxing_profile_page_wishlist_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-profile-page-wishlist.png")
}
fn default_qianxing_profile_page_wardrobe_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-profile-page-wardrobe.png")
}
fn default_teyvat_profile_adventure_rank_template() -> PathBuf {
    PathBuf::from("deps/assets/teyvat-profile-adventure-rank.png")
}
fn default_teyvat_profile_world_level_template() -> PathBuf {
    PathBuf::from("deps/assets/teyvat-profile-world-level.png")
}
fn default_teyvat_profile_add_friend_template() -> PathBuf {
    PathBuf::from("deps/assets/teyvat-profile-add-friend.png")
}
fn default_qianxing_profile_domain_level_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-profile-domain-level.png")
}
fn default_qianxing_profile_add_friend_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-profile-add-friend.png")
}
fn default_qianxing_profile_view_profile_template() -> PathBuf {
    PathBuf::from("deps/assets/invite-view-profile.png")
}

fn default_world_wish_template() -> PathBuf {
    PathBuf::from("deps/assets/world-wish.png")
}

fn default_main_menu_template() -> PathBuf {
    PathBuf::from("deps/assets/ui-main-menu-return.png")
}

impl Default for TemplateConfig {
    fn default() -> Self {
        // 模板路径默认使用发布布局 deps/assets/，文件名与
        // tests/fixtures/config.full.yaml 及 assets/ 目录一致；
        // 全部非空才能通过 validate() 的路径校验。
        Self {
            panel_downsample_factors: std::collections::BTreeMap::new(),
            blue_marker: PathBuf::from("deps/assets/chat-marker-blue.png"),
            yellow_marker: PathBuf::from("deps/assets/chat-marker-yellow.png"),
            pink_marker: PathBuf::from("deps/assets/chat-marker-pink.png"),
            secondary_back: PathBuf::from("deps/assets/ui-secondary-back.png"),
            secondary_hall: PathBuf::from("deps/assets/ui-secondary-hall.png"),
            main_menu: default_main_menu_template(),
            world_wish: default_world_wish_template(),
            invite_view_star: PathBuf::from("deps/assets/invite-view-star.png"),
            invite_goto_hall: PathBuf::from("deps/assets/invite-goto-hall.png"),
            invite_enter_hall: PathBuf::from("deps/assets/invite-enter-hall.png"),
            friend_panel: PathBuf::from("deps/assets/friend-panel.png"),
            friend_search_panel: PathBuf::from("deps/assets/friend-search-panel.png"),
            friend_more_settings: PathBuf::from("deps/assets/friend-more-settings.png"),
            friend_block_chat: PathBuf::from("deps/assets/friend-block-chat.png"),
            friend_blacklist: PathBuf::from("deps/assets/friend-blacklist.png"),
            confirm_button: PathBuf::from("deps/assets/confirm-button.png"),
            confirm_message_random_hall: default_confirm_message_random_hall_template(),
            confirm_message_return_teyvat: default_confirm_message_return_teyvat_template(),
            confirm_message_block: default_confirm_message_block_template(),
            secondary_chat_submit: default_secondary_chat_submit_template(),
            qianxing_f2_hall_list: default_qianxing_f2_hall_list_template(),
            qianxing_f2_invite_player: default_qianxing_f2_invite_player_template(),
            qianxing_exit_menu: default_qianxing_exit_menu_template(),
            qianxing_map_label: default_qianxing_map_label_template(),
            qianxing_profile_page_wishlist: default_qianxing_profile_page_wishlist_template(),
            qianxing_profile_page_wardrobe: default_qianxing_profile_page_wardrobe_template(),
            teyvat_profile_adventure_rank: default_teyvat_profile_adventure_rank_template(),
            teyvat_profile_world_level: default_teyvat_profile_world_level_template(),
            teyvat_profile_add_friend: default_teyvat_profile_add_friend_template(),
            qianxing_profile_domain_level: default_qianxing_profile_domain_level_template(),
            qianxing_profile_add_friend: default_qianxing_profile_add_friend_template(),
            qianxing_profile_view_profile: default_qianxing_profile_view_profile_template(),
            marker_threshold: 0.9,
        }
    }
}

impl TemplateConfig {
    fn validate(&self) -> Result<()> {
        validate_unit_interval(self.marker_threshold, "templates.marker_threshold")?;
        for (key, factor) in &self.panel_downsample_factors {
            if ![
                "friend_panel",
                "friend_search_panel",
                "friend_block_chat",
                "confirm_button",
                "invite_enter_hall",
                "invite_goto_hall",
                "invite_view_star",
            ]
            .contains(&key.as_str())
                || !(1..=4).contains(factor)
            {
                bail!("templates.panel_downsample_factors: 未知用途或倍数不在 1 到 4 之间: {key}");
            }
        }
        for (path, field) in [
            (&self.blue_marker, "templates.blue_marker"),
            (&self.yellow_marker, "templates.yellow_marker"),
            (&self.pink_marker, "templates.pink_marker"),
            (&self.secondary_back, "templates.secondary_back"),
            (&self.secondary_hall, "templates.secondary_hall"),
            (&self.world_wish, "templates.world_wish"),
            (&self.invite_view_star, "templates.invite_view_star"),
            (&self.invite_goto_hall, "templates.invite_goto_hall"),
            (&self.invite_enter_hall, "templates.invite_enter_hall"),
            (&self.friend_panel, "templates.friend_panel"),
            (&self.friend_search_panel, "templates.friend_search_panel"),
            (&self.friend_more_settings, "templates.friend_more_settings"),
            (&self.friend_block_chat, "templates.friend_block_chat"),
            (&self.friend_blacklist, "templates.friend_blacklist"),
            (&self.confirm_button, "templates.confirm_button"),
            (
                &self.confirm_message_random_hall,
                "templates.confirm_message_random_hall",
            ),
            (
                &self.confirm_message_return_teyvat,
                "templates.confirm_message_return_teyvat",
            ),
            (
                &self.confirm_message_block,
                "templates.confirm_message_block",
            ),
            (
                &self.secondary_chat_submit,
                "templates.secondary_chat_submit",
            ),
            (
                &self.qianxing_f2_hall_list,
                "templates.qianxing_f2_hall_list",
            ),
            (
                &self.qianxing_f2_invite_player,
                "templates.qianxing_f2_invite_player",
            ),
            (&self.qianxing_exit_menu, "templates.qianxing_exit_menu"),
            (&self.qianxing_map_label, "templates.qianxing_map_label"),
            (
                &self.qianxing_profile_page_wishlist,
                "templates.qianxing_profile_page_wishlist",
            ),
            (
                &self.qianxing_profile_page_wardrobe,
                "templates.qianxing_profile_page_wardrobe",
            ),
            (
                &self.teyvat_profile_adventure_rank,
                "templates.teyvat_profile_adventure_rank",
            ),
            (
                &self.teyvat_profile_world_level,
                "templates.teyvat_profile_world_level",
            ),
            (
                &self.teyvat_profile_add_friend,
                "templates.teyvat_profile_add_friend",
            ),
            (
                &self.qianxing_profile_domain_level,
                "templates.qianxing_profile_domain_level",
            ),
            (
                &self.qianxing_profile_add_friend,
                "templates.qianxing_profile_add_friend",
            ),
            (
                &self.qianxing_profile_view_profile,
                "templates.qianxing_profile_view_profile",
            ),
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
    #[serde(default)]
    pub access_token: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18888,
            enabled: true,
            access_token: "miliastra-default-access-token".to_string(),
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
        if self.access_token.trim().is_empty() {
            bail!("http.access_token 不能为空");
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
        for (value, field) in [
            (&self.pause_key, "hotkeys.pause_key"),
            (&self.exit_key, "hotkeys.exit_key"),
        ] {
            parse_hotkey_virtual_code(value)
                .map_err(|_| anyhow::anyhow!("{field} 仅支持 F1-F12 或单个 ASCII 字符"))?;
        }
        Ok(())
    }
}

pub(crate) fn parse_hotkey_virtual_code(key: &str) -> Result<u16> {
    let normalized = key.trim().to_ascii_uppercase();
    let code = match normalized.as_str() {
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        single if single.len() == 1 && single.is_ascii() => single.as_bytes()[0] as u16,
        _ => bail!("不支持的热键: {key}"),
    };
    Ok(code)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FriendDeliveryConfig {
    /// 已确认未发送消息的最大自动重试次数。
    pub auto_retry_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    type ConfigMutation = (&'static str, fn(&mut AppConfig));

    fn playback_timing(yaml: &str) -> PlaybackTimingConfig {
        serde_norway::from_str(yaml).expect("valid playback timing config")
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
        let error = serde_norway::from_str::<PlaybackTimingConfig>(
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
            serde_norway::from_str(bundled_config_yaml()).expect("default app config");
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
            serde_norway::from_str(bundled_config_yaml()).expect("default config");

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
    fn auto_start_requires_nonzero_self_uid() {
        // UID 0 会让 chat 侧的"是否自己发的"判定永久为假 → BOT 把自己的
        // 发言当命令再执行一次，形成指令回环。
        let mut config = AppConfig::default();
        config.chat_source.enabled = true;
        config.chat_source.token = "token".to_string();
        config.chat_source.auto_start = true;
        config.chat_source.self_uid = 0;
        let error = config.validate().expect_err("self_uid=0 必须被拒绝");
        assert!(error.to_string().contains("self_uid"), "实际: {error}");

        // 填上 UID 后通过。
        config.chat_source.self_uid = 900000001;
        config.validate().expect("合法配置应通过");

        // 不自动启动时 self_uid 可以为 0（自行部署由用户在自己命令行填）。
        config.chat_source.auto_start = false;
        config.chat_source.self_uid = 0;
        config.validate().expect("不自动启动时 self_uid 可为 0");
    }

    #[test]
    fn default_ocr_backend_prefers_openvino_with_cpu_fallback() {
        // 发布包只有一个 EXE：默认先试 OpenVINO，运行时加载失败会自动回落 MNN。
        // 精简包删掉 deps/openvino 后，同样靠这条回落链继续工作。
        let config = AppConfig::default();

        assert_eq!(
            config.ocr.backend_priority,
            vec!["openvino".to_string(), "cpu".to_string()]
        );
        assert!(config.ocr.det_model.is_some());
        assert!(config.ocr.rec_model.is_some());
        assert!(config.ocr.openvino.det_model.is_some());
        assert!(config.ocr.openvino.det_weights.is_some());
        assert!(config.ocr.openvino.rec_model.is_some());
        assert!(config.ocr.openvino.rec_weights.is_some());
        config
            .validate()
            .expect("默认 OpenVINO/MNN 双后端配置必须能通过完整校验");
    }

    #[test]
    fn openvino_rec_fixed_width_defaults_and_round_trips_explicit_profile() {
        assert_eq!(OpenVinoConfig::default().rec_fixed_width, None);
        let mut legacy = serde_json::to_value(OpenVinoConfig::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("rec_fixed_width");
        let legacy: OpenVinoConfig = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.rec_fixed_width, None);
        legacy.validate().unwrap();
        for width in [None, Some(768)] {
            let config = OpenVinoConfig {
                rec_fixed_width: width,
                ..legacy.clone()
            };
            let restored: OpenVinoConfig =
                serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
            assert_eq!(restored.rec_fixed_width, width);
            restored.validate().unwrap();
        }
    }

    #[test]
    fn startup_validation_rejects_unsupported_openvino_rec_fixed_width() {
        for width in [0, 1, 760, 769, 1024, u32::MAX] {
            let mut config = AppConfig::default();
            config.ocr.openvino.rec_fixed_width = Some(width);
            let error = config.validate().expect_err("only fixed768 is supported");
            assert!(error.to_string().contains("ocr.openvino.rec_fixed_width"));
        }
    }

    #[test]
    fn old_window_config_defaults_to_gdi_capture() {
        let config: WindowConfig = serde_json::from_value(serde_json::json!({
            "target_process": "game.exe",
            "content_width": 1920,
            "content_height": 1080,
            "auto_activate_window": false,
            "focus_point": { "x": 1919, "y": 1000 }
        }))
        .expect("old window config remains readable");
        assert_eq!(config.capture_method, WindowCaptureMethod::Gdi);
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

    /// 直接解析发布模板，避免测试读取或改写本机启动配置。
    fn bootstrap_config_yaml() -> BootstrapConfig {
        serde_norway::from_str(include_str!("../../config.example.yaml"))
            .expect("解析发布启动配置模板")
    }

    #[test]
    fn release_bootstrap_http_logging_match_defaults_without_a_shared_token() {
        let bootstrap = bootstrap_config_yaml();
        let defaults = AppConfig::default();

        assert_eq!(bootstrap.http.host, defaults.http.host);
        assert_eq!(bootstrap.http.port, defaults.http.port);
        assert_eq!(bootstrap.http.enabled, defaults.http.enabled);
        assert!(bootstrap.http.access_token.is_empty());
        assert!(bootstrap.http.validate().is_err());
        assert_eq!(
            serde_norway::to_value(&bootstrap.logging).expect("序列化 logging 段"),
            serde_norway::to_value(&defaults.logging).expect("序列化默认 logging 段"),
            "发布模板的 logging 段必须与 AppConfig::default() 一致"
        );
    }

    #[test]
    fn bootstrap_generates_and_preserves_a_token_when_empty_or_omitted() {
        let root = std::env::temp_dir().join(format!("bootstrap-token-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut previous_token = String::new();
        for omit_token in [false, true] {
            let mut value = serde_norway::to_value(bootstrap_config_yaml()).unwrap();
            if omit_token {
                value["http"]
                    .as_mapping_mut()
                    .unwrap()
                    .remove("access_token");
            }
            let config_path = root.join("config.yaml");
            fs::write(&config_path, serde_norway::to_string(&value).unwrap()).unwrap();
            let bootstrap = BootstrapConfig::load(&config_path, &root).unwrap();
            bootstrap.http.validate().unwrap();
            assert!(uuid::Uuid::parse_str(&bootstrap.http.access_token).is_ok());
            assert_ne!(bootstrap.http.access_token, previous_token);
            let reloaded = BootstrapConfig::load(&config_path, &root).unwrap();
            assert_eq!(bootstrap.http.access_token, reloaded.http.access_token);
            let persisted: BootstrapConfig =
                serde_norway::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
            assert!(persisted.database_path.is_relative());
            previous_token = bootstrap.http.access_token;
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[cfg(windows)]
    #[test]
    fn runtime_paths_resolve_against_the_executable_root_and_preserve_absolute_paths() {
        let root = Path::new(r"C:\发布目录");
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("默认配置应可解析");
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
            &config.templates.confirm_button,
            &config.playback.credential_directory,
            &config.playback.login_helper_executable,
            &config.logging.dir,
            &config.state.playback_state_path,
            &config.state.hall_state_path,
            &config.state.executed_commands_log_path,
            &config.song_dedup.history_path,
            &config.startup.templates.wonderland_map_star,
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
            let rendered = path.to_string_lossy();
            assert!(
                !rendered.contains(r"\/") && !rendered.contains(r"/\"),
                "解析后的路径出现混合分隔符: {rendered}"
            );
        }
        assert_eq!(
            config.startup.exe_path,
            PathBuf::from(r"D:\游戏\GenshinImpact.exe")
        );
        let workflow = &config.custom_workflows.workflows[0];
        assert_eq!(
            workflow.steps[0].template.as_deref(),
            Some(r"C:\发布目录\inline\button.png")
        );
        assert_eq!(workflow.steps[1].template.as_deref(), Some("按钮"));
    }

    /// 分隔符规范化是纯函数：Linux 上也要能回归 Windows 路径的最终形态。
    #[test]
    fn normalize_separators_text_unifies_windows_paths() {
        let cases = [
            (
                r"C:\Users\玩家甲\Desktop\deps/assets/idioms.txt",
                r"C:\Users\玩家甲\Desktop\deps\assets\idioms.txt",
            ),
            ("models/det.xml", r"models\det.xml"),
            (r"\\server\share/模板.png", r"\\server\share\模板.png"),
            (r"\\?\C:\a/b\c", r"\\?\C:\a\b\c"),
            (r"C:\a\\b", r"C:\a\b"),
            (r"C:/a/b/", r"C:\a\b"),
            (r"C:foo/bar", r"C:foo\bar"),
            ("plain.txt", "plain.txt"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_separators_text(input, '\\'),
                expected,
                "输入 {input:?}"
            );
        }
        // 正斜杠为目标时，Windows 反斜杠同样被统一（同一纯函数的对称用法）。
        assert_eq!(
            normalize_separators_text(r"C:\Users\玩家甲\deps\assets", '/'),
            "C:/Users/玩家甲/deps/assets"
        );
        // 幂等：规范化两次结果一致。
        let once = normalize_separators_text("a//b\\c", '\\');
        assert_eq!(normalize_separators_text(&once, '\\'), once);
    }

    #[test]
    fn normalize_native_path_is_idempotent_on_this_platform() {
        for raw in ["deps/assets/a.txt", "/tmp/x/y", "relative/name"] {
            let once = normalize_native_path(Path::new(raw));
            let twice = normalize_native_path(&once);
            assert_eq!(once, twice, "输入 {raw:?}");
        }
        assert_eq!(normalize_native_path(Path::new("")), PathBuf::new());
        assert_eq!(normalize_native_path_string(""), "");
    }

    /// 启动流程每个阶段的**上限**都不超过一分钟（用户明确要求）。
    ///
    /// 改动前最坏要 15 分钟：星标 120×2.5s + 星标消失 60s + 大厅 90×2s
    /// + 确认弹窗 60s + 确认稳定 60s + 主界面 120s。卡住时用户完全不知道
    /// 还要等多久，所以这里把各阶段预算钉死，避免以后调参时悄悄回涨。
    #[test]
    fn startup_stage_budgets_stay_under_one_minute() {
        const ONE_MINUTE_MS: u64 = 60_000;
        // 用内置默认配置（与发布包 config.yaml 同源），而不是 AppConfig::default()：
        // 后者是运行时内部默认，前者才是用户实际拿到的值。
        let config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("默认配置应可解析");
        let startup = &config.startup;

        // 启动游戏：等窗口的总上限。
        let launch_total = startup.launch_wait_ms * u64::from(startup.launch_retries.max(1));
        assert!(
            launch_total <= ONE_MINUTE_MS,
            "启动游戏阶段上限 {launch_total}ms 超过一分钟"
        );

        // 进入游戏：点击进入的等待上限。
        assert!(
            startup.enter_game_timeout_ms <= ONE_MINUTE_MS,
            "进入游戏阶段上限 {}ms 超过一分钟",
            startup.enter_game_timeout_ms
        );

        // 进入千星：各子阶段之和。主菜单大世界等待走 timing.workflow.default_timeout_ms。
        let star = u64::from(startup.wonderland_map_star_retries.max(1))
            * startup.wonderland_map_star_retry_ms.max(1);
        let hall = u64::from(startup.wonderland_hall_retries.max(1))
            * startup.wonderland_hall_retry_ms.max(1);
        let wonderland_total = config.timing.workflow.default_timeout_ms
            + star
            + startup.wonderland_transition_timeout_ms * 2
            + hall
            + startup.wonderland_confirm_stable_timeout_ms
            + startup.final_primary_timeout_ms;
        assert!(
            wonderland_total <= ONE_MINUTE_MS,
            "进入千星阶段上限 {wonderland_total}ms 超过一分钟"
        );
    }
    #[test]
    fn empty_optional_runtime_path_stays_empty() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("默认配置应可解析");
        config.startup.exe_path = PathBuf::new();

        config.resolve_runtime_paths(Path::new(r"C:\发布目录"));

        assert!(config.startup.exe_path.as_os_str().is_empty());
    }

    #[test]
    fn default_direction_workflows_click_middle_before_every_direction_key() {
        let config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");

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
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        config.window.target_process = " \t ".to_string();

        let error = config
            .validate()
            .expect_err("an empty target process must fail before runtime startup");

        assert!(error.to_string().contains("window.target_process"));
    }

    #[test]
    fn startup_validation_rejects_a_zero_ai_request_timeout() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
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
    fn lyrics_lead_seconds_accepts_only_finite_values_from_zero_to_sixty() {
        for valid in [0.0, 1.5, miliastra_playback::MAX_LYRICS_LEAD_SECONDS] {
            let mut config: AppConfig =
                serde_norway::from_str(bundled_config_yaml()).expect("default config");
            config.timing.playback.lyrics_lead_seconds = valid;
            config.validate().expect("valid lyrics lead seconds");
        }

        for invalid in [
            -0.001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            miliastra_playback::MAX_LYRICS_LEAD_SECONDS + 0.001,
        ] {
            let mut config: AppConfig =
                serde_norway::from_str(bundled_config_yaml()).expect("default config");
            config.timing.playback.lyrics_lead_seconds = invalid;
            let error = config
                .validate()
                .expect_err("invalid lyrics lead seconds must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("timing.playback.lyrics_lead_seconds"),
                "value={invalid:?} error={error}"
            );
        }
    }

    #[test]
    fn lyrics_lead_seconds_rejects_non_numeric_configuration_values() {
        for invalid in ["\"1.5\"", "[1.5]"] {
            let yaml = bundled_config_yaml().replace(
                "lyrics_lead_seconds: 0.0",
                &format!("lyrics_lead_seconds: {invalid}"),
            );
            let error = serde_norway::from_str::<AppConfig>(&yaml)
                .expect_err("non-numeric lyrics lead must be rejected");
            assert!(error.to_string().contains("lyrics_lead_seconds"));
        }
    }

    #[test]
    fn missing_lyrics_lead_seconds_defaults_to_zero_for_existing_configurations() {
        let mut value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("default config value");
        value["timing"]["playback"]
            .as_mapping_mut()
            .expect("playback timing mapping")
            .remove(serde_norway::Value::String(
                "lyrics_lead_seconds".to_string(),
            ));

        let config: AppConfig =
            serde_norway::from_value(value).expect("existing config without lyric lead");

        assert_eq!(config.timing.playback.lyrics_lead_seconds, 0.0);
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
                serde_norway::from_str(bundled_config_yaml()).expect("default config");
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
    fn partial_kugou_device_information_is_rejected() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        // 整体留空表示沿用已有 kugou-device.json。
        config.kugou = KugouDeviceConfig::default();
        config.validate().expect("empty device info is allowed");
        // 三项齐全时通过。
        config.kugou = KugouDeviceConfig {
            device_guid: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            device_dev: "550e8400e29b41d4a716446655440001".to_owned(),
            device_mac: "aa:bb:cc:dd:ee:ff".to_owned(),
        };
        config.validate().expect("complete device info is allowed");
        // 只填一部分必须被拒绝，避免半个设备身份进入登录协议。
        config.kugou.device_mac.clear();
        let error = config
            .validate()
            .expect_err("partial device info must fail");
        assert!(error.to_string().contains("kugou.device_mac"));
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
            ("templates.secondary_back", |config| {
                config.templates.secondary_back = PathBuf::new();
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
                serde_norway::from_str(bundled_config_yaml()).expect("default config");
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
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        // 夹具默认已配置 OpenVINO IR；这里显式清空，验证选中 openvino 后缺少 IR 路径会被拒绝。
        config.ocr.backend_priority = vec!["openvino".to_string()];
        config.ocr.openvino.det_model = None;
        config.ocr.openvino.det_weights = None;
        config.ocr.openvino.rec_model = None;
        config.ocr.openvino.rec_weights = None;
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
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
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
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        config.invite.friend_chat_region.x = config.screen.expected_width as i32;

        let error = config
            .validate()
            .expect_err("out-of-canvas UI region must fail before startup");

        assert!(error.to_string().contains("invite.friend_chat_region"));
    }

    #[test]
    fn startup_validation_rejects_an_empty_screen_region() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        config.screen.chat_rect.width = 0;

        let error = config
            .validate()
            .expect_err("an empty chat region must fail before runtime startup");

        assert!(error.to_string().contains("screen.chat_rect"));
    }

    #[test]
    fn startup_validation_rejects_a_zero_startup_poll_interval() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
        config.startup.poll_ms = 0;

        let error = config
            .validate()
            .expect_err("a zero startup poll interval must fail before runtime startup");

        assert!(error.to_string().contains("startup.poll_ms"));
    }

    #[test]
    fn startup_validation_rejects_invalid_thresholds_and_queue_capacity() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");

        config.templates.marker_threshold = 1.1;
        assert!(config.validate().is_err());

        config.templates.marker_threshold = 0.9;
        config.queue.max_size = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn startup_validation_rejects_cross_field_feature_invariants() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default config");
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
            let mut value: serde_norway::Value =
                serde_norway::from_str(bundled_config_yaml()).expect("default config value");
            value
                .as_mapping_mut()
                .expect("root mapping")
                .remove(serde_norway::Value::String(section.to_string()));

            let error = serde_norway::from_value::<AppConfig>(value)
                .expect_err("core top-level section must be required");

            assert!(
                error.to_string().contains(section),
                "section={section} error={error}"
            );
        }
        // 完整模板必须包含全部可拆段（供发布脚本提取）。
        let full: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("default config value");
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
    fn hotkey_parser_accepts_function_keys_and_single_ascii_characters() {
        assert_eq!(parse_hotkey_virtual_code("F1").unwrap(), 0x70);
        assert_eq!(parse_hotkey_virtual_code("f12").unwrap(), 0x7B);
        assert_eq!(parse_hotkey_virtual_code("a").unwrap(), b'A' as u16);
        assert_eq!(parse_hotkey_virtual_code("7").unwrap(), b'7' as u16);
    }

    #[test]
    fn hotkey_parser_rejects_invalid_values() {
        for value in ["", "F13", "AB", "中"] {
            assert!(
                parse_hotkey_virtual_code(value).is_err(),
                "应拒绝 {value:?}"
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
            "ai.extra_body",
            "turtle_soup.batch_max_parts",
            "startup.wonderland_map_star_retries",
            "startup.wonderland_map_star_retry_ms",
            "startup.wonderland_hall_retries",
            "startup.wonderland_hall_retry_ms",
            "startup.wonderland_transition_timeout_ms",
            "startup.wonderland_confirm_stable_timeout_ms",
            "startup.wonderland_confirm_threshold",
            "startup.wonderland_hall_ocr_region",
            "startup.wonderland_map_star_region",
            "startup.templates.wonderland_map_star",
            "startup.templates.paimon_menu",
        ] {
            let mut value: serde_norway::Value =
                serde_norway::from_str(bundled_config_yaml()).expect("default config value");
            let segments = path.split('.').collect::<Vec<_>>();
            let mut parent = &mut value;
            for segment in &segments[..segments.len() - 1] {
                parent = parent
                    .as_mapping_mut()
                    .expect("configuration path mapping")
                    .get_mut(serde_norway::Value::String((*segment).to_string()))
                    .expect("configuration path segment");
            }
            let field = segments.last().expect("configuration field");
            parent
                .as_mapping_mut()
                .expect("configuration field parent")
                .remove(serde_norway::Value::String((*field).to_string()));

            let error = serde_norway::from_value::<AppConfig>(value)
                .expect_err("current configuration field must be required");

            assert!(
                error.to_string().contains(field),
                "path={path} error={error}"
            );
        }
    }

    #[test]
    fn http_proxy_fields_default_when_omitted_from_existing_config() {
        let mut value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("default config value");
        for path in ["ai.http_proxy"] {
            let segments = path.split('.').collect::<Vec<_>>();
            let mut parent = &mut value;
            for segment in &segments[..segments.len() - 1] {
                parent = parent
                    .as_mapping_mut()
                    .expect("configuration path mapping")
                    .get_mut(serde_norway::Value::String((*segment).to_string()))
                    .expect("configuration path segment");
            }
            let field = segments.last().expect("configuration field");
            parent
                .as_mapping_mut()
                .expect("configuration field parent")
                .remove(serde_norway::Value::String((*field).to_string()));
        }

        let config: AppConfig = serde_norway::from_value(value)
            .expect("proxy fields are optional for existing configurations");

        assert!(config.ai.http_proxy.is_empty());
    }

    #[test]
    fn player_fast_observation_interval_stays_below_low_normal_intervals() {
        let mut config: AppConfig =
            serde_norway::from_str(bundled_config_yaml()).expect("default app config");

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
            serde_norway::from_str(bundled_config_yaml()).expect("default app config");
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
        let screen = serde_norway::from_str::<ScreenConfig>(
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

        let templates = serde_norway::from_str::<TemplateConfig>(
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
confirm_button: confirm.png
marker_threshold: 0.9
"#,
        )
        .expect_err("removed template alias must be rejected");
        assert!(templates.to_string().contains("enter"));

        let mut startup_value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("default config value");
        let startup = startup_value
            .get_mut("startup")
            .and_then(serde_norway::Value::as_mapping_mut)
            .expect("startup mapping");
        startup.remove(serde_norway::Value::String(
            "wonderland_map_star_retries".to_string(),
        ));
        startup.insert(
            serde_norway::Value::String("wonderland_home_retries".to_string()),
            serde_norway::Value::Number(120.into()),
        );
        let startup_error = serde_norway::from_value::<AppConfig>(startup_value)
            .expect_err("removed Wonderland startup aliases must be rejected");
        assert!(
            startup_error
                .to_string()
                .contains("wonderland_home_retries")
        );
    }

    fn inline_audio_cache_mapping(config: &mut serde_norway::Value) -> &mut serde_norway::Mapping {
        config
            .get_mut("playback")
            .and_then(|playback| playback.get_mut("audio_cache"))
            .and_then(serde_norway::Value::as_mapping_mut)
            .expect("内嵌音频缓存配置")
    }

    #[test]
    fn inline_audio_cache_loads_with_default_directory() {
        // bundled 完整配置中 audio_cache.directory 为空字符串：normalize 后
        // 回退到发布布局默认 deps/cache/audio。
        let directory =
            std::env::temp_dir().join(format!("config-cache-old-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("完整配置");
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_norway::to_string(&value).unwrap()).unwrap();

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
        let mut value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("完整配置");
        inline_audio_cache_mapping(&mut value).insert(
            serde_norway::Value::String("metadata_directory".to_string()),
            serde_norway::Value::String("deps/data".to_string()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_norway::to_string(&value).unwrap()).unwrap();

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
        let mut value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("完整配置");
        let audio_cache = inline_audio_cache_mapping(&mut value);
        audio_cache.insert(
            serde_norway::Value::String("directory".to_string()),
            serde_norway::Value::String("cache/audio".to_string()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_norway::to_string(&value).unwrap()).unwrap();

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
        let mut value: serde_norway::Value =
            serde_norway::from_str(bundled_config_yaml()).expect("完整配置");
        let audio_cache = inline_audio_cache_mapping(&mut value);
        audio_cache.insert(
            serde_norway::Value::String("directory".to_string()),
            serde_norway::Value::String(audio_directory.to_string_lossy().into_owned()),
        );
        let config_path = directory.join("config.yaml");
        std::fs::write(&config_path, serde_norway::to_string(&value).unwrap()).unwrap();

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
