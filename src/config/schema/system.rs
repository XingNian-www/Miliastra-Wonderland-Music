//! 系统段 schema：Web/API 面板、日志、终端 TUI、诊断存档、好友投递、
//! 自定义流程、身份映射、酷狗设备。
//!
//! 这一组是运行环境的配置：进程怎么起、日志写哪里、面板监听哪个地址、
//! 谁有权限、自定义流程能点哪里。`http`/`logging` 由启动引导（config.yaml）
//! 提供，schema 只为面板展示而声明。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, enum_of, float, int};

/// http 段：启动引导配置，仅展示，在 config.yaml 修改后重启生效。
pub(super) fn http_section() -> Vec<ConfigFieldSchema> {
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
            "启动引导配置，在 config.yaml 修改后重启生效；非本机监听时必须设置；首次留空时程序自动生成并写回",
        ),
    ]
}

/// logging 段：启动引导配置，仅展示，在 config.yaml 修改后重启生效。
pub(super) fn logging_section() -> Vec<ConfigFieldSchema> {
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
pub(super) fn tui_section() -> Vec<ConfigFieldSchema> {
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
pub(super) fn diagnostics_section() -> Vec<ConfigFieldSchema> {
    vec![ConfigFieldSchema::db_live(
        "ui_frames",
        "保存界面识别画面",
        FieldKind::Bool,
        "开启后把界面识别时的画面存到程序目录 deps/diagnostics，用于排查「界面持续无法识别」这类问题（存的是画面，文件名不含正文/昵称/UID）。默认关闭：无人看管时会持续占磁盘，排查完请记得关掉。失败帧（deps/logs/ui-failures）不受此开关影响，始终保存",
    )]
}

/// friend_delivery 段：好友投递。
pub(super) fn friend_delivery_section() -> Vec<ConfigFieldSchema> {
    vec![ConfigFieldSchema::db_live(
        "auto_retry_count",
        "自动重试次数",
        int(0, MAX_STABLE_COUNT),
        "好友投递中确认尚未发送项的自动重试次数；0 表示禁用；保存后立即生效",
    )]
}

/// custom_workflows 段：配置驱动的自定义流程命令。
pub(super) fn custom_workflows_section() -> Vec<ConfigFieldSchema> {
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

/// identity 段：身份映射表（备注串 / 显示名 / 真实昵称 / UID → 角色）。
pub(super) fn identity_section() -> Vec<ConfigFieldSchema> {
    vec![ConfigFieldSchema::db_live(
        "mappings",
        "身份映射",
        FieldKind::Object,
        "备注串（游戏内好友备注，也就是屏幕上显示的名字）与 OCR 精确匹配；显示名用于游戏回复、Web 和 API，可留空，留空时依次回退真实昵称与「玩家」；真实昵称由聊天桥自动写入，只做展示与搜索兜底；角色控制命令权限。保存后立即生效。",
    )]
}

pub(super) fn kugou_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "device_guid",
            "设备 GUID",
            FieldKind::String,
            "填写你自己酷狗客户端的设备 GUID（形如 550e8400-e29b-41d4-a716-446655440000）。程序不会自动生成设备信息；三项留空时沿用凭据目录下已有的 kugou-device.json。保存后在下一次酷狗登录时生效。",
        ),
        ConfigFieldSchema::db_idle_reload(
            "device_dev",
            "设备型号",
            FieldKind::String,
            "填写该设备上报的设备标识（设备型号名，任意稳定字符串即可，例如 KuGouMusicApi）。三项必须同时填写。保存后在下一次酷狗登录时生效。",
        ),
        ConfigFieldSchema::db_idle_reload(
            "device_mac",
            "设备 MAC",
            FieldKind::String,
            "填写该设备的 MAC 地址（形如 AA:BB:CC:DD:EE:FF）。三项必须同时填写。保存后在下一次酷狗登录时生效。",
        ),
    ]
}
