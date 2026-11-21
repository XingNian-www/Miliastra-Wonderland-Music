//! 基础与通用段 schema：窗口、屏幕区域、稳定性、输出、状态文件、全局热键。
//!
//! 这些段是其它业务流程的坐标/时序底座：窗口决定坐标基准，屏幕区域决定
//! 截图切分，稳定性决定判定门槛，输出与热键决定怎么点、怎么停。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, int};

/// window 段：窗口与坐标基准。
pub(super) fn window_section() -> Vec<ConfigFieldSchema> {
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
            "capture_method",
            "截图采集方式",
            FieldKind::Enum(vec![
                ("gdi".to_string(), "GDI 窗口采集".to_string()),
                (
                    "print_window".to_string(),
                    "PrintWindow 窗口绘制".to_string(),
                ),
                ("desktop".to_string(), "桌面可见区域".to_string()),
                ("wgc".to_string(), "Windows Graphics Capture".to_string()),
            ]),
            "选择 Windows 截图来源；GDI 通常最兼容，PrintWindow 可尝试采集被遮挡窗口，桌面方式读取屏幕可见内容，WGC 使用系统图形捕获 API（Windows 10 1903+，建会话失败时自动回落到 GDI）",
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
pub(super) fn stability_section() -> Vec<ConfigFieldSchema> {
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

/// output 段：输出开关与点击点。
pub(super) fn output_section() -> Vec<ConfigFieldSchema> {
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

pub(super) fn state_section() -> Vec<ConfigFieldSchema> {
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
pub(super) fn screen_section() -> Vec<ConfigFieldSchema> {
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
            "secondary_chat_submit_rect",
            "二级聊天发送按钮区域",
            FieldKind::Rect,
            "二级聊天输入行「发送」圆钮的搜索区；与返回按钮一起判二级聊天",
        ),
        ConfigFieldSchema::db_idle_reload(
            "main_menu_rect",
            "奇域主菜单区域",
            FieldKind::Rect,
            "千星奇域主菜单右下角「返回提瓦特」按钮的搜索区；该按钮只有枢纽节点才有，用来在二级聊天判定的最后一刻否决它",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hall_bar_rect",
            "大厅底栏区域",
            FieldKind::Rect,
            "千星大厅底栏 👥 O 图标组的搜索区；提瓦特底栏没有这一组，用来把提瓦特的世界聊天气泡与千星聊天标记区分开",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_f2_invite_rect",
            "F2 邀请玩家按钮区域",
            FieldKind::Rect,
            "F2 千星大厅页右下「邀请玩家」按钮的搜索区；左侧「大厅列表」锚点复用 hall_bar_rect",
        ),
        ConfigFieldSchema::db_idle_reload(
            "world_wish_rect",
            "主世界祈愿区域",
            FieldKind::Rect,
            "主世界右上角祈愿图标搜索区域，覆盖图标栏以适应横向位置变化；仅持续未知时探测",
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

/// hotkeys 段：全局热键。
pub(super) fn hotkeys_section() -> Vec<ConfigFieldSchema> {
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
