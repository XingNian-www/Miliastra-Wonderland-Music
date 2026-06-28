use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub window: WindowConfig,
    pub screen: ScreenConfig,
    pub ocr: OcrConfig,
    pub templates: TemplateConfig,
    pub output: OutputConfig,
    pub feeluown: FeelUOwnConfig,
    pub http: HttpConfig,
    pub logging: LoggingConfig,
    pub state: StateConfig,
    pub queue: QueueConfig,
    pub ai: AiConfig,
    pub matching: MatchConfig,
    pub hotkeys: HotkeyConfig,
    pub invite: InviteConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            screen: ScreenConfig::default(),
            ocr: OcrConfig::default(),
            templates: TemplateConfig::default(),
            output: OutputConfig::default(),
            feeluown: FeelUOwnConfig::default(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
            state: StateConfig::default(),
            queue: QueueConfig::default(),
            ai: AiConfig::default(),
            matching: MatchConfig::default(),
            hotkeys: HotkeyConfig::default(),
            invite: InviteConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
    pub active_check_timeout_ms: u64,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            target_process: "yuanshen.exe".to_string(),
            content_width: 1920,
            content_height: 1080,
            auto_activate_window: false,
            active_check_timeout_ms: 2000,
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            return serde_yaml::from_str(&text)
                .with_context(|| format!("parse config {}", path.display()));
        }

        let text = default_config_yaml();
        let config: Self =
            serde_yaml::from_str(text).context("validate default config template")?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        fs::write(path, text)
            .with_context(|| format!("write default config {}", path.display()))?;
        Ok(config)
    }
}

fn default_config_yaml() -> &'static str {
    r#"# Miliastra Wonderland Music 配置
# 坐标沿用旧脚本习惯：以游戏客户区左上角为原点，按 1920x1080 有效画面写坐标

window:
  # 目标游戏进程名，按进程文件名匹配，大小写不敏感
  target_process: yuanshen.exe
  # 配置坐标对应的游戏有效画面宽度
  content_width: 1920
  # 配置坐标对应的游戏有效画面高度
  content_height: 1080
  # /active-window 或输入保护是否尝试自动切回目标窗口
  auto_activate_window: false
  # 活动窗口检测 PowerShell 超时，单位毫秒
  active_check_timeout_ms: 2000

screen:
  # 截图会缩放到这个宽度后再做模板匹配和 OCR
  expected_width: 1920
  # 截图会缩放到这个高度后再做模板匹配和 OCR
  expected_height: 1080
  # 截图尺寸和预期不一致时是否记录 warning
  warn_on_size_mismatch: true
  # 聊天区域，用于匹配蓝/黄/粉聊天标志和 OCR 聊天文本
  chat_rect:
    x: 39
    y: 879
    width: 416
    height: 143
  # 一级聊天界面左下角回车按钮模板检测区域
  enter_rect:
    x: 0
    y: 1020
    width: 120
    height: 60
  # 二级大厅/面板界面模板检测区域
  secondary_hall_rect:
    x: 0
    y: 0
    width: 260
    height: 400
  # F2 大厅页顶部大厅名称 OCR 区域
  hall_name_rect:
    x: 75
    y: 425
    width: 325
    height: 40
  # F2 大厅页剩余时间 OCR 区域，只保留识别到的分钟数字
  hall_time_rect:
    x: 430
    y: 520
    width: 110
    height: 40

ocr:
  # PaddleOCR 检测模型路径
  det_model: models/PP-OCRv6_small_det.mnn
  # PaddleOCR 识别模型路径
  rec_model: models/PP-OCRv6_small_rec.mnn
  # PaddleOCR 字符集路径
  charset: models/ppocr_keys_v6_small.txt
  # OCR 最低置信度，低于该值的结果会被过滤
  min_confidence: 0.9
  # OCR 线程数
  threads: 4
  # OCR 后端优先级，当前发布包只使用 CPU OCR
  backend_priority:
    - cpu
  # 检测模型最长边限制；保持 960 与 PaddleOCR/BetterGI 常用配置一致
  det_max_side_len: 960
  # 检测分割阈值；越低越容易检出细小文字，也更容易产生噪声框
  det_score_threshold: 0.3
  # 文本框外扩比例；2.0 更接近 BetterGI，减少裁掉边缘字符
  det_unclip_ratio: 2.0
  # 最小文本框面积；小聊天文字用较低值，避免漏掉短命令
  det_min_area: 9
  # OCR 库额外裁剪边框；BetterGI 主要依赖 unclip 外扩，这里关闭额外扩边
  det_box_border: 0
  # OCR 兜底扫描间隔，单位毫秒；画面没变化时按这个间隔强制扫描一次
  poll_interval_ms: 2000
  # 一级界面下的画面变化检测间隔，单位毫秒；只做轻量图像 diff，不跑 OCR
  change_poll_interval_ms: 60
  # 检测到画面变化后等待稳定的时间，单位毫秒
  change_debounce_ms: 120
  # 两次变化触发 OCR 之间的最小间隔，单位毫秒
  change_cooldown_ms: 250
  # 命令执行后等待聊天列表/动画稳定的时间，单位毫秒
  post_command_settle_ms: 500
  # 聊天区缩略图平均像素差超过该值时认为画面有变化
  change_mean_threshold: 6.0
  # 聊天区缩略图变化像素比例超过该值时认为画面有变化
  change_pixel_threshold: 0.03
  # 聊天标志右侧到文本区域的间距
  text_left_gap: 8
  # 聊天消息块顶部向上扩展像素
  block_top_padding: 2
  # 聊天消息块底部向上收缩像素，避免吃到下一条标志
  block_bottom_padding: 2
  # 单条聊天消息 OCR 块最大高度
  max_block_height: 120
  # OCR 结果合并为同一行的 Y 轴容差
  same_line_y_tolerance: 10
  # 聊天标志去重 X 轴容差
  marker_dedupe_x: 8
  # 聊天标志去重 Y 轴容差
  marker_dedupe_y: 8
  # 判定下一条聊天标志的最小 Y 轴间隔
  next_marker_min_gap: 12
  # 聊天文本区域右侧留白
  right_padding: 4
  # OCR worker 内存重建阈值，暂作为后续内存保护配置
  memory_rebuild_limit_bytes: 4294967296

templates:
  # 蓝色聊天标志模板，通常是自己/普通聊天行标志
  blue_marker: assets/chat-marker-blue.png
  # 黄色聊天标志模板，通常是系统/高亮聊天行标志
  yellow_marker: assets/chat-marker-yellow.png
  # 粉色聊天标志模板，用于识别好友命令：邀请、麦克风
  pink_marker: assets/chat-marker-pink.png
  # 一级聊天界面的回车按钮模板
  enter: assets/ui-primary-enter.png
  # 二级大厅/面板界面模板
  dating: assets/ui-secondary-dating.png
  # 邀请流程里的“查看千星”按钮模板
  invite_view_star: assets/invite-view-star.png
  # 邀请流程里的“前往其大厅”按钮模板
  invite_goto_hall: assets/invite-goto-hall.png
  # 邀请流程里的“进入大厅”按钮模板
  invite_enter_hall: assets/invite-enter-hall.png
  # 聊天标志模板匹配阈值，越高越严格
  marker_threshold: 0.82

output:
  # 是否真的向游戏内发送回复；false 时只写日志
  send_enabled: true
  # 是否同时弹出 Windows 本地通知
  notify: true
  # 用于聚焦/返回一级聊天界面的点击点
  focus_point:
    x: 1919
    y: 540
  # 打开聊天输入后的第一次点击位置
  chat_click_1:
    x: 120
    y: 225
  # 打开聊天输入后的第二次点击位置，通常是输入框位置
  chat_click_2:
    x: 600
    y: 1013
  # 执行命令前等待回到一级界面的最长时间，单位毫秒
  paste_timeout_ms: 15000
  # 聚焦游戏后的等待时间，单位毫秒
  focus_delay_ms: 300
  # 打开聊天窗口后的等待时间，单位毫秒
  open_chat_delay_ms: 300
  # 两次鼠标点击之间的等待时间，单位毫秒
  click_delay_ms: 150
  # 输入文本后到发送前的等待时间，单位毫秒
  input_delay_ms: 250
  # 发送聊天后等待界面稳定的时间，单位毫秒
  send_delay_ms: 300

feeluown:
  # FeelUOwn TCP RPC 地址
  host: 127.0.0.1
  # FeelUOwn TCP RPC 端口
  port: 23333
  # FeelUOwn 请求超时，单位毫秒
  timeout_ms: 10000

http:
  # 预留 Web/API 面板监听地址
  host: 127.0.0.1
  # 预留 Web/API 面板端口
  port: 18888
  # 是否启用 Web/API 面板；当前仍是占位
  enabled: true

logging:
  # 日志目录
  dir: logs
  # 日志级别：error/warn/info/debug/trace
  level: info

state:
  # 运行时状态持久化路径
  runtime_state_path: data/runtime-state.json
  # 点歌队列持久化路径
  queue_path: data/queue.json

queue:
  # 队列最大长度
  max_size: 5
  # 当前歌曲剩余多少秒以内自动播放队列下一首
  auto_advance_seconds: 5

ai:
  # AI 供应商：mimo/openai/deepseek/custom
  provider: mimo
  # AI API Key，留空表示 AI 功能未启用
  api_key: ""
  # 自定义 OpenAI-compatible chat completions 地址；custom 必填，其他供应商留空使用默认地址
  endpoint: ""
  # AI 模型名；留空使用供应商默认模型，不同供应商可自行填写
  model: ""

matching:
  # 歌名最低匹配分数
  min_song_name_score: 0.5
  # 4 字以内中文歌名最多允许漏字数
  short_chinese_song_max_miss: 1
  # 长中文歌名最低命中比例
  long_chinese_song_min_score: 0.5
  # 完整歌名后最多忽略的 OCR 噪声字符数
  max_ocr_noise_chars: 1
  # 是否启用中文歌手模糊匹配
  enable_fuzzy_singer: true
  # 4 字以内中文歌手最多允许漏字数
  short_chinese_singer_max_miss: 1
  # 长中文歌手最低命中比例
  long_chinese_singer_min_score: 0.8
  # 英文歌名编辑距离占比上限
  en_max_edit_fraction: 0.3
  # 英文歌手编辑距离占比上限
  en_singer_max_edit_fraction: 0.35

hotkeys:
  # 是否启用全局热键
  enabled: true
  # 暂停/恢复热键
  pause_key: F7
  # 退出热键
  exit_key: F12

invite:
  # 好友列表 OCR 区域，用于查找发起邀请的用户名
  friend_list_region:
    x: 80
    y: 280
    width: 170
    height: 600
  # 邀请确认列表 OCR 区域
  confirm_list_region:
    x: 400
    y: 160
    width: 180
    height: 900
  # “查看千星”模板搜索区域
  view_star_region:
    x: 400
    y: 80
    width: 440
    height: 860
  # “前往其大厅”模板搜索区域
  goto_hall_region:
    x: 700
    y: 560
    width: 500
    height: 300
  # “进入大厅”模板搜索区域
  enter_hall_region:
    x: 700
    y: 700
    width: 500
    height: 100
  # 邀请流程每一步点击后的等待时间，单位毫秒
  step_delay_ms: 800
  # 非公共大厅邀请时等待 @邀请确认/@邀请拒绝 的时间，单位毫秒
  confirm_timeout_ms: 30000
"#
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RectConfig {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl RectConfig {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

impl Default for RectConfig {
    fn default() -> Self {
        Self::new(0, 0, 1, 1)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PointConfig {
    pub x: i32,
    pub y: i32,
}

impl PointConfig {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl Default for PointConfig {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            expected_width: 1920,
            expected_height: 1080,
            warn_on_size_mismatch: true,
            chat_rect: RectConfig::new(39, 879, 416, 143),
            enter_rect: RectConfig::new(0, 1020, 120, 60),
            secondary_hall_rect: RectConfig::new(0, 0, 260, 400),
            hall_name_rect: RectConfig::new(75, 425, 325, 40),
            hall_time_rect: RectConfig::new(430, 520, 110, 40),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    pub poll_interval_ms: u64,
    pub change_poll_interval_ms: u64,
    pub change_debounce_ms: u64,
    pub change_cooldown_ms: u64,
    pub post_command_settle_ms: u64,
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
    pub memory_rebuild_limit_bytes: u64,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            det_model: PathBuf::from("models/PP-OCRv6_small_det.mnn"),
            rec_model: PathBuf::from("models/PP-OCRv6_small_rec.mnn"),
            charset: PathBuf::from("models/ppocr_keys_v6_small.txt"),
            min_confidence: 0.9,
            threads: 4,
            backend_priority: vec!["cpu".to_string()],
            det_max_side_len: 960,
            det_score_threshold: 0.3,
            det_unclip_ratio: 2.0,
            det_min_area: 9,
            det_box_border: 0,
            poll_interval_ms: 2000,
            change_poll_interval_ms: 60,
            change_debounce_ms: 120,
            change_cooldown_ms: 250,
            post_command_settle_ms: 500,
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
            memory_rebuild_limit_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateConfig {
    pub blue_marker: PathBuf,
    pub yellow_marker: PathBuf,
    pub pink_marker: PathBuf,
    pub enter: PathBuf,
    pub dating: PathBuf,
    pub invite_view_star: PathBuf,
    pub invite_goto_hall: PathBuf,
    pub invite_enter_hall: PathBuf,
    pub marker_threshold: f32,
}

impl Default for TemplateConfig {
    fn default() -> Self {
        Self {
            blue_marker: PathBuf::from("assets/chat-marker-blue.png"),
            yellow_marker: PathBuf::from("assets/chat-marker-yellow.png"),
            pink_marker: PathBuf::from("assets/chat-marker-pink.png"),
            enter: PathBuf::from("assets/ui-primary-enter.png"),
            dating: PathBuf::from("assets/ui-secondary-dating.png"),
            invite_view_star: PathBuf::from("assets/invite-view-star.png"),
            invite_goto_hall: PathBuf::from("assets/invite-goto-hall.png"),
            invite_enter_hall: PathBuf::from("assets/invite-enter-hall.png"),
            marker_threshold: 0.82,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub send_enabled: bool,
    pub notify: bool,
    pub focus_point: PointConfig,
    pub chat_click_1: PointConfig,
    pub chat_click_2: PointConfig,
    pub paste_timeout_ms: u64,
    pub focus_delay_ms: u64,
    pub open_chat_delay_ms: u64,
    pub click_delay_ms: u64,
    pub input_delay_ms: u64,
    pub send_delay_ms: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            send_enabled: true,
            notify: true,
            focus_point: PointConfig::new(1919, 540),
            chat_click_1: PointConfig::new(120, 225),
            chat_click_2: PointConfig::new(600, 1013),
            paste_timeout_ms: 15000,
            focus_delay_ms: 300,
            open_chat_delay_ms: 300,
            click_delay_ms: 150,
            input_delay_ms: 250,
            send_delay_ms: 300,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FeelUOwnConfig {
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
}

impl Default for FeelUOwnConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 23333,
            timeout_ms: 10000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub enabled: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18888,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("logs"),
            level: "info".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StateConfig {
    pub runtime_state_path: PathBuf,
    pub queue_path: PathBuf,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            runtime_state_path: PathBuf::from("data/runtime-state.json"),
            queue_path: PathBuf::from("data/queue.json"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueConfig {
    pub max_size: usize,
    pub auto_advance_seconds: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_size: 5,
            auto_advance_seconds: 5,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "mimo".to_string(),
            api_key: String::new(),
            endpoint: String::new(),
            model: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InviteConfig {
    pub friend_list_region: RectConfig,
    pub confirm_list_region: RectConfig,
    pub view_star_region: RectConfig,
    pub goto_hall_region: RectConfig,
    pub enter_hall_region: RectConfig,
    pub step_delay_ms: u64,
    pub confirm_timeout_ms: u64,
}

impl Default for InviteConfig {
    fn default() -> Self {
        Self {
            friend_list_region: RectConfig::new(80, 280, 170, 600),
            confirm_list_region: RectConfig::new(400, 160, 180, 900),
            view_star_region: RectConfig::new(400, 80, 440, 860),
            goto_hall_region: RectConfig::new(700, 560, 500, 300),
            enter_hall_region: RectConfig::new(700, 700, 500, 100),
            step_delay_ms: 800,
            confirm_timeout_ms: 30000,
        }
    }
}
