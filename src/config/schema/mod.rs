//! 配置 schema 声明（阶段 4a）：Web 配置页按此生成表单。
//!
//! 每个字段声明点路径（与 AppConfig 各段结构一一对应）、中文显示名、
//! 表单控件类型、生效级别与来源。字段路径必须与 struct 字段名一致，
//! 字段类型与叶路径覆盖测试防止声明与配置结构脱节。
//! 阶段 5 起由 HTTP 配置接口（interfaces/http/routes/config.rs）消费。
//!
//! # 目录布局
//!
//! 本文件是 schema 的**汇总入口**：核心类型（[`ConfigFieldSchema`]、
//! [`ConfigSectionSchema`]、[`Effect`]、[`FieldKind`]、[`Domain`]、
//! [`SECRET_PATHS`]）与共享构造器、默认值提取函数都在这里；各业务域的段
//! 定义按用户任务分到子模块，[`config_sections`] 只负责按既有顺序汇总，
//! 保证前端展示顺序与拆分前逐字节一致。
//!
//! - [`general`]：窗口、屏幕区域、稳定性、输出、状态文件、全局热键；
//! - [`ui`]：外部聊天源、时序、OCR、模板图片、管理、启动流程、邀请流程、表情；
//! - [`playback`]：播放器、点歌队列、同歌去重、歌名匹配；
//! - [`ai`]：AI 连接、点歌 AI、歌曲审核、AI 聊天、语音 TTS；
//! - [`games`]：成语接龙、斗地主、谁是卧底、海龟汤、数字炸弹；
//! - [`system`]：Web/API 面板、日志、终端 TUI、诊断存档、好友投递、
//!   自定义流程、身份映射、酷狗设备。

use serde_json::Value;

mod ai;
mod games;
mod general;
mod playback;
mod system;
mod ui;

use ai::{ai_chat_section, ai_section, song_ai_section, song_review_section, tts_section};
use games::{
    idiom_chain_section, landlord_section, number_bomb_section, turtle_soup_section,
    undercover_section,
};
use general::{
    hotkeys_section, output_section, screen_section, stability_section, state_section,
    window_section,
};
use playback::{matching_section, playback_section, queue_section, song_dedup_section};
use system::{
    custom_workflows_section, diagnostics_section, friend_delivery_section, http_section,
    identity_section, kugou_section, logging_section, tui_section,
};
use ui::{
    chat_source_section, emoji_section, invite_section, moderation_section, ocr_section,
    startup_section, templates_section, timing_section,
};

use super::AppConfig;

/// 配置字段来源：数据库可编辑 / 启动引导（bootstrap，仅展示）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// 数据库可编辑
    Db,
    /// 启动引导（config.yaml）提供，Web 页面仅展示
    Bootstrap,
}

/// 生效级别：保存后立即生效 / 空闲时整程序重启 / 播放器空闲时整程序重启 /
/// 人工重启生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Effect {
    /// 保存后立即生效
    Live,
    /// 保存后等待运行态空闲；随后整个子进程以退出码 75（`CONFIG_RELOAD_EXIT_CODE`）退出，由 `main.rs` 的看门狗重新拉起（不是只重载组件）。
    IdleReload,
    /// 保存后等待运行态空闲，且播放器已闲置或处于可安全恢复的用户暂停状态，
    /// 再按 `IdleReload` 的方式重启整个子进程。
    PlaybackIdleReload,
    /// 只能在人工重启后生效（用于 config.yaml 启动引导字段）。
    Restart,
}

/// 字段类型（驱动 Web 表单控件）
#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    /// 自由字符串
    String,
    /// 多行文本（提示词等长文本）
    Text,
    /// 文件/目录路径（相对 EXE 根目录）
    Path,
    /// 枚举：值 + 中文说明
    Enum(Vec<(String, String)>),
    /// 数组（元素为字符串）
    StringArray,
    /// 嵌套对象（未知结构，高级 JSON 编辑）
    Object,
    /// 矩形区域 {x, y, width, height}
    Rect,
    /// 点 {x, y}
    Point,
    /// 敏感字段（API Key 等）：只显示掩码，空/掩码提交表示不修改
    Secret,
    /// 模型名：面板提供"从服务端列表选择"按钮，同时保留手填。
    ///
    /// 单独一种类型而不是复用 String：模型名可以远程获取，
    /// 但网关差异大、列表也可能拉取失败，所以必须同时允许手写。
    ModelName,
}

/// 需要脱敏的 secret 字段（点路径），**唯一权威定义**。
///
/// 脱敏仅用于展示（[`crate::config::ConfigStore::current_value`]），数据库与
/// 历史快照中始终保存明文（本地数据库）。含 `http.access_token`：该字段由启动
/// 引导（config.yaml）提供、不在配置库 sections 中，store 的保留规则
/// （`get_path` 找不到即跳过）天然兼容，无需特殊处理。
///
/// `store.rs` 的 `SECRET_PATHS` 引用本常量，禁止在别处重复定义。
pub const SECRET_PATHS: &[&str] = &[
    "http.access_token",
    "ai.api_key",
    "chat_source.token",
    "tts.api_key",
];

/// 配置字段 schema：路径、中文名、类型、生效级别、来源与提示。
#[derive(Clone, Debug)]

pub struct ConfigFieldSchema {
    /// 点路径，如 "ocr.det_model"
    pub path: String,
    /// 中文显示名
    pub label: String,
    pub kind: FieldKind,
    pub effect: Effect,
    pub source: ConfigSource,
    /// 中文说明/提示
    pub hint: String,
    /// 段内小节名（可选）。只给字段较多的大段用，
    /// 前端渲染成可折叠小节，避免 30+ 字段平铺。
    pub group: Option<&'static str>,
    /// 字段值可为 null（对应 struct 的 Option 字段；如 ocr.det_model、
    /// playback.audio_cache 本身）。表单允许提交 null 表示未启用。
    pub nullable: bool,
    /// 字段的父级可能为 null（如 audio_cache=null 时其 6 个子字段在结构中
    /// 仍存在但取值路径中间节点为 null）。测试取值时允许该路径返回 null。
    pub optional_parent: bool,
}

/// 配置段所属的功能域。
///
/// 配置页按域分组呈现：用户先想"我要改哪类东西"，再在该域里找具体段。
/// 域的划分依据是用户任务，不是代码模块——例如 ocr/templates/screen
/// 在代码里是三段，但对用户都是"识别与画面"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Domain {
    /// 识别与画面：视觉档案、窗口、截图区域、OCR、模板、稳定判定。
    Recognition,
    /// 播放与点歌：播放器、队列、去重、点歌 AI、歌曲审核。
    Playback,
    /// 娱乐玩法：成语接龙、斗地主、卧底、海龟汤、热键。
    Entertainment,
    /// AI 与交互：AI 连接、AI 聊天、输出、命令权限、身份、自定义流程。
    Interaction,
    /// 系统：面板、日志、TUI、状态、启动、管理、邀请、设备。
    System,
}

impl Domain {
    pub const ALL: [Self; 5] = [
        Self::Recognition,
        Self::Playback,
        Self::Entertainment,
        Self::Interaction,
        Self::System,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Recognition => "识别与画面",
            Self::Playback => "播放与点歌",
            Self::Entertainment => "娱乐玩法",
            Self::Interaction => "AI 与交互",
            Self::System => "系统",
        }
    }
}

/// 配置段 schema：段名（与 AppConfig 字段名一致）、中文分组名、顺序与字段列表。
#[derive(Clone, Debug)]
pub struct ConfigSectionSchema {
    /// 段名（与 AppConfig 字段名一致），如 "ocr"
    pub name: String,
    /// 中文分组名
    pub label: String,
    /// 所属功能域（面板按域分组呈现）
    pub domain: Domain,
    /// 域内显示顺序
    pub order: usize,
    pub fields: Vec<ConfigFieldSchema>,
}

/// 超时/间隔类字段上限（一天，单位毫秒）。
const MAX_TIMEOUT_MS: i64 = 86_400_000;

/// 稳定确认次数类字段上限。
const MAX_STABLE_COUNT: i64 = 1024;

impl ConfigFieldSchema {
    /// 数据库可编辑、保存后在运行态闲置时自动重载的字段。
    fn db_idle_reload(path: &str, label: &str, kind: FieldKind, hint: &str) -> Self {
        Self {
            path: path.to_string(),
            label: label.to_string(),
            kind,
            effect: Effect::IdleReload,
            source: ConfigSource::Db,
            hint: hint.to_string(),
            nullable: false,
            optional_parent: false,
            group: None,
        }
    }

    /// 与 [`Self::db_live`] 相同，但带面板分组标题。
    fn db_live_grouped(
        path: &str,
        label: &str,
        kind: FieldKind,
        hint: &str,
        group: &'static str,
    ) -> Self {
        let mut field = Self::db_live(path, label, kind, hint);
        field.group = Some(group);
        field
    }

    /// 数据库可编辑、保存后等待播放器闲置再自动重载的字段。
    fn db_playback_idle_reload(path: &str, label: &str, kind: FieldKind, hint: &str) -> Self {
        Self {
            path: path.to_string(),
            label: label.to_string(),
            kind,
            effect: Effect::PlaybackIdleReload,
            source: ConfigSource::Db,
            hint: hint.to_string(),
            nullable: false,
            optional_parent: false,
            group: None,
        }
    }

    /// 数据库可编辑、保存后立即生效字段（阶段 7 热更新接入）。
    /// 标 Live 的字段必须同时接入 [`crate::config::LiveConfigs`] 共享句柄与
    /// 消费方运行态读取点，由 live.rs 的一致性测试强制一一对应。
    fn db_live(path: &str, label: &str, kind: FieldKind, hint: &str) -> Self {
        Self {
            path: path.to_string(),
            label: label.to_string(),
            kind,
            effect: Effect::Live,
            source: ConfigSource::Db,
            hint: hint.to_string(),
            nullable: false,
            optional_parent: false,
            group: None,
        }
    }

    /// 启动引导（config.yaml）字段，仅展示，重启生效。
    fn bootstrap_restart(path: &str, label: &str, kind: FieldKind, hint: &str) -> Self {
        Self {
            path: path.to_string(),
            label: label.to_string(),
            kind,
            effect: Effect::Restart,
            source: ConfigSource::Bootstrap,
            hint: hint.to_string(),
            nullable: false,
            optional_parent: false,
            group: None,
        }
    }

    /// 标记字段值可为 null（对应 Option 字段）。
    fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// 标记字段的父级可能为 null（父级 Option 未启用时子字段仍视为存在）。
    fn optional_parent(mut self) -> Self {
        self.optional_parent = true;
        self
    }
}

/// 整数控件。
fn int(min: i64, max: i64) -> FieldKind {
    FieldKind::Int { min, max }
}

/// 浮点控件。
fn float(min: f64, max: f64) -> FieldKind {
    FieldKind::Float { min, max }
}

/// 枚举控件：值 + 中文说明。
fn enum_of(pairs: &[(&str, &str)]) -> FieldKind {
    FieldKind::Enum(
        pairs
            .iter()
            .map(|(value, label)| (value.to_string(), label.to_string()))
            .collect(),
    )
}

/// 给段内字段路径统一加段前缀（如 "target_process" -> "window.target_process"），
/// 保证字段声明处只写相对路径、最终 path 与配置 JSON 结构一一对应。

fn with_section_prefix(
    section: &str,
    mut fields: Vec<ConfigFieldSchema>,
) -> Vec<ConfigFieldSchema> {
    for field in &mut fields {
        field.path = format!("{section}.{}", field.path);
    }
    fields
}

/// 全部配置段 schema（与 AppConfig 顶层段一一对应，含 bootstrap 的 http/logging）。
/// 段内小节：按字段路径前缀推导，避免在 300+ 个声明点逐个手写。
///
/// 只有字段数较多的段才值得分子节；小节名面向用户，所以用中文，
/// 且按"用户改它时的意图"命名，不照搬代码里的路径前缀。

fn field_group(section: &str, path: &str) -> Option<&'static str> {
    // 去掉段前缀，只看段内路径。
    let inner = path.strip_prefix(section)?.strip_prefix('.')?;
    let head = inner.split('.').next().unwrap_or(inner);
    match (section, head) {
        // 时序：按子系统分节，User 找的是"聊天扫描还是命令执行"。
        ("timing", "watchdog_restart_ms" | "loop_idle_ms") => Some("主循环"),
        ("timing", "chat_scan") => Some("聊天扫描"),
        ("timing", "command") => Some("命令执行"),
        ("timing", "input") => Some("输入等待"),
        ("timing", "invite") => Some("邀请流程"),
        ("timing", "workflow") => Some("自定义流程"),
        ("timing", "hall") => Some("大厅"),
        ("timing", "moderation") => Some("管理投票"),
        ("timing", "playback") => Some("播放器"),
        ("timing", "decision") => Some("决策窗口"),
        ("timing", "external") => Some("外部程序"),
        // OCR：检测参数、识别参数、聊天几何三者改动场景不同。
        ("ocr", "det_model" | "rec_model" | "charset" | "min_confidence" | "threads") => {
            Some("模型与置信度")
        }
        (
            "ocr",
            "det_max_side_len"
            | "det_score_threshold"
            | "det_unclip_ratio"
            | "det_min_area"
            | "det_box_border",
        ) => Some("检测参数"),
        ("ocr", "request_timeout_ms" | "shutdown_timeout_ms" | "backend_priority") => {
            Some("运行控制")
        }
        ("ocr", "openvino") => Some("OpenVINO"),
        (
            "ocr",
            "marker_search_width"
            | "marker_dedupe_min_x"
            | "marker_dedupe_min_y"
            | "marker_dedupe_x"
            | "marker_dedupe_y",
        ) => Some("气泡定位"),
        (
            "ocr",
            "text_left_gap"
            | "block_top_padding"
            | "block_bottom_padding"
            | "max_block_height"
            | "same_line_y_tolerance"
            | "next_marker_min_gap"
            | "right_padding",
        ) => Some("文本排版"),
        ("ocr", "change_mean_threshold" | "change_pixel_threshold") => Some("变化检测"),
        // 启动流程：截图判定 vs 点击导航。
        (
            "startup",
            "stable_mean_threshold"
            | "stable_changed_ratio_threshold"
            | "stable_frame_count"
            | "settle_ms",
        ) => Some("稳定判定"),
        // AI 聊天：协议块 / 工具描述 / 长文本提示词各自成节。
        ("ai_chat", "protocols") => Some("正文协议"),
        ("ai_chat", "tool_descriptions") => Some("工具描述"),
        ("ai_chat", "tool_prompts") => Some("工具提示词"),
        ("ai_chat", "vision") => Some("识图"),
        ("ai_chat", "ai_reply") => Some("AI 回复"),
        ("ai_chat", "summary_prompt" | "vision_prompt") => Some("长文本提示词"),
        _ => None,
    }
}

pub fn config_sections() -> Vec<ConfigSectionSchema> {
    let mut sections = vec![
        ConfigSectionSchema {
            name: "visual".to_string(),
            label: "视觉档案".to_string(),
            domain: Domain::Recognition,
            order: 0,
            // 字段表里只保留 `visual.name`：它仍是 Live 字段（档案改名要热更新），
            // 且 LiveConfigs 的一致性测试要求 schema 与热更新路径一一对应。
            // 面板不会把它渲染成普通输入框——该段整体由档案编辑器接管，
            // 名称在档案里改。
            fields: with_section_prefix(
                "visual",
                vec![ConfigFieldSchema::db_live(
                    "name",
                    "档案名称",
                    FieldKind::String,
                    "默认 1080P；在下方视觉档案编辑器里修改",
                )],
            ),
        },
        ConfigSectionSchema {
            name: "command_access".to_string(),
            label: "命令权限".to_string(),
            domain: Domain::Interaction,
            order: 33,
            fields: with_section_prefix(
                "command_access",
                vec![
                    ConfigFieldSchema::db_live(
                        "overrides",
                        "命令权限策略",
                        FieldKind::Object,
                        "命令键到启用状态与最低身份的覆盖：user、friend、admin、owner；保存后立即生效",
                    ),
                    ConfigFieldSchema::db_live(
                        "default_song_source",
                        "默认点歌平台",
                        FieldKind::String,
                        "普通@点歌使用的平台；请在命令面板选择，实际平台权限仍会检查",
                    )
                    .nullable(),
                ],
            ),
        },
        ConfigSectionSchema {
            name: "chat_source".to_string(),
            label: "外部聊天源".to_string(),
            domain: Domain::Interaction,
            order: 34,
            fields: with_section_prefix("chat_source", chat_source_section()),
        },
        ConfigSectionSchema {
            name: "window".to_string(),
            label: "窗口".to_string(),
            domain: Domain::Recognition,
            order: 1,
            fields: with_section_prefix("window", window_section()),
        },
        ConfigSectionSchema {
            name: "screen".to_string(),
            label: "屏幕区域".to_string(),
            domain: Domain::Recognition,
            order: 2,
            fields: with_section_prefix("screen", screen_section()),
        },
        ConfigSectionSchema {
            name: "stability".to_string(),
            label: "稳定性".to_string(),
            domain: Domain::Recognition,
            order: 3,
            fields: with_section_prefix("stability", stability_section()),
        },
        ConfigSectionSchema {
            name: "timing".to_string(),
            label: "时序".to_string(),
            domain: Domain::System,
            order: 4,
            fields: with_section_prefix("timing", timing_section()),
        },
        ConfigSectionSchema {
            name: "ocr".to_string(),
            label: "OCR".to_string(),
            domain: Domain::Recognition,
            order: 5,
            fields: with_section_prefix("ocr", ocr_section()),
        },
        ConfigSectionSchema {
            name: "templates".to_string(),
            label: "模板图片".to_string(),
            domain: Domain::Recognition,
            order: 6,
            fields: with_section_prefix("templates", templates_section()),
        },
        ConfigSectionSchema {
            name: "output".to_string(),
            label: "输出".to_string(),
            domain: Domain::Interaction,
            order: 7,
            fields: with_section_prefix("output", output_section()),
        },
        ConfigSectionSchema {
            name: "moderation".to_string(),
            label: "管理（拉黑/屏蔽）".to_string(),
            domain: Domain::System,
            order: 8,
            fields: with_section_prefix("moderation", moderation_section()),
        },
        ConfigSectionSchema {
            name: "playback".to_string(),
            label: "播放器".to_string(),
            domain: Domain::Playback,
            order: 9,
            fields: with_section_prefix("playback", playback_section()),
        },
        ConfigSectionSchema {
            name: "http".to_string(),
            label: "Web/API 面板".to_string(),
            domain: Domain::System,
            order: 10,
            fields: with_section_prefix("http", http_section()),
        },
        ConfigSectionSchema {
            name: "logging".to_string(),
            label: "日志".to_string(),
            domain: Domain::System,
            order: 11,
            fields: with_section_prefix("logging", logging_section()),
        },
        ConfigSectionSchema {
            name: "tui".to_string(),
            label: "终端 TUI".to_string(),
            domain: Domain::System,
            order: 12,
            fields: with_section_prefix("tui", tui_section()),
        },
        ConfigSectionSchema {
            name: "state".to_string(),
            label: "状态文件".to_string(),
            domain: Domain::System,
            order: 13,
            fields: with_section_prefix("state", state_section()),
        },
        ConfigSectionSchema {
            name: "diagnostics".to_string(),
            label: "诊断存档".to_string(),
            domain: Domain::System,
            order: 44,
            fields: with_section_prefix("diagnostics", diagnostics_section()),
        },
        ConfigSectionSchema {
            name: "queue".to_string(),
            label: "点歌队列".to_string(),
            domain: Domain::Playback,
            order: 14,
            fields: with_section_prefix("queue", queue_section()),
        },
        ConfigSectionSchema {
            name: "song_dedup".to_string(),
            label: "同歌去重".to_string(),
            domain: Domain::Playback,
            order: 15,
            fields: with_section_prefix("song_dedup", song_dedup_section()),
        },
        ConfigSectionSchema {
            name: "idiom_chain".to_string(),
            label: "成语接龙".to_string(),
            domain: Domain::Entertainment,
            order: 16,
            fields: with_section_prefix("idiom_chain", idiom_chain_section()),
        },
        ConfigSectionSchema {
            name: "landlord".to_string(),
            label: "斗地主".to_string(),
            domain: Domain::Entertainment,
            order: 17,
            fields: with_section_prefix("landlord", landlord_section()),
        },
        ConfigSectionSchema {
            name: "undercover".to_string(),
            label: "谁是卧底".to_string(),
            domain: Domain::Entertainment,
            order: 18,
            fields: with_section_prefix("undercover", undercover_section()),
        },
        ConfigSectionSchema {
            name: "turtle_soup".to_string(),
            label: "海龟汤".to_string(),
            domain: Domain::Entertainment,
            order: 19,
            fields: with_section_prefix("turtle_soup", turtle_soup_section()),
        },
        ConfigSectionSchema {
            name: "number_bomb".to_string(),
            label: "数字炸弹".to_string(),
            domain: Domain::Entertainment,
            order: 31,
            fields: with_section_prefix("number_bomb", number_bomb_section()),
        },
        ConfigSectionSchema {
            name: "ai".to_string(),
            label: "AI 连接".to_string(),
            domain: Domain::Interaction,
            order: 20,
            fields: with_section_prefix("ai", ai_section()),
        },
        ConfigSectionSchema {
            name: "song_ai".to_string(),
            label: "点歌 AI".to_string(),
            domain: Domain::Playback,
            order: 21,
            fields: with_section_prefix("song_ai", song_ai_section()),
        },
        ConfigSectionSchema {
            name: "song_review".to_string(),
            label: "歌曲审核".to_string(),
            domain: Domain::Playback,
            order: 22,
            fields: with_section_prefix("song_review", song_review_section()),
        },
        ConfigSectionSchema {
            name: "matching".to_string(),
            label: "歌名匹配".to_string(),
            domain: Domain::Recognition,
            order: 23,
            fields: with_section_prefix("matching", matching_section()),
        },
        ConfigSectionSchema {
            name: "hotkeys".to_string(),
            label: "全局热键".to_string(),
            domain: Domain::Entertainment,
            order: 24,
            fields: with_section_prefix("hotkeys", hotkeys_section()),
        },
        ConfigSectionSchema {
            name: "startup".to_string(),
            label: "启动流程".to_string(),
            domain: Domain::System,
            order: 25,
            fields: with_section_prefix("startup", startup_section()),
        },
        ConfigSectionSchema {
            name: "invite".to_string(),
            label: "邀请流程".to_string(),
            domain: Domain::System,
            order: 26,
            fields: with_section_prefix("invite", invite_section()),
        },
        ConfigSectionSchema {
            name: "friend_delivery".to_string(),
            label: "好友投递".to_string(),
            domain: Domain::System,
            order: 27,
            fields: with_section_prefix("friend_delivery", friend_delivery_section()),
        },
        ConfigSectionSchema {
            name: "custom_workflows".to_string(),
            label: "自定义流程".to_string(),
            domain: Domain::Interaction,
            order: 28,
            fields: with_section_prefix("custom_workflows", custom_workflows_section()),
        },
        ConfigSectionSchema {
            name: "identity".to_string(),
            label: "身份映射".to_string(),
            domain: Domain::Interaction,
            order: 29,
            fields: with_section_prefix("identity", identity_section()),
        },
        ConfigSectionSchema {
            name: "kugou".to_string(),
            label: "酷狗设备".to_string(),
            domain: Domain::System,
            order: 30,
            fields: with_section_prefix("kugou", kugou_section()),
        },
        ConfigSectionSchema {
            name: "ai_chat".to_string(),
            label: "AI 聊天".to_string(),
            domain: Domain::Interaction,
            order: 32,
            fields: with_section_prefix("ai_chat", ai_chat_section()),
        },
        ConfigSectionSchema {
            name: "tts".to_string(),
            label: "语音 TTS".to_string(),
            domain: Domain::Interaction,
            order: 35,
            fields: with_section_prefix("tts", tts_section()),
        },
        ConfigSectionSchema {
            name: "emoji".to_string(),
            label: "二级聊天表情".to_string(),
            domain: Domain::Interaction,
            order: 36,
            fields: with_section_prefix("emoji", emoji_section()),
        },
    ];
    // 统一补段内小节：声明点不必逐个手写 group。
    for section in &mut sections {
        let name = section.name.clone();
        for field in &mut section.fields {
            field.group = field_group(&name, &field.path);
        }
    }
    sections
}

/// 单段 schema。

pub fn section_schema(name: &str) -> Option<ConfigSectionSchema> {
    config_sections()
        .into_iter()
        .find(|section| section.name == name)
}

/// 查询点路径对应的生效级别。Object/Rect/Point 的路径继承最深的
/// schema 父字段；未声明路径返回 None。
pub(crate) fn config_effect_for_path(path: &str) -> Option<Effect> {
    config_sections()
        .into_iter()
        .flat_map(|section| section.fields)
        .filter(|field| {
            field.path == path
                || (matches!(
                    field.kind,
                    FieldKind::Object | FieldKind::Rect | FieldKind::Point
                ) && path
                    .strip_prefix(&field.path)
                    .is_some_and(|suffix| suffix.starts_with('.')))
        })
        .max_by_key(|field| field.path.len())
        .map(|field| field.effect)
}

/// 内置默认配置的完整 JSON（AppConfig::default() 序列化，含 http/logging/state）。
pub fn default_config_json() -> Value {
    let mut value = serde_json::to_value(AppConfig::default()).expect("AppConfig 序列化不应失败");
    normalize_default_numbers(&mut value);
    value
}

/// 把 f32 默认值还原成人类可读的最短十进制。
///
/// `AppConfig` 里很多阈值是 f32，序列化成 f64 会变成 0.8999999761581421；
/// 面板「默认值」展示和用户手填的 0.9 对比时会被误判成改过。这里用最短
/// f32 表示（0.9）替换，只在回读成 f32 仍等价时生效。
pub fn normalize_default_numbers(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for item in map.values_mut() {
                normalize_default_numbers(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_default_numbers(item);
            }
        }
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                return;
            }
            let Some(current) = number.as_f64() else {
                return;
            };
            let as_f32 = current as f32;
            if !as_f32.is_finite() {
                return;
            }
            for precision in 1..=9 {
                let candidate = format!("{as_f32:.precision$}", precision = precision);
                let Ok(parsed) = candidate.parse::<f64>() else {
                    continue;
                };
                if parsed as f32 == as_f32 {
                    if parsed.to_bits() != current.to_bits() {
                        if let Some(replacement) = serde_json::Number::from_f64(parsed) {
                            *number = replacement;
                        }
                    }
                    return;
                }
            }
        }
        _ => {}
    }
}

/// 与 [`default_config_json`] 相同，但 audio_cache 替换为启用后的默认对象：
/// 内置默认中 audio_cache 为 null（不启用），其子字段无法提取默认值；
/// Web 表单「启用」音频缓存时需要这些子字段默认值预填。
/// 数值与运行时（crates/miliastra-playback AudioCacheConfig::default）一致：
/// 上限 20 GiB、并发 2、请求超时 15s、跳转等待 10s；目录为发布布局相对路径。
pub fn default_config_json_with_audio_cache() -> Value {
    let mut value = default_config_json();
    if let Some(playback) = value
        .as_object_mut()
        .and_then(|object| object.get_mut("playback"))
        .and_then(Value::as_object_mut)
    {
        playback.insert(
            "audio_cache".to_string(),
            serde_json::json!({
                "enabled": true,
                "directory": "deps/cache/audio",
                "max_bytes_mb": 20 * 1024,
                "max_concurrent_downloads": 2,
                "request_timeout_ms": 15_000,
                "seek_wait_timeout_ms": 10_000,
            }),
        );
    }
    value
}

#[cfg(test)]
mod tests_snapshot;

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;
    use serde_json::json;

    /// schema 全部字段路径 → (nullable, optional_parent) 映射，供 json_path 判断
    /// 中间节点为 null 时该路径是否允许。
    fn schema_nullability() -> HashMap<String, (bool, bool)> {
        config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .map(|field| (field.path, (field.nullable, field.optional_parent)))
            .collect()
    }

    /// schema 全部字段路径 → 类型映射（供叶路径枚举判断 Object/Rect/Point 不深入）。
    fn schema_kinds() -> HashMap<String, FieldKind> {
        config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .map(|field| (field.path, field.kind))
            .collect()
    }

    /// 按点路径在 JSON 中取值；中间节点必须是对象，路径不存在时返回 None。
    /// 中间节点为 null（Option 未启用，如默认 audio_cache）时：仅当该路径在
    /// schema 中声明 nullable 或 optional_parent 才视为字段存在（返回该 null），
    /// 否则 panic——防止「null 提前返回」掩盖声明与结构脱节。
    fn json_path<'a>(
        root: &'a Value,
        path: &str,
        nullability: &HashMap<String, (bool, bool)>,
    ) -> Option<&'a Value> {
        let segments: Vec<&str> = path.split('.').collect();
        let mut current = root;
        for (index, segment) in segments.iter().enumerate() {
            if current.is_null() {
                let parent_path = segments[..index].join(".");
                if nullability
                    .get(&parent_path)
                    .is_some_and(|(nullable, optional_parent)| *nullable || *optional_parent)
                {
                    return Some(current);
                }
                panic!(
                    "字段 {path} 的中间节点 {parent_path} 为 null，但 schema 未声明 nullable/optional_parent"
                );
            }
            current = match current.as_object()?.get(*segment) {
                Some(value) => value,
                None if index + 1 == segments.len()
                    && nullability.get(path).is_some_and(|(nullable, _)| *nullable) =>
                {
                    return Some(&Value::Null);
                }
                None => return None,
            };
        }
        Some(current)
    }

    /// 按点路径写入 JSON 值；路径必须已存在（中间节点为对象）。
    fn set_path(root: &mut Value, path: &str, value: Value) {
        let segments: Vec<&str> = path.split('.').collect();
        let mut current = root;
        for segment in &segments[..segments.len() - 1] {
            current = current
                .as_object_mut()
                .unwrap_or_else(|| panic!("路径 {path} 的中间节点不是对象"))
                .get_mut(*segment)
                .unwrap_or_else(|| panic!("路径 {path} 的中间节点 {} 不存在", segment));
        }
        current
            .as_object_mut()
            .expect("目标父节点必须是对象")
            .insert(segments[segments.len() - 1].to_string(), value);
    }

    /// 递归枚举 JSON 叶路径：对象键逐层拼接；schema 声明为 Object/Rect/Point 且
    /// 无子字段声明的路径整体计为叶（如 ai.extra_body、Rect/Point 内部键不进
    /// schema）；有子字段声明的容器（如 playback.audio_cache）自身也是叶路径，
    /// 同时递归深入子字段；数组整体计为叶（不追踪索引路径）。
    fn collect_leaf_paths(
        value: &Value,
        prefix: &str,
        kinds: &HashMap<String, FieldKind>,
        schema_paths: &BTreeSet<String>,
        out: &mut BTreeSet<String>,
    ) {
        match value {
            Value::Object(map) => {
                let has_declared_children = schema_paths
                    .iter()
                    .any(|path| path.starts_with(&format!("{prefix}.")));
                if !has_declared_children
                    && matches!(
                        kinds.get(prefix),
                        Some(FieldKind::Object | FieldKind::Rect | FieldKind::Point)
                    )
                {
                    out.insert(prefix.to_string());
                    return;
                }
                // 容器自身在 schema 中声明（如 playback.audio_cache）时，自身也是叶路径。
                if schema_paths.contains(prefix) {
                    out.insert(prefix.to_string());
                }
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_leaf_paths(child, &path, kinds, schema_paths, out);
                }
            }
            _ => {
                out.insert(prefix.to_string());
            }
        }
    }

    /// 带音频缓存样例的配置 JSON：默认 audio_cache 为 null，无法覆盖其子路径，
    /// 用启用后的默认对象补一个完整的 AudioCacheFileConfig 再序列化。
    /// 与正式函数共用同一份默认值（Web「启用」预填来源）。
    fn sample_config_json_with_audio_cache() -> Value {
        default_config_json_with_audio_cache()
    }

    /// JSON 中存在但 schema 不声明的注入字段（由启动引导提供，不进配置库）：
    /// state.playback_state_path 由统一数据库路径注入；http/logging 段 schema
    /// 已声明（bootstrap 仅展示），无需豁免。
    /// 由启动引导注入、不出现在 schema 字段里的路径。
    const INJECTED_JSON_PATHS: &[&str] = &["state.playback_state_path"];

    #[test]
    fn effect_names_are_camel_case_in_json() {
        assert_eq!(serde_json::to_value(Effect::Live).unwrap(), json!("live"));
        assert_eq!(
            serde_json::to_value(Effect::IdleReload).unwrap(),
            json!("idleReload")
        );
        assert_eq!(
            serde_json::to_value(Effect::PlaybackIdleReload).unwrap(),
            json!("playbackIdleReload")
        );
        assert_eq!(
            serde_json::to_value(Effect::Restart).unwrap(),
            json!("restart")
        );
    }

    #[test]
    fn openvino_fixed_rec_profile_schema_is_nullable_and_idle_reload() {
        let field = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .find(|field| field.path == "ocr.openvino.rec_fixed_width")
            .expect("explicit recognition profile schema");
        assert_eq!(field.kind, FieldKind::Int { min: 768, max: 768 });
        assert!(field.nullable);
        assert_eq!(field.effect, Effect::IdleReload);
    }

    #[test]
    fn lyrics_lead_schema_exposes_the_valid_range_and_live_effect() {
        let field = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .find(|field| field.path == "timing.playback.lyrics_lead_seconds")
            .expect("lyrics lead schema field");

        assert_eq!(
            field.kind,
            FieldKind::Float {
                min: 0.0,
                max: miliastra_playback::MAX_LYRICS_LEAD_SECONDS,
            }
        );
        assert_eq!(field.effect, Effect::Live);
    }

    #[test]
    fn changed_leaf_paths_inherit_the_deepest_container_effect() {
        assert_eq!(
            config_effect_for_path("window.focus_point.x"),
            Some(Effect::IdleReload)
        );
        assert_eq!(
            config_effect_for_path("playback.audio_cache.directory"),
            Some(Effect::IdleReload)
        );
        assert_eq!(config_effect_for_path("unknown.path"), None);
    }

    #[test]
    fn only_playback_source_recovery_fields_wait_for_an_active_song() {
        let path = "playback.credential_directory";
        assert_eq!(
            config_effect_for_path(path),
            Some(Effect::PlaybackIdleReload),
            "field={path}"
        );
        for path in [
            "stability.default_count",
            "timing.external.volume_smooth_step_ms",
            "playback.login_helper_executable",
            "playback.audio_cache.enabled",
        ] {
            assert_eq!(
                config_effect_for_path(path),
                Some(Effect::IdleReload),
                "field={path}"
            );
        }
        // task-33：这些字段已改为 Live（消费方每次读 live 快照），不再是
        // PlaybackIdleReload/IdleReload；保留本测试只守「哪些字段必须等播放空闲」。
        for path in [
            "playback.login_timeout_ms",
            "timing.playback.uri_stable_samples",
            "timing.playback.transport_stable_samples",
            "timing.playback.stale_timeout_ms",
        ] {
            assert_eq!(
                config_effect_for_path(path),
                Some(Effect::Live),
                "field={path}"
            );
        }
    }

    #[test]
    fn database_fields_never_require_manual_restart() {
        for field in config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
        {
            match field.source {
                ConfigSource::Db => assert_ne!(
                    field.effect,
                    Effect::Restart,
                    "数据库字段 {} 应立即生效或在闲置时自动重载",
                    field.path
                ),
                ConfigSource::Bootstrap => assert_eq!(
                    field.effect,
                    Effect::Restart,
                    "bootstrap 字段 {} 应保留人工重启语义",
                    field.path
                ),
            }
        }
    }

    /// schema 段集合必须与 AppConfig 顶层段集合一致（含 http/logging）：
    /// to_db_value 剔除 http/logging 与 state.playback_state_path，schema 对
    /// bootstrap 段仍声明用于展示；state 段只含 hall/executed 两字段，
    /// playback_state_path 不在 schema 中（由统一数据库路径注入）。
    #[test]
    fn every_section_belongs_to_a_domain_and_domains_are_ordered() {
        // 每个段都必须能归入一个功能域，否则面板上它会落到"系统"里，
        // 用户找不到——这是分组呈现的前提。
        let sections = config_sections();
        assert!(!sections.is_empty());
        for section in &sections {
            assert!(
                Domain::ALL.contains(&section.domain),
                "段 {} 的域 {section:?} 不在 Domain::ALL 中",
                section.name
            );
        }
        // 五个域都应有段，避免出现空分组。
        for domain in Domain::ALL {
            assert!(
                sections.iter().any(|s| s.domain == domain),
                "域 {} 没有任何配置段",
                domain.label()
            );
        }
    }

    #[test]
    fn large_sections_are_split_into_named_subgroups() {
        // 字段多的大段必须分子节，否则 30+ 字段平铺难以浏览。
        let sections = config_sections();
        for name in ["timing", "ocr"] {
            let section = sections
                .iter()
                .find(|s| s.name == name)
                .expect("大段应存在");
            let grouped = section.fields.iter().filter(|f| f.group.is_some()).count();
            assert!(
                grouped * 2 > section.fields.len(),
                "{name} 的多数字段应归入小节，实际 {grouped}/{}",
                section.fields.len()
            );
        }
    }

    #[test]
    fn subgroup_names_are_non_empty_and_user_facing() {
        // 小节名会直接显示给用户：不能为空，也不能是代码里的路径前缀。
        for section in config_sections() {
            for field in &section.fields {
                if let Some(group) = field.group {
                    assert!(!group.trim().is_empty(), "{} 的小节名为空", field.path);
                    assert!(
                        !group.contains('.') && !group.contains('_'),
                        "{} 的小节名像代码标识符：{group}",
                        field.path
                    );
                }
            }
        }
    }

    #[test]
    fn schema_covers_all_app_config_sections() {
        let declared: BTreeSet<String> = config_sections()
            .into_iter()
            .map(|section| section.name)
            .collect();
        let db_value = AppConfig::default().to_db_value();
        let db_keys: BTreeSet<String> = db_value
            .as_object()
            .expect("to_db_value 必须是对象")
            .keys()
            .cloned()
            .collect();
        let mut expected = db_keys.clone();
        expected.insert("http".to_string());
        expected.insert("logging".to_string());
        assert_eq!(
            declared, expected,
            "schema 段集合必须等于 AppConfig 顶层段集合（含 http/logging）"
        );
    }

    /// section_schema 查找与 config_sections 一致，未知段返回 None。
    #[test]
    fn section_schema_lookup_returns_matching_section() {
        for section in config_sections() {
            let looked_up = section_schema(&section.name)
                .unwrap_or_else(|| panic!("查找已声明段 {}", section.name));
            assert_eq!(looked_up.name, section.name);
            assert_eq!(looked_up.label, section.label);
            assert_eq!(looked_up.order, section.order);
            assert_eq!(looked_up.fields.len(), section.fields.len());
        }
        assert!(section_schema("不存在").is_none(), "未知段必须返回 None");
    }

    /// 字段点路径全 schema 内唯一，防止重复声明。
    #[test]
    fn field_paths_are_unique_within_schema() {
        let mut seen = BTreeSet::new();
        for section in config_sections() {
            for field in section.fields {
                assert!(
                    seen.insert(field.path.clone()),
                    "重复字段路径: {}",
                    field.path
                );
            }
        }
    }

    /// schema 中 kind==Secret 的字段路径集合必须与 SECRET_PATHS 完全一致，
    /// 防止脱敏集合与表单声明脱节（新增 secret 字段必须同时登记两处）。
    #[test]
    fn secret_paths_are_consistent_with_schema() {
        let schema_secrets: BTreeSet<String> = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .filter(|field| matches!(field.kind, FieldKind::Secret))
            .map(|field| field.path)
            .collect();
        let declared: BTreeSet<String> = SECRET_PATHS.iter().map(|path| path.to_string()).collect();
        assert_eq!(
            schema_secrets, declared,
            "schema 中 Secret 字段路径必须与 SECRET_PATHS 一致（含 http.access_token）"
        );
    }

    /// 每个字段的 FieldKind 必须与默认配置 JSON 中的值类型一致，防止表单控件
    /// 类型与 struct 反序列化类型脱节（Bool→bool、Int→整数、Float→浮点、
    /// String/Path/Secret/Enum→字符串、StringArray→数组、Object→对象/数组、
    /// Rect→{x,y,width,height}、Point→{x,y}）；null 仅在 nullable/optional_parent
    /// 时允许。
    #[test]
    fn schema_fields_validate_against_default_config_types() {
        let value = default_config_json();
        let nullability = schema_nullability();
        for section in config_sections() {
            for field in &section.fields {
                let field_value = json_path(&value, &field.path, &nullability)
                    .unwrap_or_else(|| panic!("字段 {} 在默认配置 JSON 中不存在", field.path));
                if field_value.is_null() {
                    assert!(
                        field.nullable || field.optional_parent,
                        "字段 {} 在默认配置中为 null，但未声明 nullable/optional_parent",
                        field.path
                    );
                    continue;
                }
                match &field.kind {
                    FieldKind::Bool => {
                        assert!(field_value.is_boolean(), "字段 {} 必须是布尔", field.path);
                    }
                    FieldKind::Int { .. } => {
                        assert!(
                            field_value.is_i64() || field_value.is_u64(),
                            "字段 {} 必须是整数",
                            field.path
                        );
                    }
                    FieldKind::Float { .. } => {
                        assert!(
                            field_value.is_f64() || field_value.is_i64() || field_value.is_u64(),
                            "字段 {} 必须是浮点数",
                            field.path
                        );
                    }
                    FieldKind::String
                    | FieldKind::Text
                    | FieldKind::Path
                    | FieldKind::Secret
                    | FieldKind::ModelName
                    | FieldKind::Enum(_) => {
                        assert!(field_value.is_string(), "字段 {} 必须是字符串", field.path);
                    }
                    FieldKind::StringArray => {
                        assert!(field_value.is_array(), "字段 {} 必须是数组", field.path);
                    }
                    // custom_workflows.workflows 默认值是数组，故对象或数组都接受。
                    FieldKind::Object => {
                        assert!(
                            field_value.is_object() || field_value.is_array(),
                            "字段 {} 必须是对象或数组（Object）",
                            field.path
                        );
                    }
                    FieldKind::Rect => {
                        let keys = field_value
                            .as_object()
                            .expect("Rect 必须是对象")
                            .keys()
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        let expected = ["x", "y", "width", "height"]
                            .into_iter()
                            .map(str::to_string)
                            .collect::<BTreeSet<_>>();
                        assert_eq!(
                            keys, expected,
                            "字段 {} 的 Rect 键集合必须为 {{x,y,width,height}}",
                            field.path
                        );
                    }
                    FieldKind::Point => {
                        let keys = field_value
                            .as_object()
                            .expect("Point 必须是对象")
                            .keys()
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        let expected = ["x", "y"]
                            .into_iter()
                            .map(str::to_string)
                            .collect::<BTreeSet<_>>();
                        assert_eq!(
                            keys, expected,
                            "字段 {} 的 Point 键集合必须为 {{x,y}}",
                            field.path
                        );
                    }
                }
            }
        }
    }

    /// 启用音频缓存后的默认子字段必须与运行时默认对齐：
    /// Web「启用」开关预填这些值，若与运行时不一致会出现"默认值却不生效"。
    #[test]
    fn audio_cache_defaults_match_runtime_values() {
        let value = default_config_json_with_audio_cache();
        let audio_cache = &value["playback"]["audio_cache"];
        assert_eq!(audio_cache["enabled"], json!(true));
        assert_eq!(audio_cache["directory"], json!("deps/cache/audio"));
        // 与 crates/miliastra-playback AudioCacheConfig::default 对齐（20 GiB / 2 / 15s / 10s）。
        assert_eq!(audio_cache["max_bytes_mb"], json!(20 * 1024));
        assert_eq!(audio_cache["max_concurrent_downloads"], json!(2));
        assert_eq!(audio_cache["request_timeout_ms"], json!(15_000));
        assert_eq!(audio_cache["seek_wait_timeout_ms"], json!(10_000));
        // 内置默认仍为 null（不启用）。
        assert_eq!(
            default_config_json()["playback"]["audio_cache"],
            Value::Null
        );
    }

    /// schema 声明的字段路径集合必须与默认配置 JSON 的全部叶路径双向一致：    /// schema 有而 JSON 没有、JSON 有而 schema 没有都算失败。默认 JSON 中
    /// audio_cache 为 null，其 6 个子路径用样例配置（Some）补充枚举，两个集合
    /// 取并集后再与 schema 比较；注入字段（state.playback_state_path）显式豁免。
    #[test]
    fn schema_leaf_paths_cover_all_default_config_leaves() {
        let schema_paths: BTreeSet<String> = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .map(|field| field.path)
            .collect();
        let kinds = schema_kinds();
        let mut json_paths = BTreeSet::new();
        collect_leaf_paths(
            &default_config_json(),
            "",
            &kinds,
            &schema_paths,
            &mut json_paths,
        );
        collect_leaf_paths(
            &sample_config_json_with_audio_cache(),
            "",
            &kinds,
            &schema_paths,
            &mut json_paths,
        );
        let mut with_default_platform = default_config_json();
        with_default_platform["command_access"]["default_song_source"] =
            Value::String("qqmusic".to_string());
        collect_leaf_paths(
            &with_default_platform,
            "",
            &kinds,
            &schema_paths,
            &mut json_paths,
        );
        for injected in INJECTED_JSON_PATHS {
            json_paths.remove(*injected);
        }
        assert_eq!(
            schema_paths, json_paths,
            "schema 字段路径集合必须与默认配置 JSON 叶路径集合一致（注入字段除外）"
        );
    }

    /// 每个 Enum 字段的每个枚举值都必须能被对应 struct 反序列化（与 serde 属性
    /// 如 rename_all 一致）；把默认配置 JSON 中该字段替换为枚举值后，整个配置
    /// 必须仍能反序列化为 AppConfig（http/logging/state 缺失时有 serde(default)）。
    #[test]
    fn enum_values_deserialize_back_into_default_config() {
        for section in config_sections() {
            for field in &section.fields {
                let FieldKind::Enum(pairs) = &field.kind else {
                    continue;
                };
                for (enum_value, _) in pairs {
                    let mut value = default_config_json();
                    set_path(&mut value, &field.path, Value::String(enum_value.clone()));
                    let restored: AppConfig =
                        serde_json::from_value(value).unwrap_or_else(|error| {
                            panic!(
                                "字段 {} 的枚举值 {} 无法反序列化为 AppConfig: {error}",
                                field.path, enum_value
                            )
                        });
                    // 反序列化后的字段值必须与枚举值一致。
                    let restored_value = serde_json::to_value(&restored).expect("序列化还原配置");
                    let actual = json_path(&restored_value, &field.path, &schema_nullability())
                        .unwrap_or_else(|| panic!("字段 {} 反序列化后不存在", field.path));
                    assert_eq!(
                        actual.as_str(),
                        Some(enum_value.as_str()),
                        "字段 {} 的枚举值 {} 反序列化后不一致",
                        field.path,
                        enum_value
                    );
                }
            }
        }
    }
    /// 视觉档案接管的字段必须都存在于 schema；否则字段会既不在档案面板可编辑，
    /// 又无法被 schema 标记，用户将失去修改入口。
    #[test]
    fn every_visual_managed_path_exists_in_the_schema() {
        let schema_paths: std::collections::BTreeSet<_> = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .map(|field| field.path)
            .collect();
        for path in crate::config::visual::managed_paths() {
            assert!(
                schema_paths.contains(&path),
                "视觉档案路径不在 schema 中: {path}"
            );
        }
    }

    /// 全视觉段由视觉面板编辑，普通表单和搜索不重复展示。
    #[test]
    fn fully_visual_sections_are_owned_by_the_visual_profile() {
        let fully_owned: Vec<_> = config_sections()
            .into_iter()
            .filter(|section| {
                !section.fields.is_empty()
                    && section
                        .fields
                        .iter()
                        .all(|field| crate::config::visual::managed_paths().contains(&field.path))
            })
            .map(|section| section.name)
            .collect();
        assert_eq!(
            fully_owned,
            vec!["visual", "screen", "stability", "templates", "moderation"],
            "整段被档案接管的段列表变化了"
        );
    }

    #[test]
    fn default_numbers_use_shortest_f32_representation() {
        let json = default_config_json();
        let value = &json["ocr"]["min_confidence"];
        assert_eq!(
            value.as_f64().unwrap(),
            0.9,
            "0.8999999761581421 应显示为 0.9"
        );
        let value = &json["startup"]["stable_changed_ratio_threshold"];
        assert_eq!(value.as_f64().unwrap(), 0.01);
        let value = &json["custom_workflows"]["default_threshold"];
        assert_eq!(value.as_f64().unwrap(), 0.9);
        // 整数不受影响。
        assert_eq!(json["http"]["port"].as_i64().is_some(), true);
    }
}
