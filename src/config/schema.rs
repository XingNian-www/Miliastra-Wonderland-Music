//! 配置 schema 声明（阶段 4a）：Web 配置页按此生成表单。
//!
//! 每个字段声明点路径（与 AppConfig 各段结构一一对应）、中文显示名、
//! 表单控件类型、生效级别与来源。字段路径必须与 struct 字段名一致，
//! 测试 [`schema_fields_exist_in_default_config`] 防止声明与结构脱节。
//! 阶段 5 起由 HTTP 配置接口（interfaces/http/routes/config.rs）消费。

use serde_json::Value;

use super::AppConfig;

/// 配置字段来源：数据库可编辑 / 启动引导（bootstrap，仅展示）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// 数据库可编辑
    Db,
    /// 启动引导（config.yaml）提供，Web 页面仅展示
    Bootstrap,
}

/// 生效级别：保存后立即生效 / 运行态闲置时重载 / 播放器闲置时重载 /
/// 人工重启生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Effect {
    /// 保存后立即生效
    Live,
    /// 保存后等待运行态闲置，再由看门狗重载子进程。
    IdleReload,
    /// 保存后等待运行态闲置，且播放器已闲置或处于可安全恢复的用户暂停状态，
    /// 再由看门狗重载子进程。
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
    "song_review.provider.api_key",
    "turtle_soup.ai.api_key",
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
    /// 字段值可为 null（对应 struct 的 Option 字段；如 ocr.det_model、
    /// playback.audio_cache 本身）。表单允许提交 null 表示未启用。
    pub nullable: bool,
    /// 字段的父级可能为 null（如 audio_cache=null 时其 6 个子字段在结构中
    /// 仍存在但取值路径中间节点为 null）。测试取值时允许该路径返回 null。
    pub optional_parent: bool,
}

/// 配置段 schema：段名（与 AppConfig 字段名一致）、中文分组名、顺序与字段列表。
#[derive(Clone, Debug)]
pub struct ConfigSectionSchema {
    /// 段名（与 AppConfig 字段名一致），如 "ocr"
    pub name: String,
    /// 中文分组名
    pub label: String,
    /// 分组显示顺序
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
        }
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

/// window 段：窗口与坐标基准。
fn window_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "target_process",
            "目标进程名",
            FieldKind::String,
            "目标游戏进程名，按进程文件名匹配，大小写不敏感；多个进程名可用逗号分隔",
        ),
        ConfigFieldSchema::db_idle_reload(
            "content_width",
            "画面宽度",
            int(1, 65535),
            "配置坐标对应的游戏有效画面宽度，必须与 screen.expected_width 一致",
        ),
        ConfigFieldSchema::db_idle_reload(
            "content_height",
            "画面高度",
            int(1, 65535),
            "配置坐标对应的游戏有效画面高度，必须与 screen.expected_height 一致",
        ),
        ConfigFieldSchema::db_idle_reload(
            "auto_activate_window",
            "自动激活窗口",
            FieldKind::Bool,
            "/active-window 接口是否尝试自动切回目标窗口",
        ),
        ConfigFieldSchema::db_idle_reload(
            "focus_point",
            "安全聚焦点",
            FieldKind::Point,
            "全局安全聚焦点，只在业务入口显式激活/聚焦游戏窗口时使用一次",
        ),
    ]
}

/// stability 段：连续确认次数。
fn stability_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "default_count",
            "全局稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "连续确认次数的全局默认值；只有大于 1 才生效，否则使用内置默认值 2",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ui_state_count",
            "UI 状态确认次数",
            int(0, MAX_STABLE_COUNT),
            "公共 UI 模板状态的局部覆盖；大于 1 时优先，否则继承 default_count",
        ),
        ConfigFieldSchema::db_live(
            "secondary_hall_count",
            "二级大厅确认次数",
            int(0, MAX_STABLE_COUNT),
            "二级大厅气泡与发送者关联的局部覆盖；大于 1 时优先，否则继承 default_count；保存后立即生效",
        ),
    ]
}

/// timing 段：全部子段用点路径前缀（chat_scan/command/input/workflow/hall/invite/moderation/playback/decision/external）。
fn timing_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "watchdog_restart_ms",
            "看门狗重启等待",
            int(1, MAX_TIMEOUT_MS),
            "监听子进程异常退出后的重启等待时间，单位毫秒；父看门狗会在下次异常退出时直接读取最新值，无需重载子进程",
        ),
        ConfigFieldSchema::db_live(
            "loop_idle_ms",
            "主循环空转间隔",
            int(1, MAX_TIMEOUT_MS),
            "监听主循环空转间隔；脚本暂停或每轮扫描结束后等待多久再继续，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "chat_scan.fallback_ms",
            "聊天兜底扫描间隔",
            int(1, MAX_TIMEOUT_MS),
            "聊天 OCR 兜底扫描间隔；画面没变化时按这个间隔强制扫描一次，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "chat_scan.change_debounce_ms",
            "变化去抖等待",
            int(1, MAX_TIMEOUT_MS),
            "聊天变化后等待画面稳定再 OCR 的时间，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "chat_scan.change_cooldown_ms",
            "变化触发冷却",
            int(1, MAX_TIMEOUT_MS),
            "两次变化触发 OCR 之间的最小间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "command.ui_timeout_ms",
            "返回一级界面超时",
            int(1, MAX_TIMEOUT_MS),
            "执行命令前等待回到一级界面的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "command.return_retry_ms",
            "ESC 返回重试间隔",
            int(1, MAX_TIMEOUT_MS),
            "返回一级界面时每次 ESC 后等待重新检测的基础时间；连续失败后会递增，超过 5 次固定为 2000ms",
        ),
        ConfigFieldSchema::db_live(
            "command.post_settle_ms",
            "命令后稳定等待",
            int(1, MAX_TIMEOUT_MS),
            "命令执行后等待聊天列表/动画稳定再复扫的时间，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "command.help_batch_ms",
            "帮助消息间隔",
            int(1, MAX_TIMEOUT_MS),
            "@帮助及其他批量消息之间的间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.after_activate_ms",
            "激活后等待",
            int(1, MAX_TIMEOUT_MS),
            "自动激活游戏窗口后等待前台窗口切换完成的时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.focus_ms",
            "聚焦后等待",
            int(1, MAX_TIMEOUT_MS),
            "手动调试工具在显式聚焦游戏窗口后的等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.open_chat_ms",
            "打开聊天等待",
            int(1, MAX_TIMEOUT_MS),
            "按回车打开聊天输入后的等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.click_ms",
            "点击后等待",
            int(1, MAX_TIMEOUT_MS),
            "每次点击聊天输入区域后的等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.text_ms",
            "输入文本等待",
            int(1, MAX_TIMEOUT_MS),
            "输入文本后到发送前的等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "input.send_ms",
            "发送后稳定等待",
            int(1, MAX_TIMEOUT_MS),
            "发送聊天后等待界面稳定的时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "workflow.default_timeout_ms",
            "模板等待超时",
            int(1, MAX_TIMEOUT_MS),
            "wait_template/click_template 默认等待模板出现的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "workflow.default_poll_ms",
            "模板轮询间隔",
            int(1, MAX_TIMEOUT_MS),
            "等待模板出现时的轮询间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "workflow.default_step_wait_ms",
            "步骤间等待",
            int(1, MAX_TIMEOUT_MS),
            "每个步骤执行后的默认等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall.page_settle_ms",
            "大厅页面稳定等待",
            int(1, MAX_TIMEOUT_MS),
            "进入/退出大厅页面后等待页面稳定的时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall.ocr_sample_interval_ms",
            "大厅 OCR 采样间隔",
            int(1, MAX_TIMEOUT_MS),
            "大厅信息多次 OCR 采样之间的间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "invite.open_chat_ms",
            "邀请打开面板等待",
            int(1, MAX_TIMEOUT_MS),
            "邀请流程打开好友/聊天面板前后的固定等待，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "invite.step_ms",
            "邀请步骤等待",
            int(1, MAX_TIMEOUT_MS),
            "点击好友后发送私聊反馈、以及输入大厅密码前的短暂等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_live(
            "invite.confirm_timeout_ms",
            "邀请确认超时",
            int(1, MAX_TIMEOUT_MS),
            "非公共大厅邀请时等待邀请确认/拒绝命令的最长时间，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "invite.confirm_poll_ms",
            "邀请确认扫描间隔",
            int(1, MAX_TIMEOUT_MS),
            "邀请确认扫描间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "moderation.vote_timeout_ms",
            "投票等待超时",
            int(1, MAX_TIMEOUT_MS),
            "拉黑/屏蔽 UID 请求的好友私聊投票等待时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "moderation.vote_poll_ms",
            "投票扫描间隔",
            int(1, MAX_TIMEOUT_MS),
            "投票 OCR 扫描间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "moderation.search_result_timeout_ms",
            "搜索结果超时",
            int(1, MAX_TIMEOUT_MS),
            "点击 UID 搜索按钮后等待搜索结果出现的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "moderation.confirm_wait_ms",
            "确认动作等待",
            int(1, MAX_TIMEOUT_MS),
            "点击确认按钮后等待动作完成的时间，单位毫秒",
        ),
        ConfigFieldSchema::db_live(
            "playback.status_poll_ms",
            "播放状态查询间隔",
            int(1, MAX_TIMEOUT_MS),
            "点歌后查询播放状态的间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "playback.monitor_tick_ms",
            "播放监控循环间隔",
            int(1, MAX_TIMEOUT_MS),
            "播放监控线程循环间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "playback.monitor_status_ms",
            "播放状态校准间隔",
            int(1, MAX_TIMEOUT_MS),
            "播放结束监控向原生播放运行时校准状态的间隔，单位毫秒；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "playback.uri_stable_samples",
            "URI 稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "播放 URI 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "playback.transport_stable_samples",
            "播放状态确认次数",
            int(0, MAX_STABLE_COUNT),
            "播放状态连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "playback.stale_timeout_ms",
            "陈旧观测保留超时",
            int(1, MAX_TIMEOUT_MS),
            "播放器异常时保留上一条稳定观测的最长时间，必须为正整数，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "decision.timeout_ms",
            "确认等待超时",
            int(1, MAX_TIMEOUT_MS),
            "匹配失败/AI 自动匹配后等待用户确认的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "decision.poll_ms",
            "确认扫描间隔",
            int(1, MAX_TIMEOUT_MS),
            "匹配失败/AI 自动匹配期间扫描确认命令的间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "external.volume_smooth_step_ms",
            "音量渐变步进间隔",
            int(1, MAX_TIMEOUT_MS),
            "内置播放器音量渐变（防爆音）：每个平滑步进之间的等待时间，单位毫秒；异步执行不阻塞命令",
        ),
        ConfigFieldSchema::db_idle_reload(
            "external.ai_request_timeout_ms",
            "AI 请求超时",
            int(1, MAX_TIMEOUT_MS),
            "AI HTTP 请求超时，单位毫秒",
        ),
    ]
}

/// output 段：输出开关与点击点。
fn output_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "send_enabled",
            "发送回复开关",
            FieldKind::Bool,
            "是否真的向游戏内发送回复；false 时只写日志；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "focus_point",
            "安全点击点",
            FieldKind::Point,
            "保留给手动调试工具使用的聊天面板安全点击点；自动流程返回一级只使用 ESC",
        ),
        ConfigFieldSchema::db_idle_reload(
            "chat_click_2",
            "聊天输入框点击点",
            FieldKind::Point,
            "打开聊天输入后的输入框位置",
        ),
    ]
}

/// http 段：启动引导配置，仅展示，在 config.yaml 修改后重启生效。
fn http_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::bootstrap_restart(
            "enabled",
            "启用 Web 面板",
            FieldKind::Bool,
            "启动引导配置，在 config.yaml 修改后重启生效；是否启用 Web/API 面板",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "host",
            "监听地址",
            FieldKind::String,
            "启动引导配置，在 config.yaml 修改后重启生效；Web/API 面板监听地址",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "port",
            "监听端口",
            int(1, 65535),
            "启动引导配置，在 config.yaml 修改后重启生效；Web/API 面板端口",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "access_token",
            "访问令牌",
            FieldKind::Secret,
            "启动引导配置，在 config.yaml 修改后重启生效；非本机监听时必须设置；留空时由看门狗生成本次运行期临时令牌",
        ),
    ]
}

/// logging 段：启动引导配置，仅展示，在 config.yaml 修改后重启生效。
fn logging_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::bootstrap_restart(
            "dir",
            "日志目录",
            FieldKind::Path,
            "启动引导配置，在 config.yaml 修改后重启生效；日志目录（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "level",
            "日志级别",
            enum_of(&[
                ("error", "错误"),
                ("warn", "警告"),
                ("info", "信息"),
                ("debug", "调试"),
                ("trace", "跟踪"),
            ]),
            "启动引导配置，在 config.yaml 修改后重启生效；日志级别",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "rotate_daily",
            "按日轮转",
            FieldKind::Bool,
            "启动引导配置，在 config.yaml 修改后重启生效；是否按自然日分文件，启用后主日志和性能日志都会在跨日时自动轮转",
        ),
        ConfigFieldSchema::bootstrap_restart(
            "retain_days",
            "日志保留天数",
            int(1, 3650),
            "启动引导配置，在 config.yaml 修改后重启生效；最多保留多少个自然日的按日日志",
        ),
    ]
}

/// tui 段：终端 TUI 面板。
fn tui_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用 TUI",
            FieldKind::Bool,
            "是否启用终端 TUI 面板；启用后实时显示事件日志、OCR 内容和队列状态",
        ),
        ConfigFieldSchema::db_idle_reload(
            "refresh_ms",
            "刷新间隔",
            int(1, 60_000),
            "TUI 刷新间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "log_lines",
            "日志行数",
            int(1, 10_000),
            "TUI 保留最近多少行日志",
        ),
    ]
}

/// state 段：状态文件路径（playback_state_path 由统一数据库路径注入，不进 schema）。
fn state_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "hall_state_path",
            "大厅状态路径",
            FieldKind::Path,
            "大厅倒计时状态持久化路径（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_live(
            "executed_commands_log_path",
            "命令记录路径",
            FieldKind::Path,
            "最终执行命令记录路径（相对 EXE 根目录）；保存后立即生效",
        ),
    ]
}

/// screen 段：游戏画面缩放尺寸与各 UI 区域矩形。
fn screen_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "expected_width",
            "预期宽度",
            int(1, 65535),
            "截图会缩放到这个宽度后再做模板匹配和 OCR；必须与 window.content_width 一致",
        ),
        ConfigFieldSchema::db_idle_reload(
            "expected_height",
            "预期高度",
            int(1, 65535),
            "截图会缩放到这个高度后再做模板匹配和 OCR；必须与 window.content_height 一致",
        ),
        ConfigFieldSchema::db_idle_reload(
            "warn_on_size_mismatch",
            "尺寸不符警告",
            FieldKind::Bool,
            "截图尺寸和预期不一致时是否记录 warning",
        ),
        ConfigFieldSchema::db_idle_reload(
            "chat_rect",
            "聊天区域",
            FieldKind::Rect,
            "聊天区域，用于匹配蓝/黄/粉聊天标志和 OCR 聊天文本",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_rect",
            "好友按钮区域",
            FieldKind::Rect,
            "一级聊天界面无聊天内容时好友按钮模板检测区域；有聊天内容时由蓝/黄/粉标识判定",
        ),
        ConfigFieldSchema::db_idle_reload(
            "secondary_back_rect",
            "返回按钮区域",
            FieldKind::Rect,
            "二级聊天界面左上角返回按钮模板检测区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "secondary_hall_rect",
            "二级大厅区域",
            FieldKind::Rect,
            "二级大厅/面板界面模板检测区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall_name_rect",
            "大厅名称区域",
            FieldKind::Rect,
            "F2 大厅页顶部大厅名称 OCR 区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall_member_count_rect",
            "成员人数区域",
            FieldKind::Rect,
            "大厅成员人数 OCR 区域，用于判断是否需要滚动右侧成员列表",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall_time_rect",
            "剩余时间区域",
            FieldKind::Rect,
            "F2 大厅页剩余时间 OCR 区域，只保留识别到的分钟数字",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall_member_list_rect",
            "成员列表区域",
            FieldKind::Rect,
            "F2 大厅页成员列表滚动区域；拖动点位约在画布宽度的 2/3 处",
        ),
    ]
}

/// ocr 段：模型路径、后端与检测/识别参数。
fn ocr_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "det_model",
            "检测模型",
            FieldKind::Path,
            "MNN/PaddleOCR 检测模型路径；OpenVINO-only 配置可设为 null 或省略",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "rec_model",
            "识别模型",
            FieldKind::Path,
            "MNN/PaddleOCR 识别模型路径；OpenVINO-only 配置可设为 null 或省略",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "charset",
            "字符集",
            FieldKind::Path,
            "PaddleOCR 字符集路径",
        ),
        ConfigFieldSchema::db_idle_reload(
            "min_confidence",
            "最低置信度",
            float(0.0, 1.0),
            "OCR 最低置信度，低于该值的结果会被过滤",
        ),
        ConfigFieldSchema::db_idle_reload("threads", "OCR 线程数", int(1, 1024), "OCR 线程数"),
        ConfigFieldSchema::db_idle_reload(
            "request_timeout_ms",
            "请求超时",
            int(1, MAX_TIMEOUT_MS),
            "单次 OCR 请求从入队到返回的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "shutdown_timeout_ms",
            "关闭超时",
            int(1, MAX_TIMEOUT_MS),
            "关闭时等待底层 OCR 推理退出的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "backend_priority",
            "后端优先级",
            FieldKind::StringArray,
            "OCR 后端优先级；可用值: cuda, vulkan, opencl, openvino, cpu。引擎逐个尝试、失败自动回退到下一个",
        ),
        ConfigFieldSchema::db_idle_reload(
            "openvino.det_model",
            "OpenVINO 检测模型",
            FieldKind::Path,
            "检测模型 XML 文件（OpenVINO IR）；选择 openvino 后端时不能为空",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "openvino.det_weights",
            "OpenVINO 检测权重",
            FieldKind::Path,
            "检测模型权重文件，与 det_model 配对；选择 openvino 后端时不能为空",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "openvino.rec_model",
            "OpenVINO 识别模型",
            FieldKind::Path,
            "识别模型 XML 文件（OpenVINO IR）；选择 openvino 后端时不能为空",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "openvino.rec_weights",
            "OpenVINO 识别权重",
            FieldKind::Path,
            "识别模型权重文件，与 rec_model 配对；选择 openvino 后端时不能为空",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "openvino.device",
            "OpenVINO 设备",
            FieldKind::String,
            "OpenVINO 设备名，通常为 CPU（安装时可选用 GPU/NPU）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "openvino.cache_dir",
            "OpenVINO 编译缓存",
            FieldKind::Path,
            "OpenVINO GPU/CPU 编译缓存，首次启动会预热聊天 OCR 的固定形状；设置为空可关闭缓存",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "det_max_side_len",
            "检测最长边",
            int(1, 8192),
            "检测模型最长边限制；保持 960 与 PaddleOCR/BetterGI 常用配置一致",
        ),
        ConfigFieldSchema::db_idle_reload(
            "det_score_threshold",
            "检测分割阈值",
            float(0.0, 1.0),
            "检测分割阈值；越低越容易检出细小文字，也更容易产生噪声框",
        ),
        ConfigFieldSchema::db_idle_reload(
            "det_unclip_ratio",
            "文本框外扩比例",
            float(0.1, 10.0),
            "文本框外扩比例；2.0 更接近 BetterGI，减少裁掉边缘字符",
        ),
        ConfigFieldSchema::db_idle_reload(
            "det_min_area",
            "最小文本框面积",
            int(1, 100_000),
            "最小文本框面积；小聊天文字用较低值，避免漏掉短命令",
        ),
        ConfigFieldSchema::db_idle_reload(
            "det_box_border",
            "额外裁剪边框",
            int(0, 100),
            "OCR 库额外裁剪边框；BetterGI 主要依赖 unclip 外扩，这里关闭额外扩边",
        ),
        ConfigFieldSchema::db_idle_reload(
            "change_mean_threshold",
            "画面变化均值阈值",
            float(0.0, 255.0),
            "聊天区缩略图平均像素差超过该值时认为画面有变化",
        ),
        ConfigFieldSchema::db_idle_reload(
            "change_pixel_threshold",
            "画面变化像素阈值",
            float(0.0, 1.0),
            "聊天区缩略图变化像素比例超过该值时认为画面有变化",
        ),
        ConfigFieldSchema::db_idle_reload(
            "text_left_gap",
            "标志右侧间距",
            int(0, 1000),
            "聊天标志右侧到文本区域的间距",
        ),
        ConfigFieldSchema::db_idle_reload(
            "block_top_padding",
            "块顶部扩展",
            int(0, 1000),
            "聊天消息块顶部向上扩展像素",
        ),
        ConfigFieldSchema::db_idle_reload(
            "block_bottom_padding",
            "块底部收缩",
            int(0, 1000),
            "聊天消息块底部向上收缩像素，避免吃到下一条标志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_block_height",
            "消息块最大高度",
            int(1, 10_000),
            "单条聊天消息 OCR 块最大高度",
        ),
        ConfigFieldSchema::db_idle_reload(
            "same_line_y_tolerance",
            "同行 Y 容差",
            int(0, 1000),
            "OCR 结果合并为同一行的 Y 轴容差",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_dedupe_x",
            "标志去重 X 容差",
            int(0, 1000),
            "聊天标志去重 X 轴容差",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_dedupe_y",
            "标志去重 Y 容差",
            int(0, 1000),
            "聊天标志去重 Y 轴容差",
        ),
        ConfigFieldSchema::db_idle_reload(
            "next_marker_min_gap",
            "标志最小间隔",
            int(0, 10_000),
            "判定下一条聊天标志的最小 Y 轴间隔",
        ),
        ConfigFieldSchema::db_idle_reload(
            "right_padding",
            "文本右侧留白",
            int(0, 1000),
            "聊天文本区域右侧留白",
        ),
        ConfigFieldSchema::db_idle_reload(
            "batch_recognize",
            "批量识别",
            FieldKind::Bool,
            "实验性：将聊天块拼接为单张图片一次性 OCR，减少推理次数",
        ),
    ]
}

/// templates 段：15 个模板图片路径与匹配阈值。
fn templates_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "blue_marker",
            "蓝色聊天标志",
            FieldKind::Path,
            "蓝色聊天标志模板，通常是自己/普通聊天行标志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "yellow_marker",
            "黄色聊天标志",
            FieldKind::Path,
            "黄色聊天标志模板，通常是系统/高亮聊天行标志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "pink_marker",
            "粉色聊天标志",
            FieldKind::Path,
            "粉色聊天标志模板，用于识别好友私聊命令",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend",
            "好友按钮模板",
            FieldKind::Path,
            "一级聊天界面的好友按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "secondary_back",
            "二级返回按钮模板",
            FieldKind::Path,
            "二级聊天界面左上角返回按钮模板，用于稳定判断二级界面",
        ),
        ConfigFieldSchema::db_idle_reload(
            "secondary_hall",
            "二级大厅模板",
            FieldKind::Path,
            "二级大厅/面板界面模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "invite_view_star",
            "查看千星按钮模板",
            FieldKind::Path,
            "邀请流程里的“查看千星”按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "invite_goto_hall",
            "前往大厅按钮模板",
            FieldKind::Path,
            "邀请流程里的“前往其大厅”按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "invite_enter_hall",
            "进入大厅按钮模板",
            FieldKind::Path,
            "邀请流程里的“进入大厅”按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_panel",
            "好友界面模板",
            FieldKind::Path,
            "好友界面模板，用于 UID 拉黑/屏蔽流程",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_search_panel",
            "好友搜索模板",
            FieldKind::Path,
            "好友搜索界面模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_more_settings",
            "更多设置模板",
            FieldKind::Path,
            "好友搜索结果里的更多设置按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_block_chat",
            "屏蔽聊天模板",
            FieldKind::Path,
            "更多设置里的屏蔽聊天按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_blacklist",
            "拉黑按钮模板",
            FieldKind::Path,
            "更多设置里的拉黑按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_confirm",
            "确认按钮模板",
            FieldKind::Path,
            "拉黑/屏蔽弹窗里的确认按钮模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_threshold",
            "匹配阈值",
            float(0.0, 1.0),
            "UI/聊天标志模板匹配阈值，越高越严格",
        ),
    ]
}

/// playback 段：播放器凭据、程序路径与启动时加载的数值配置。
fn playback_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_playback_idle_reload(
            "credential_directory",
            "凭据目录",
            FieldKind::Path,
            "账号凭据目录，敏感；三个平台凭据的版本化存储目录（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "login_helper_executable",
            "登录助手程序",
            FieldKind::Path,
            "受控登录使用的短生命周期助手",
        ),
        ConfigFieldSchema::db_playback_idle_reload(
            "kugou_api_executable",
            "酷狗 API sidecar",
            FieldKind::Path,
            "内置酷狗概念版 API sidecar；仅当程序目录下存在该文件时才自动启动",
        ),
        ConfigFieldSchema::db_idle_reload(
            "login_timeout_ms",
            "登录超时",
            int(1, MAX_TIMEOUT_MS),
            "单次交互登录的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_playback_idle_reload(
            "kugou_api_base_url",
            "酷狗 API 地址",
            FieldKind::String,
            "酷狗 KuGouMusicApi 服务地址；文档站不是 API 服务",
        ),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache",
            "音频缓存",
            FieldKind::Object,
            "音频数据缓存整体配置；为 null 时不启用（默认）。启用后编辑下方子字段",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.enabled",
            "启用音频缓存",
            FieldKind::Bool,
            "音频数据缓存（本地代理 + 磁盘缓存）；播放时音源先落本地缓存，重复播放与断网时直接从磁盘服务",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.directory",
            "缓存目录",
            FieldKind::Path,
            "音频文件目录；相对路径基于程序 exe 所在目录，留空默认 deps/cache/audio",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.max_bytes_mb",
            "磁盘占用上限",
            int(1, 1_048_576),
            "磁盘占用上限，单位 MiB",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.max_concurrent_downloads",
            "并发下载上限",
            int(1, 1024),
            "同时进行的源站下载任务上限",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.request_timeout_ms",
            "源站请求超时",
            int(1, MAX_TIMEOUT_MS),
            "源站连接/响应超时，单位毫秒",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.seek_wait_timeout_ms",
            "跳转等待超时",
            int(1, MAX_TIMEOUT_MS),
            "请求尚未下载完成的位置时，等待下载推进的最长时间，单位毫秒",
        )
        .optional_parent(),
    ]
}

/// moderation 段：拉黑/屏蔽 UID 流程的投票参数与区域。
fn moderation_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "stable_vote_samples",
            "连续稳定识别次数",
            int(1, MAX_STABLE_COUNT),
            "同一好友同一判决需要连续稳定识别次数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "required_vote_margin",
            "投票差额",
            int(1, MAX_STABLE_COUNT),
            "同意人数 - 不同意人数 达到该值才执行",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_panel_region",
            "好友界面区域",
            FieldKind::Rect,
            "好友界面模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "search_panel_region",
            "搜索按钮区域",
            FieldKind::Rect,
            "搜索按钮模板搜索区域；命中后会点击按钮左侧 500 像素处粘贴 UID，再点击按钮",
        ),
        ConfigFieldSchema::db_idle_reload(
            "search_input_point",
            "搜索输入框点击点",
            FieldKind::Point,
            "UID 搜索输入框点击点",
        ),
        ConfigFieldSchema::db_idle_reload(
            "search_button_point",
            "搜索按钮点击点",
            FieldKind::Point,
            "UID 搜索按钮点击点",
        ),
        ConfigFieldSchema::db_idle_reload(
            "more_settings_region",
            "更多设置区域",
            FieldKind::Rect,
            "更多设置按钮模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "block_chat_region",
            "屏蔽聊天区域",
            FieldKind::Rect,
            "屏蔽聊天按钮模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "blacklist_region",
            "拉黑按钮区域",
            FieldKind::Rect,
            "拉黑按钮模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "confirm_region",
            "确认按钮区域",
            FieldKind::Rect,
            "拉黑/屏蔽弹窗确认按钮模板搜索区域",
        ),
    ]
}

/// queue 段：点歌队列。
fn queue_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "max_size",
            "队列最大长度",
            int(1, 100_000),
            "队列最大长度",
        ),
        ConfigFieldSchema::db_idle_reload(
            "pool_max_size",
            "播放池容量",
            int(0, 1_000_000),
            "播放池最大容量；已确认播放过的点歌会进入播放池，队列播完后从播放池随机播放；0 表示禁用播放池",
        ),
        ConfigFieldSchema::db_live(
            "protect_current_song_until_finished",
            "保护当前歌曲",
            FieldKind::Bool,
            "机器人确认播放的歌曲是否保护；true 时新点歌会排队，未结束只能用 @下一首 提前切换；歌曲播完由自然结束自动出队；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "external_playback_protect_after_seconds",
            "外部播放保护时间",
            int(0, 86_400),
            "非点歌歌曲连续正常播放多少秒后加入当前歌曲保护；0 表示外部播放永不保护，单位秒；保存后立即生效",
        ),
    ]
}

/// song_dedup 段：长时间同歌去重。
fn song_dedup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用去重",
            FieldKind::Bool,
            "长时间同歌去重；播放/入队前检查，确认播放成功后记录；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "window_seconds",
            "统计窗口",
            int(1, 86_400),
            "最近多少秒内统计同一首歌播放次数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "max_count",
            "最多播放次数",
            int(1, MAX_STABLE_COUNT),
            "最近窗口内同一首歌最多允许播放次数；达到该值后拒绝或跳过；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "console_bypass",
            "控制台豁免",
            FieldKind::Bool,
            "控制台来源是否默认豁免长时间同歌去重",
        ),
        ConfigFieldSchema::db_idle_reload(
            "history_path",
            "历史记录路径",
            FieldKind::Path,
            "长时间同歌去重历史持久化路径（相对 EXE 根目录）",
        ),
    ]
}

/// idiom_chain 段：成语接龙。
fn idiom_chain_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用接龙",
            FieldKind::Bool,
            "是否启用项目内置成语词库的接龙功能",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lexicon_path",
            "词库路径",
            FieldKind::Path,
            "成语词库路径；相对路径以主程序 EXE 所在目录为基准",
        ),
        ConfigFieldSchema::db_idle_reload(
            "history_limit",
            "历史上限",
            int(1, 10_000),
            "最多保留多少个最近成语用于会话历史；本局完整已用集合始终禁止重复",
        ),
        ConfigFieldSchema::db_idle_reload(
            "idle_timeout_seconds",
            "空闲超时",
            int(1, 86_400),
            "多久无人接词就自动结束本局；启用接龙时须大于 0，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "allow_consecutive_player",
            "允许连续接词",
            FieldKind::Bool,
            "false 时开局玩家不能连续接词",
        ),
        ConfigFieldSchema::db_idle_reload(
            "allow_anyone_stop",
            "允许任意结束",
            FieldKind::Bool,
            "false 时仅开局玩家可使用 #结束",
        ),
    ]
}

/// landlord 段：斗地主。
fn landlord_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用斗地主",
            FieldKind::Bool,
            "三名好友参与的单局斗地主娱乐模块",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lobby_timeout_seconds",
            "组局超时",
            int(1, 86_400),
            "等待三人组局的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "turn_timeout_seconds",
            "出牌时间",
            int(1, 86_400),
            "每名玩家的出牌时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "trustee_after_timeouts",
            "托管触发次数",
            int(1, MAX_STABLE_COUNT),
            "连续超时多少次后进入自动托管；启用斗地主时须大于 0",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hand_cooldown_seconds",
            "手牌查询冷却",
            int(0, 86_400),
            "好友私聊 #手牌 的单人查询冷却，单位秒",
        ),
    ]
}

/// undercover 段：谁是卧底。
fn undercover_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用卧底",
            FieldKind::Bool,
            "谁是卧底娱乐模块；启用前复制 undercover.example.yaml 为 undercover.yaml 并审核词对",
        ),
        ConfigFieldSchema::db_idle_reload(
            "word_bank_path",
            "词库路径",
            FieldKind::Path,
            "词对词库路径（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "used_state_path",
            "已用词记录路径",
            FieldKind::Path,
            "已使用词对持久化路径（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "min_players",
            "最少玩家",
            int(4, 11),
            "开局最少玩家数；启用时需 4..=max_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "double_min_players",
            "双卧底最少玩家",
            int(6, 11),
            "双卧底模式所需最少玩家数；启用时需 6..=max_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_players",
            "最多玩家",
            int(4, 11),
            "开局最多玩家数；4..=11 且 >= min_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lobby_timeout_seconds",
            "大厅等待超时",
            int(1, 86_400),
            "等待玩家进入大厅的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "phase_timeout_seconds",
            "阶段超时",
            int(1, 86_400),
            "每个游戏阶段的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "progress_interval_seconds",
            "进度公布间隔",
            int(1, 86_400),
            "投票阶段公布未投票位置的间隔，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "description_max_width",
            "描述最大宽度",
            int(1, 1000),
            "单条公屏 #内容 的最大显示宽度",
        ),
    ]
}

/// turtle_soup 段：海龟汤。
fn turtle_soup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用海龟汤",
            FieldKind::Bool,
            "海龟汤娱乐模块；启用前必须准备正式题库并配置独立 AI Provider",
        ),
        ConfigFieldSchema::db_idle_reload(
            "question_bank_path",
            "题库路径",
            FieldKind::Path,
            "正式题库路径，格式参考 turtle_soup.example.yaml（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "used_state_path",
            "已用题目记录路径",
            FieldKind::Path,
            "永久已使用题目记录；程序不提供重置接口，只能手动修改此文件",
        ),
        ConfigFieldSchema::db_idle_reload(
            "idle_timeout_seconds",
            "空闲超时",
            int(1, 86_400),
            "进行中多久没有新问题就自动结束，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_session_seconds",
            "单局最长时长",
            int(1, 86_400),
            "单局最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_concurrency",
            "并发裁决上限",
            int(1, 1024),
            "AI 同时裁决的最大请求数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_pending",
            "待裁决上限",
            int(1, 1024),
            "除正在裁决外，最多等待多少条问题",
        ),
        ConfigFieldSchema::db_idle_reload(
            "batch_max_parts",
            "批量答案分段上限",
            int(1, 1024),
            "单个昵称的一份批量长答案最多暂存多少段；每段仍受游戏聊天长度限制",
        ),
        ConfigFieldSchema::db_idle_reload(
            "nickname_stable_count",
            "昵称稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "昵称 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "content_stable_count",
            "正文稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "正文 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "request_timeout_seconds",
            "AI 请求超时",
            int(1, 86_400),
            "单次 AI 请求超时，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_count",
            "重试次数",
            int(0, MAX_STABLE_COUNT),
            "单次裁决请求失败后的重试次数；实际最多请求 retry_count + 1 次",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_delay_ms",
            "重试间隔",
            int(0, MAX_TIMEOUT_MS),
            "AI 请求重试间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "custom_prompt",
            "自定义规则",
            FieldKind::String,
            "附加在固定裁决提示词后的房间规则；不要要求模型输出自由解释或汤底",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.endpoint",
            "AI 接口地址",
            FieldKind::String,
            "OpenAI 官方完整 Chat Completions 地址",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.api_key",
            "AI API Key",
            FieldKind::Secret,
            "填写 OpenAI API Key；不要提交到 Git",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.model",
            "AI 模型",
            FieldKind::String,
            "高能力模型；请求只发送 OpenAI 标准 Chat Completions 字段",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.http_proxy",
            "AI 代理",
            FieldKind::String,
            "可选的独立 HTTP(S) 代理；留空时沿用环境代理设置",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.max_tokens",
            "最大 Token",
            int(1, 1_000_000),
            "单次裁决允许生成的最大 Token；程序按 OpenAI Chat Completions 的 max_tokens 字段发送",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai.extra_body",
            "第三方兼容字段",
            FieldKind::Object,
            "第三方兼容字段；默认空。Web 可载入主流网关模板或手动填写 JSON。只能补充字段，和程序固定字段同名时程序值优先",
        ),
    ]
}

/// ai 段：点歌 AI。
fn ai_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "provider",
            "服务商",
            FieldKind::Enum(vec![
                ("mimo".to_string(), "小米 MiMo".to_string()),
                ("openai".to_string(), "OpenAI".to_string()),
                ("deepseek".to_string(), "DeepSeek".to_string()),
                ("custom".to_string(), "自定义网关".to_string()),
            ]),
            "点歌 AI Provider。二段式配置：先选服务商，再按服务商填写参数；custom 必须填写 endpoint 与 model",
        ),
        ConfigFieldSchema::db_idle_reload(
            "api_key",
            "API Key",
            FieldKind::Secret,
            "API Key。留空表示点歌 AI 未启用；不要提交到 Git，也不要放进 URL 查询参数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "endpoint",
            "接口地址",
            FieldKind::String,
            "完整的 Chat Completions 请求地址，必须以 /chat/completions 结尾；非 custom Provider 留空时使用默认 endpoint",
        ),
        ConfigFieldSchema::db_idle_reload(
            "model",
            "模型名",
            FieldKind::String,
            "模型名；点歌默认使用低延迟模型，custom 必须填写",
        ),
        ConfigFieldSchema::db_idle_reload(
            "http_proxy",
            "代理",
            FieldKind::String,
            "可选的独立 HTTP(S) 代理；只作用于点歌 AI，留空时沿用环境代理设置",
        ),
        ConfigFieldSchema::db_idle_reload(
            "extra_body",
            "第三方兼容字段",
            FieldKind::Object,
            "服务商专属请求参数。Web 对 MiMo/OpenAI/DeepSeek 提供官方字段结构化表单；只有 custom 使用自由 JSON。与程序固定字段同名时程序值优先",
        ),
    ]
}

/// song_review 段：候选歌曲 AI 审核。
fn song_review_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用审核",
            FieldKind::Bool,
            "候选歌曲 AI 审核；只审核最终候选歌曲/URI，控制台来源最高权限免审",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_allowed_level",
            "最高允许等级",
            int(1, 10),
            "审核返回 level 为 1-10，超过该阈值会拒绝点歌",
        ),
        ConfigFieldSchema::db_idle_reload(
            "failure_policy",
            "失败策略",
            enum_of(&[("reject", "拒绝"), ("allow", "放行")]),
            "审核服务多次失败后的策略：reject 拒绝，allow 放行并写警告日志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_count",
            "重试次数",
            int(0, MAX_STABLE_COUNT),
            "审核请求失败后的重试次数；实际最多请求 retry_count + 1 次",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_delay_ms",
            "重试间隔",
            int(0, MAX_TIMEOUT_MS),
            "审核重试间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "reply_reason_max_chars",
            "拒绝原因字符上限",
            int(1, 10_000),
            "游戏内拒绝原因最多显示的字符数；完整原因写日志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "policy_prompt",
            "审核条件",
            FieldKind::String,
            "审核条件；可以按房间氛围修改，但不要要求模型改变 JSON 输出格式",
        ),
        ConfigFieldSchema::db_idle_reload(
            "custom_prompt",
            "追加规则",
            FieldKind::String,
            "追加审核规则；会附加在 policy_prompt 后面，用于临时补充口径",
        ),
        ConfigFieldSchema::db_idle_reload(
            "provider.endpoint",
            "审核接口地址",
            FieldKind::String,
            "OpenAI 官方完整 Responses 地址；程序会固定要求 web_search",
        ),
        ConfigFieldSchema::db_idle_reload(
            "provider.api_key",
            "审核 API Key",
            FieldKind::Secret,
            "Responses API 使用 Authorization: Bearer <key>；不要提交到 Git",
        ),
        ConfigFieldSchema::db_idle_reload(
            "provider.model",
            "审核模型",
            FieldKind::String,
            "高能力审核模型；启用审核时必须同时填写 api_key",
        ),
        ConfigFieldSchema::db_idle_reload(
            "provider.http_proxy",
            "审核代理",
            FieldKind::String,
            "可选的独立 HTTP(S) 代理；只作用于歌曲审核，留空时沿用环境代理设置",
        ),
        ConfigFieldSchema::db_idle_reload(
            "provider.extra_body",
            "第三方兼容字段",
            FieldKind::Object,
            "第三方兼容字段；必须是 JSON object，与官方字段同名时官方值覆盖",
        ),
    ]
}

/// matching 段：歌名/歌手匹配。
fn matching_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "min_song_name_score",
            "歌名最低分数",
            float(0.0, 1.0),
            "歌名最低匹配分数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "short_chinese_song_max_miss",
            "短中文歌名漏字上限",
            int(0, 100),
            "4 字以内中文歌名最多允许漏字数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "long_chinese_song_min_score",
            "长中文歌名最低比例",
            float(0.0, 1.0),
            "长中文歌名最低命中比例；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "max_ocr_noise_chars",
            "OCR 噪声字符上限",
            int(0, 100),
            "完整歌名后最多忽略的 OCR 噪声字符数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "enable_fuzzy_singer",
            "歌手模糊匹配",
            FieldKind::Bool,
            "是否启用中文歌手模糊匹配；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "short_chinese_singer_max_miss",
            "短中文歌手漏字上限",
            int(0, 100),
            "4 字以内中文歌手最多允许漏字数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "long_chinese_singer_min_score",
            "长中文歌手最低比例",
            float(0.0, 1.0),
            "长中文歌手最低命中比例；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "en_max_edit_fraction",
            "英文歌名编辑距离上限",
            float(0.0, 1.0),
            "英文歌名编辑距离占比上限；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "en_singer_max_edit_fraction",
            "英文歌手编辑距离上限",
            float(0.0, 1.0),
            "英文歌手编辑距离占比上限；保存后立即生效",
        ),
    ]
}

/// hotkeys 段：全局热键。
fn hotkeys_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用热键",
            FieldKind::Bool,
            "是否启用全局热键",
        ),
        ConfigFieldSchema::db_idle_reload(
            "pause_key",
            "暂停/恢复热键",
            FieldKind::String,
            "暂停/恢复热键",
        ),
        ConfigFieldSchema::db_idle_reload("exit_key", "退出热键", FieldKind::String, "退出热键"),
    ]
}

/// startup 段：启动流程。
fn startup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用启动流程",
            FieldKind::Bool,
            "程序启动后是否按配置自动排队执行“启动游戏”和“进入千星”",
        ),
        ConfigFieldSchema::db_idle_reload(
            "launch_game",
            "启动游戏",
            FieldKind::Bool,
            "找不到游戏窗口时是否启动游戏",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_game",
            "进入游戏",
            FieldKind::Bool,
            "是否处理“点击进入”等开门按钮",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_wonderland",
            "进入千星",
            FieldKind::Bool,
            "是否按 M 打开地图，进入千星奇域大厅后停止",
        ),
        ConfigFieldSchema::db_idle_reload(
            "exe_path",
            "启动 EXE 路径",
            FieldKind::Path,
            "启动 EXE 路径；可填完整 exe 文件路径，也可填 exe 所在目录；留空时自动从米哈游启动器注册表查找",
        ),
        ConfigFieldSchema::db_idle_reload(
            "game_args",
            "启动参数",
            FieldKind::String,
            "启动游戏时附加的命令行参数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "launch_wait_ms",
            "窗口出现等待",
            int(1, MAX_TIMEOUT_MS),
            "启动游戏后每次等待窗口出现的时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "launch_retries",
            "窗口检查次数",
            int(1, MAX_STABLE_COUNT),
            "启动游戏后最多检查窗口次数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_game_timeout_ms",
            "点击进入超时",
            int(1, MAX_TIMEOUT_MS),
            "OCR 等待并点击“点击进入”的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_wonderland_timeout_ms",
            "千星确认超时",
            int(1, MAX_TIMEOUT_MS),
            "等待千星奇域界面/大厅确认的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_map_star_retries",
            "星型入口匹配次数",
            int(1, MAX_STABLE_COUNT),
            "地图星型入口阶段最多匹配次数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_map_star_retry_ms",
            "星型入口轮询间隔",
            int(1, MAX_TIMEOUT_MS),
            "地图星型入口匹配轮询间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_hall_retries",
            "千星大厅重试次数",
            int(1, MAX_STABLE_COUNT),
            "千星大厅 OCR 阶段最多重试次数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_hall_retry_ms",
            "千星大厅轮询间隔",
            int(1, MAX_TIMEOUT_MS),
            "千星大厅 OCR 轮询间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_transition_timeout_ms",
            "阶段过渡超时",
            int(1, MAX_TIMEOUT_MS),
            "千星入口和确认弹窗各阶段过渡等待的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_confirm_stable_timeout_ms",
            "确认稳定超时",
            int(1, MAX_TIMEOUT_MS),
            "确认按钮消失后等待该区域像素稳定的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "final_primary_timeout_ms",
            "主界面等待超时",
            int(1, MAX_TIMEOUT_MS),
            "启动游戏结束时等待派蒙菜单模板的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "poll_ms",
            "轮询间隔",
            int(1, MAX_TIMEOUT_MS),
            "启动流程 OCR/截图轮询间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "stable_mean_threshold",
            "像素稳定平均差阈值",
            float(0.0, 255.0),
            "像素稳定平均差阈值",
        ),
        ConfigFieldSchema::db_idle_reload(
            "stable_changed_ratio_threshold",
            "像素稳定变化比例",
            float(0.0, 1.0),
            "像素稳定变化比例阈值",
        ),
        ConfigFieldSchema::db_idle_reload(
            "template_threshold",
            "模板匹配阈值",
            float(0.0, 1.0),
            "启动界面模板匹配阈值（例如派蒙菜单）；越高越严格",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_confirm_threshold",
            "千星确认阈值",
            float(0.0, 1.0),
            "千星确认按钮模板匹配阈值；越高越严格",
        ),
        ConfigFieldSchema::db_idle_reload(
            "templates.wonderland_map_star",
            "星型入口模板",
            FieldKind::Path,
            "地图右下角星型入口模板（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "templates.wonderland_confirm",
            "千星确认模板",
            FieldKind::Path,
            "千星弹窗“确认”按钮模板（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "templates.paimon_menu",
            "派蒙菜单模板",
            FieldKind::Path,
            "派蒙菜单模板；启动游戏完成判断使用（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_game_text_region",
            "点击进入 OCR 区域",
            FieldKind::Rect,
            "“点击进入”按钮 OCR 区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_hall_ocr_region",
            "千星大厅 OCR 区域",
            FieldKind::Rect,
            "右侧千星大厅选项 OCR 区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_confirm_region",
            "千星确认区域",
            FieldKind::Rect,
            "千星确认按钮模板区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "main_ui_region",
            "主界面区域",
            FieldKind::Rect,
            "派蒙菜单/主界面模板区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wonderland_map_star_region",
            "星型入口区域",
            FieldKind::Rect,
            "地图右下角星型入口模板区域",
        ),
    ]
}

/// invite 段：邀请流程区域。
fn invite_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "friend_name_stable_count",
            "好友备注确认次数",
            int(0, MAX_STABLE_COUNT),
            "好友备注连续完整匹配次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_list_region",
            "好友列表区域",
            FieldKind::Rect,
            "好友列表 OCR 区域，用于查找发起邀请的用户名",
        ),
        ConfigFieldSchema::db_idle_reload(
            "friend_chat_region",
            "好友聊天区域",
            FieldKind::Rect,
            "点击好友后用于确认备注的二级聊天内容区域，不使用顶部原昵称",
        ),
        ConfigFieldSchema::db_idle_reload(
            "view_star_region",
            "查看千星区域",
            FieldKind::Rect,
            "“查看千星”模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "goto_hall_region",
            "前往大厅区域",
            FieldKind::Rect,
            "“前往其大厅”模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "enter_hall_region",
            "进入大厅区域",
            FieldKind::Rect,
            "“进入大厅”模板搜索区域",
        ),
    ]
}

/// friend_delivery 段：好友投递。
fn friend_delivery_section() -> Vec<ConfigFieldSchema> {
    vec![ConfigFieldSchema::db_live(
        "auto_retry_count",
        "自动重试次数",
        int(0, MAX_STABLE_COUNT),
        "好友投递中确认尚未发送项的自动重试次数；0 表示禁用；保存后立即生效",
    )]
}

/// custom_workflows 段：配置驱动的自定义流程命令。
fn custom_workflows_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用自定义流程",
            FieldKind::Bool,
            "是否启用配置驱动的自定义流程命令；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "default_threshold",
            "模板默认阈值",
            float(0.0, 1.0),
            "模板默认匹配阈值；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "wait_template_absent_stable_default",
            "模板消失后等稳定",
            FieldKind::Bool,
            "wait_template_absent 默认在模板消失后继续等待像素稳定；可在步骤上设置 stable_after_absent: false 关闭；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "max_hold_key_seconds",
            "按住按键上限",
            int(1, 86_400),
            "hold_key 单次按住按键允许的最大秒数；时长从命令参数读取；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "templates",
            "自定义模板映射",
            FieldKind::Object,
            "自定义模板名到图片路径的映射，步骤里通过 template 引用这里的名字（高级 JSON 编辑）；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "workflows",
            "自定义流程定义",
            FieldKind::Object,
            "自定义流程定义列表；支持的步骤与变量见配置模板注释（高级 JSON 编辑）；保存后立即生效",
        ),
    ]
}

/// 全部配置段 schema（27 段：与 AppConfig 顶层段一一对应，含 bootstrap 的 http/logging）。
pub fn config_sections() -> Vec<ConfigSectionSchema> {
    vec![
        ConfigSectionSchema {
            name: "window".to_string(),
            label: "窗口".to_string(),
            order: 1,
            fields: with_section_prefix("window", window_section()),
        },
        ConfigSectionSchema {
            name: "screen".to_string(),
            label: "屏幕区域".to_string(),
            order: 2,
            fields: with_section_prefix("screen", screen_section()),
        },
        ConfigSectionSchema {
            name: "stability".to_string(),
            label: "稳定性".to_string(),
            order: 3,
            fields: with_section_prefix("stability", stability_section()),
        },
        ConfigSectionSchema {
            name: "timing".to_string(),
            label: "时序".to_string(),
            order: 4,
            fields: with_section_prefix("timing", timing_section()),
        },
        ConfigSectionSchema {
            name: "ocr".to_string(),
            label: "OCR".to_string(),
            order: 5,
            fields: with_section_prefix("ocr", ocr_section()),
        },
        ConfigSectionSchema {
            name: "templates".to_string(),
            label: "模板图片".to_string(),
            order: 6,
            fields: with_section_prefix("templates", templates_section()),
        },
        ConfigSectionSchema {
            name: "output".to_string(),
            label: "输出".to_string(),
            order: 7,
            fields: with_section_prefix("output", output_section()),
        },
        ConfigSectionSchema {
            name: "moderation".to_string(),
            label: "管理（拉黑/屏蔽）".to_string(),
            order: 8,
            fields: with_section_prefix("moderation", moderation_section()),
        },
        ConfigSectionSchema {
            name: "playback".to_string(),
            label: "播放器".to_string(),
            order: 9,
            fields: with_section_prefix("playback", playback_section()),
        },
        ConfigSectionSchema {
            name: "http".to_string(),
            label: "Web/API 面板".to_string(),
            order: 10,
            fields: with_section_prefix("http", http_section()),
        },
        ConfigSectionSchema {
            name: "logging".to_string(),
            label: "日志".to_string(),
            order: 11,
            fields: with_section_prefix("logging", logging_section()),
        },
        ConfigSectionSchema {
            name: "tui".to_string(),
            label: "终端 TUI".to_string(),
            order: 12,
            fields: with_section_prefix("tui", tui_section()),
        },
        ConfigSectionSchema {
            name: "state".to_string(),
            label: "状态文件".to_string(),
            order: 13,
            fields: with_section_prefix("state", state_section()),
        },
        ConfigSectionSchema {
            name: "queue".to_string(),
            label: "点歌队列".to_string(),
            order: 14,
            fields: with_section_prefix("queue", queue_section()),
        },
        ConfigSectionSchema {
            name: "song_dedup".to_string(),
            label: "同歌去重".to_string(),
            order: 15,
            fields: with_section_prefix("song_dedup", song_dedup_section()),
        },
        ConfigSectionSchema {
            name: "idiom_chain".to_string(),
            label: "成语接龙".to_string(),
            order: 16,
            fields: with_section_prefix("idiom_chain", idiom_chain_section()),
        },
        ConfigSectionSchema {
            name: "landlord".to_string(),
            label: "斗地主".to_string(),
            order: 17,
            fields: with_section_prefix("landlord", landlord_section()),
        },
        ConfigSectionSchema {
            name: "undercover".to_string(),
            label: "谁是卧底".to_string(),
            order: 18,
            fields: with_section_prefix("undercover", undercover_section()),
        },
        ConfigSectionSchema {
            name: "turtle_soup".to_string(),
            label: "海龟汤".to_string(),
            order: 19,
            fields: with_section_prefix("turtle_soup", turtle_soup_section()),
        },
        ConfigSectionSchema {
            name: "ai".to_string(),
            label: "点歌 AI".to_string(),
            order: 20,
            fields: with_section_prefix("ai", ai_section()),
        },
        ConfigSectionSchema {
            name: "song_review".to_string(),
            label: "歌曲审核".to_string(),
            order: 21,
            fields: with_section_prefix("song_review", song_review_section()),
        },
        ConfigSectionSchema {
            name: "matching".to_string(),
            label: "歌名匹配".to_string(),
            order: 22,
            fields: with_section_prefix("matching", matching_section()),
        },
        ConfigSectionSchema {
            name: "hotkeys".to_string(),
            label: "全局热键".to_string(),
            order: 23,
            fields: with_section_prefix("hotkeys", hotkeys_section()),
        },
        ConfigSectionSchema {
            name: "startup".to_string(),
            label: "启动流程".to_string(),
            order: 24,
            fields: with_section_prefix("startup", startup_section()),
        },
        ConfigSectionSchema {
            name: "invite".to_string(),
            label: "邀请流程".to_string(),
            order: 25,
            fields: with_section_prefix("invite", invite_section()),
        },
        ConfigSectionSchema {
            name: "friend_delivery".to_string(),
            label: "好友投递".to_string(),
            order: 26,
            fields: with_section_prefix("friend_delivery", friend_delivery_section()),
        },
        ConfigSectionSchema {
            name: "custom_workflows".to_string(),
            label: "自定义流程".to_string(),
            order: 27,
            fields: with_section_prefix("custom_workflows", custom_workflows_section()),
        },
    ]
}

/// 单段 schema。
pub fn section_schema(name: &str) -> Option<ConfigSectionSchema> {
    config_sections()
        .into_iter()
        .find(|section| section.name == name)
}

/// 查询点路径对应的生效级别。Object/Rect/Point 的 JSON 叶子路径继承最深的
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
    serde_json::to_value(AppConfig::default()).expect("AppConfig 序列化不应失败")
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
            current = current.as_object()?.get(*segment)?;
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
        for path in [
            "playback.credential_directory",
            "playback.kugou_api_executable",
            "playback.kugou_api_base_url",
        ] {
            assert_eq!(
                config_effect_for_path(path),
                Some(Effect::PlaybackIdleReload),
                "field={path}"
            );
        }
        for path in [
            "stability.default_count",
            "timing.playback.uri_stable_samples",
            "timing.playback.transport_stable_samples",
            "timing.playback.stale_timeout_ms",
            "timing.external.volume_smooth_step_ms",
            "playback.login_helper_executable",
            "playback.login_timeout_ms",
            "playback.audio_cache.enabled",
        ] {
            assert_eq!(
                config_effect_for_path(path),
                Some(Effect::IdleReload),
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

    /// 每个字段路径都能在默认配置 JSON 中取到值，防止声明与 struct 脱节；
    /// Rect/Point/Object 字段取整段对象；null 值（Option 未启用）由 json_path
    /// 按 nullable/optional_parent 校验后跳过类型断言。
    #[test]
    fn schema_fields_exist_in_default_config() {
        let value = default_config_json();
        let nullability = schema_nullability();
        let mut total_fields = 0;
        for section in config_sections() {
            for field in &section.fields {
                total_fields += 1;
                let field_value = json_path(&value, &field.path, &nullability)
                    .unwrap_or_else(|| panic!("字段 {} 在默认配置 JSON 中不存在", field.path));
                if field_value.is_null() {
                    continue;
                }
                if matches!(field.kind, FieldKind::Rect | FieldKind::Point) {
                    assert!(
                        field_value.is_object(),
                        "字段 {} 的值必须是对象（Rect/Point）",
                        field.path
                    );
                }
                if matches!(field.kind, FieldKind::Object) {
                    assert!(
                        field_value.is_object() || field_value.is_array(),
                        "字段 {} 的值必须是对象或数组（Object）",
                        field.path
                    );
                }
            }
        }
        assert_eq!(
            total_fields, 266,
            "schema 总字段数应与预期一致（声明数 = 实际数）"
        );
    }

    /// default_config_json 必须与 AppConfig::default() 的完整序列化一致。
    #[test]
    fn default_config_json_matches_builtin_defaults() {
        let expected = serde_json::to_value(AppConfig::default()).expect("序列化默认配置");
        assert_eq!(default_config_json(), expected);
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
                    | FieldKind::Path
                    | FieldKind::Secret
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
}
