//! 界面识别与流程段 schema：外部聊天源、时序、OCR、模板图片、管理、
//! 启动流程、邀请流程、表情。
//!
//! 这一组回答的是"程序看到什么、按什么节奏看"：聊天内容从 WS 还是 OCR 来、
//! 各子系统等多久、识别用什么模型与模板、好友面板与确认弹窗在哪、
//! 启动/邀请流程怎么导航、二级聊天表情怎么发。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, MAX_TIMEOUT_MS, enum_of, float, int};

/// chat_source 段：外部聊天源（Miliastra-Wonderland-ChatBridge WS）。
pub(super) fn chat_source_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用外部聊天源",
            FieldKind::Bool,
            "启用后停止 OCR 的聊天内容识别，改由 Miliastra-Wonderland-ChatBridge 的 WS 推送提供消息；界面识别（一级/二级/未聊天）仍由模板匹配完成。关闭则完全走 OCR",
        ),
        ConfigFieldSchema::db_idle_reload(
            "url",
            "WebSocket 地址",
            FieldKind::Text,
            "Miliastra-Wonderland-ChatBridge 的推送地址，默认 ws://127.0.0.1:18889/stream（端口 = http.port + 1）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "token",
            "访问令牌",
            FieldKind::Secret,
            "Miliastra-Wonderland-ChatBridge 的鉴权令牌，通常与 http.access_token 相同；留空则连接会被拒绝",
        ),
        ConfigFieldSchema::db_live(
            "reconnect_ms",
            "重连间隔",
            FieldKind::Int {
                min: 100,
                max: 600_000,
            },
            "WS 断开后等待多久重连，单位毫秒",
        ),
        ConfigFieldSchema::db_live(
            "lease_timeout_ms",
            "失联回落阈值",
            FieldKind::Int {
                min: 1_000,
                max: 600_000,
            },
            "外部源多久没有消息就回落到 OCR，单位毫秒；防止 Miliastra-Wonderland-ChatBridge 挂掉后聊天完全失聪",
        ),
        ConfigFieldSchema::db_idle_reload(
            "auto_start",
            "自动启动 Miliastra-Wonderland-ChatBridge",
            FieldKind::Bool,
            "开启后 BOT 发现 Miliastra-Wonderland-ChatBridge 没在监听时替你拉起它；要求游戏未运行，因为 Miliastra-Wonderland-ChatBridge 会自己拉起游戏并注入",
        ),
        ConfigFieldSchema::db_idle_reload(
            "executable",
            "Miliastra-Wonderland-ChatBridge 路径",
            FieldKind::Text,
            "miliastra-wonderland-chatbridge.exe 的完整路径；留空则依次查找 BOT 目录及其上一级的 deps/bin、Miliastra-Wonderland-ChatBridge 目录，以及解压出的带版本号发布目录",
        ),
        ConfigFieldSchema::db_live(
            "disconnect_action",
            "失联超过回落阈值后",
            enum_of(&[
                ("fallback_ocr", "回落 OCR 识别（默认）"),
                ("restart_game", "自动重启游戏"),
            ]),
            "外部聊天源「已经连上过」之后失联（DLL/游戏链路断了），超过上面的「失联回落阈值」仍未重连时做什么：回落 OCR 只是换回截图识别（默认）；自动重启游戏会关闭当前游戏再由聊天桥重新拉起并注入 DLL。从未连上过的启动失败（桥没起来）不会触发；注意程序无法区分「游戏崩溃」和「你手动退出游戏」，开启后手动退出也会被自动重开，需要手动退出时请先关掉这个选项",
        ),
        ConfigFieldSchema::db_idle_reload(
            "self_uid",
            "本机账号 UID",
            FieldKind::Int {
                min: 0,
                max: 4_294_967_295,
            },
            "本机游戏账号的 UID。Miliastra-Wonderland-ChatBridge 用它区分自己和玩家的消息，填错会把 BOT 自己的发言当成命令再执行一次。仅「自动启动」时由 BOT 传给对方；自行部署时请在你的启动命令里填 --self-uid，此处可留 0",
        ),
    ]
}

/// timing 段：全部子段用点路径前缀（chat_scan/command/input/workflow/hall/invite/moderation/playback/decision/external）。
pub(super) fn timing_section() -> Vec<ConfigFieldSchema> {
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
        // `Live` 声明准确：该值由 `delivery::reply_batch` 在运行期重新读取 live
        // 快照（`composition/application/delivery.rs` 的 `reply_batch` 每次调用都读
        // `live_configs.snapshot().timing.command.help_batch_ms`，根本不用传进来的
        // 参数）。`PlaybackApplicationConfig` 里的 `help_batch_ms` 副本仅供测试替身
        // 使用，不代表运行期取值路径。
        ConfigFieldSchema::db_live(
            "command.help_batch_ms",
            "帮助消息间隔",
            int(1, MAX_TIMEOUT_MS),
            "@帮助及其他批量消息之间的间隔，单位毫秒；保存后立即生效（由 delivery::reply_batch 在运行期重新读取 live 快照）",
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
            "进入大厅页面或滚动成员列表后的初始等待时间，单位毫秒；之后继续检测页面是否可读",
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
        ConfigFieldSchema::db_live(
            "playback.lyrics_lead_seconds",
            "歌词提前秒数",
            float(0.0, miliastra_playback::MAX_LYRICS_LEAD_SECONDS),
            "按当前播放进度向前查询歌词的秒数，支持小数，范围 0 到 60；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "playback.uri_stable_samples",
            "URI 稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "播放 URI 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_live(
            "playback.transport_stable_samples",
            "播放状态确认次数",
            int(0, MAX_STABLE_COUNT),
            "播放状态连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_live(
            "playback.stale_timeout_ms",
            "陈旧观测保留超时",
            int(1, MAX_TIMEOUT_MS),
            "播放器异常时保留上一条稳定观测的最长时间，必须为正整数，单位毫秒",
        ),
        ConfigFieldSchema::db_live(
            "decision.timeout_ms",
            "确认等待超时",
            int(1, MAX_TIMEOUT_MS),
            "匹配失败/AI 自动匹配后等待用户确认的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_live(
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

/// ocr 段：模型路径、后端与检测/识别参数。
pub(super) fn ocr_section() -> Vec<ConfigFieldSchema> {
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
            "OCR 后端优先级；可用值: cuda, vulkan, opencl, openvino, cpu。默认 openvino、cpu：OpenVINO 优先，初始化失败自动回退 MNN CPU",
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
            "openvino.rec_fixed_width",
            "OpenVINO 识别固定宽度",
            int(768, 768),
            "仅自训 48x768、输出 stride 8 模型设为 768：超宽压缩、短行右侧归一化补零；官方模型保留 null，按等比宽度分桶",
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
            "marker_search_width",
            "标志搜索条带宽度",
            int(1, 10_000),
            "基准画布像素；聊天区左侧标志搜索范围",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_dedupe_min_x",
            "标志去重 X 下限",
            int(0, 1000),
            "与标志去重 X 容差取较大值",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_dedupe_min_y",
            "标志去重 Y 下限",
            int(0, 1000),
            "与标志去重 Y 容差取较大值",
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

/// templates 段：模板图片路径与匹配阈值。
pub(super) fn templates_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "panel_downsample_factors",
            "面板用途降采样倍数",
            FieldKind::Object,
            "用途名到 1–4 倍的覆盖；空对象保留现有 1/4 倍默认值，不影响流程点击精度",
        ),
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
            "secondary_chat_submit",
            "二级聊天发送按钮模板",
            FieldKind::Path,
            "二级聊天输入行「发送」圆钮；用来挡掉只有返回按钮的主菜单过渡帧",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_f2_hall_list",
            "F2 大厅列表按钮模板",
            FieldKind::Path,
            "F2 千星大厅页左下「大厅列表」按钮（图标+文字）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_f2_invite_player",
            "F2 邀请玩家按钮模板",
            FieldKind::Path,
            "F2 千星大厅页右下「邀请玩家」按钮（图标+文字）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_exit_menu",
            "千星退出菜单锚点",
            FieldKind::Path,
            "继续游戏/退出登录/退出桌面",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_map_label",
            "千星地图右下角锚点",
            FieldKind::Path,
            "右下角「千星奇域·大厅」文字 + 庙宇图标（不含 UID 与背景），千星地图首选判据",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_profile_page_wishlist",
            "个人资料页「心愿单」按钮模板",
            FieldKind::Path,
            "个人资料页右下角「心愿单」圆角按钮；必须与「衣橱」同时命中才判个人资料页",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_profile_page_wardrobe",
            "个人资料页「衣橱」按钮模板",
            FieldKind::Path,
            "个人资料页右下角「衣橱」圆角按钮；与「心愿单」成对判据，单命中不成立",
        ),
        ConfigFieldSchema::db_idle_reload(
            "teyvat_profile_adventure_rank",
            "提瓦特资料卡「冒险等阶」锚点",
            FieldKind::Path,
            "提瓦特资料卡左页「冒险等阶」纯文字模板；与「世界等级」同时命中才判提瓦特资料卡",
        ),
        ConfigFieldSchema::db_idle_reload(
            "teyvat_profile_world_level",
            "提瓦特资料卡「世界等级」锚点",
            FieldKind::Path,
            "提瓦特资料卡左页「世界等级」纯文字模板；与「冒险等阶」成对判据",
        ),
        ConfigFieldSchema::db_idle_reload(
            "teyvat_profile_add_friend",
            "提瓦特资料卡「添加好友」锚点",
            FieldKind::Path,
            "提瓦特资料卡右下角深色「添加好友」按钮；命中=非好友，未命中=好友",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_profile_domain_level",
            "千星资料卡「奇域等级」锚点",
            FieldKind::Path,
            "千星资料卡上部「奇域等级」纯文字模板；命中即千星资料卡",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_profile_add_friend",
            "千星资料卡「添加好友」锚点",
            FieldKind::Path,
            "千星资料卡下部白色胶囊「添加好友」；命中=非好友",
        ),
        ConfigFieldSchema::db_idle_reload(
            "qianxing_profile_view_profile",
            "千星资料卡「查看资料」锚点",
            FieldKind::Path,
            "千星资料卡下部白色胶囊「查看资料」；无「添加好友」但命中它=好友",
        ),
        ConfigFieldSchema::db_idle_reload(
            "main_menu",
            "奇域主菜单模板",
            FieldKind::Path,
            "千星奇域主菜单右下角「返回提瓦特」按钮模板，命中即判为奇域主菜单而不是二级聊天",
        ),
        ConfigFieldSchema::db_idle_reload(
            "world_wish",
            "主世界祈愿模板",
            FieldKind::Path,
            "主世界右上角祈愿图标的透明 PNG 模板；仅持续未知时参与状态识别",
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
            "confirm_button",
            "确认按钮模板",
            FieldKind::Path,
            "通用「确认」按钮模板；好友操作与启动流程共用",
        ),
        ConfigFieldSchema::db_idle_reload(
            "confirm_message_random_hall",
            "进入大厅文案模板",
            FieldKind::Path,
            "确认弹窗「随机公共大厅」关键词模板；确认弹窗必须按钮+文案双命中",
        ),
        ConfigFieldSchema::db_idle_reload(
            "confirm_message_return_teyvat",
            "离开大厅文案模板",
            FieldKind::Path,
            "确认弹窗「返回提瓦特」关键词模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "confirm_message_block",
            "屏蔽玩家文案模板",
            FieldKind::Path,
            "确认弹窗「屏蔽吗」关键词模板",
        ),
        ConfigFieldSchema::db_idle_reload(
            "marker_threshold",
            "匹配阈值",
            float(0.0, 1.0),
            "UI/聊天标志模板匹配阈值，越高越严格",
        ),
    ]
}

/// moderation 段：拉黑/屏蔽 UID 流程的投票参数与区域。
pub(super) fn moderation_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "stable_vote_samples",
            "连续稳定识别次数",
            int(1, MAX_STABLE_COUNT),
            "同一好友同一判决需要连续稳定识别次数",
        ),
        ConfigFieldSchema::db_live(
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
            "confirm_button_region",
            "确认按钮区域",
            FieldKind::Rect,
            "通用「确认」按钮模板搜索区域",
        ),
        ConfigFieldSchema::db_idle_reload(
            "confirm_message_region",
            "确认弹窗文案区域",
            FieldKind::Rect,
            "确认弹窗提示文案行搜索区域；确认按钮与关键词文案必须同时命中",
        ),
    ]
}

/// startup 段：启动流程。
pub(super) fn startup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用启动流程",
            FieldKind::Bool,
            "启用游戏启动及根据界面状态自动进入游戏、进入千星",
        ),
        ConfigFieldSchema::db_idle_reload(
            "launch_game",
            "启动游戏",
            FieldKind::Bool,
            "找不到游戏窗口时是否启动游戏",
        ),
        // 数据库可编辑，但**要等运行态闲置重载才真正生效**，不是 Live：
        // 调度侧（`recovery_guard::automatic_startup_task`）读 live 快照，执行侧
        // （`ui/routines/startup.rs` 的 `execute_enter_game`）读的却是启动期冻结在
        // `StartupUiConfig` 里的 `enter_game`。只改一半会变成「面板说已生效、实际
        // 按旧值执行」，违反「配置 effect 必须准确」。
        //
        // 与同段的 `launch_game` 同一档：`startup.*` 一旦改动，`tasks.rs` 的
        // `reload_fields_require_startup` 会把重载升级成 `ConfigReloadWithStartup`，
        // 也就是「闲置时重载并重新走启动流程」——效果等同重启，而面板文案如实写
        // 「任务结束后重载」。
        //
        // 说明：审计决策写的是「改为 `Restart`」。这里没有照抄，因为把 `source: Db`
        // 的字段声明成 `Effect::Restart` 会直接违反 `schema/mod.rs` 的
        // `database_fields_never_require_manual_restart`（「数据库字段应立即生效或在
        // 闲置时自动重载」），而那个文件不在本次允许改动范围内；`ui.rs` 里也不存在
        // 数据库字段可用的 `Restart` 构造器（`bootstrap_restart` 会连带把 source
        // 改成 Bootstrap，让面板只读且不写回，等于把可编辑字段变灰）。
        ConfigFieldSchema::db_idle_reload(
            "enter_game",
            "自动进入游戏",
            FieldKind::Bool,
            "持续未知时 OCR 检测到“点击进入”，确认大门界面后自动进入游戏；任务结束后重载子进程时生效",
        ),
        ConfigFieldSchema::db_live(
            "enter_wonderland",
            "自动进入千星",
            FieldKind::Bool,
            "持续未知时匹配到祈愿图标，确认主世界后自动进入千星奇域大厅",
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
            "启动游戏结束时等待主菜单大世界模板的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "restart_game_on_loading",
            "加载过久自动重启游戏",
            FieldKind::Bool,
            "界面长时间停在加载界面时自动重启游戏重试，默认关闭。需要先启用上面的「启用启动流程」；正常游戏里的传送加载一般只有几秒，不会碰到这个超时",
        ),
        ConfigFieldSchema::db_idle_reload(
            "loading_restart_timeout_ms",
            "加载超时",
            FieldKind::Int {
                min: 10_000,
                max: 600_000,
            },
            "停在加载界面超过这个时长就重启游戏，单位毫秒；默认 60000（60 秒）",
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
            "启动界面模板匹配阈值（例如主菜单大世界）；越高越严格",
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
            "templates.paimon_menu",
            "主菜单大世界模板",
            FieldKind::Path,
            "主菜单大世界模板；启动游戏完成判断使用（相对 EXE 根目录）",
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
            "main_ui_region",
            "主界面区域",
            FieldKind::Rect,
            "主菜单大世界/主界面模板区域",
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
pub(super) fn invite_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "friend_name_stable_count",
            "好友备注确认次数",
            int(0, MAX_STABLE_COUNT),
            "好友备注连续完整匹配次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "fuzzy_friend_match",
            "好友候选评分匹配",
            FieldKind::Bool,
            "所有列表页精确匹配失败后，回到顶部搜索候选；默认关闭",
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

/// `tts` 段：fish.audio 语音合成（仅 AI 聊天工具 speak 可触发）。
pub(super) fn emoji_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用表情发送",
            FieldKind::Bool,
            "开启后二级当前聊天支持 {表情包:页签:文字} 标签；关闭不影响文字发送",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai_enabled",
            "允许 AI 回复使用表情",
            FieldKind::Bool,
            "开启后 AI 只能使用下面白名单里的表情；关闭时 AI 回复中的表情标签会被剥离",
        ),
        ConfigFieldSchema::db_idle_reload(
            "ai_allowed",
            "AI 可用表情",
            FieldKind::StringArray,
            "每项格式 页签序号|表情文字，例如 2|交给我吧；页签 1=收藏，2..6=角色；第 7 个页签会把收藏页挤走并隐藏，不支持",
        ),
        ConfigFieldSchema::db_idle_reload(
            "capture_debug",
            "保存表情调试截图",
            FieldKind::Bool,
            "默认关闭；开启后表情流程的完整截图落到 deps/emoji-debug（可能含聊天内容）",
        ),
    ]
}
