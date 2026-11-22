//! 界面识别与流程段配置：外部聊天源、时序、OCR、模板图片。
//!
//! 这一组回答"程序看到什么、按什么节奏看"：聊天内容从 WS 还是 OCR 来、
//! 各子系统等多久、识别用什么模型与模板。

use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::custom_workflow::WorkflowTimingConfig;
use crate::features::invite::InviteTimingConfig;
use crate::features::moderation::ModerationTimingConfig;
use crate::features::playback::PlaybackTimingConfig;

use super::paths::{validate_nonempty_path, validate_unit_interval};

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
    /// 失联超过 `lease_timeout_ms` 之后做什么。
    ///
    /// 两个选项都要求「此前确实连上过」：从没连上过说明是桥本身没起来，
    /// 重启游戏毫无作用，反而会打断玩家。
    #[serde(default)]
    pub disconnect_action: ChatSourceDisconnectAction,
}

/// 外部聊天源失联后要执行的动作。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatSourceDisconnectAction {
    /// 回落到 OCR 聊天识别（默认，历史行为）。
    #[default]
    FallbackOcr,
    /// 自动重启游戏：关闭当前游戏，再交给自动拉起机制重新启动。
    ///
    /// 适用场景是「DLL 注入后链路断了」：只有让聊天桥重新创建游戏进程才会
    /// 重新注入 DLL，光等重连是恢复不了的。
    RestartGame,
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
            disconnect_action: ChatSourceDisconnectAction::FallbackOcr,
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
        if self.disconnect_action == ChatSourceDisconnectAction::RestartGame && !self.auto_start {
            // 只有聊天桥能创建可注入的游戏进程；没开自动启动就没有任何
            // 「关掉之后还能拉起来」的路径。保存期就挡住，而不是等到失联时
            // 才发现选了个永远不生效的选项（那时只会留一条告警然后放弃）。
            bail!(
                "chat_source.disconnect_action 选择「自动重启游戏」时必须同时开启 chat_source.auto_start，\
                 否则没有任何可用来自动拉起游戏的路径（只有 Miliastra-Wonderland-ChatBridge 能创建可注入的进程）"
            );
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
    pub(super) fn validate(&self) -> Result<()> {
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
        // 默认路径使用发布布局 deps/models/，与发布包 config.yaml 的文件名一致。
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
    pub(super) fn validate(&self) -> Result<()> {
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
    pub(super) fn validate(&self) -> Result<()> {
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

fn default_qianxing_exit_menu_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-exit-menu.png")
}
fn default_qianxing_map_label_template() -> PathBuf {
    PathBuf::from("deps/assets/qianxing-map-label.png")
}
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
        // src/config/test.full.yaml 及 assets/ 目录一致；
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
    pub(super) fn validate(&self) -> Result<()> {
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
            // 千星主菜单锚点：挡住 secondary_back 在 ESC 菜单上的误判，
            // 与其余必填模板一致，空值不能落库（否则运行期才在界面状态机失败）。
            (&self.main_menu, "templates.main_menu"),
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

#[cfg(test)]
mod template_validation_tests {
    use std::path::PathBuf;

    use super::TemplateConfig;

    /// `templates.*` 的每个必填路径字段都必须被 `validate()` 覆盖：
    /// 漏一个，空值就能绕过配置中心落库，直到运行期界面状态机加载模板才失败。
    ///
    /// 这里用「把单个字段置空 → 必须报出该字段名」逐字段验证，不依赖手写清单，
    /// 因为缺陷恰恰是清单漏项（`main_menu` 在结构体里、在 schema 里，却不在校验里）。
    #[test]
    fn every_required_template_path_is_validated() {
        // 字段名 → 置空用的 setter，覆盖 TemplateConfig 的全部必填 PathBuf 字段。
        let fields: Vec<(&str, fn(&mut TemplateConfig))> = vec![
            ("templates.blue_marker", |c| c.blue_marker = PathBuf::new()),
            ("templates.yellow_marker", |c| {
                c.yellow_marker = PathBuf::new()
            }),
            ("templates.pink_marker", |c| c.pink_marker = PathBuf::new()),
            ("templates.secondary_back", |c| {
                c.secondary_back = PathBuf::new()
            }),
            ("templates.secondary_hall", |c| {
                c.secondary_hall = PathBuf::new()
            }),
            ("templates.main_menu", |c| c.main_menu = PathBuf::new()),
            ("templates.world_wish", |c| c.world_wish = PathBuf::new()),
            ("templates.invite_view_star", |c| {
                c.invite_view_star = PathBuf::new()
            }),
            ("templates.invite_goto_hall", |c| {
                c.invite_goto_hall = PathBuf::new()
            }),
            ("templates.invite_enter_hall", |c| {
                c.invite_enter_hall = PathBuf::new()
            }),
            ("templates.friend_panel", |c| {
                c.friend_panel = PathBuf::new()
            }),
            ("templates.friend_search_panel", |c| {
                c.friend_search_panel = PathBuf::new()
            }),
            ("templates.friend_more_settings", |c| {
                c.friend_more_settings = PathBuf::new()
            }),
            ("templates.friend_block_chat", |c| {
                c.friend_block_chat = PathBuf::new()
            }),
            ("templates.friend_blacklist", |c| {
                c.friend_blacklist = PathBuf::new()
            }),
            ("templates.confirm_button", |c| {
                c.confirm_button = PathBuf::new()
            }),
            ("templates.confirm_message_random_hall", |c| {
                c.confirm_message_random_hall = PathBuf::new()
            }),
            ("templates.confirm_message_return_teyvat", |c| {
                c.confirm_message_return_teyvat = PathBuf::new()
            }),
            ("templates.confirm_message_block", |c| {
                c.confirm_message_block = PathBuf::new()
            }),
            ("templates.secondary_chat_submit", |c| {
                c.secondary_chat_submit = PathBuf::new()
            }),
            ("templates.qianxing_f2_hall_list", |c| {
                c.qianxing_f2_hall_list = PathBuf::new()
            }),
            ("templates.qianxing_f2_invite_player", |c| {
                c.qianxing_f2_invite_player = PathBuf::new()
            }),
            ("templates.qianxing_exit_menu", |c| {
                c.qianxing_exit_menu = PathBuf::new()
            }),
            ("templates.qianxing_map_label", |c| {
                c.qianxing_map_label = PathBuf::new()
            }),
            ("templates.qianxing_profile_page_wishlist", |c| {
                c.qianxing_profile_page_wishlist = PathBuf::new()
            }),
            ("templates.qianxing_profile_page_wardrobe", |c| {
                c.qianxing_profile_page_wardrobe = PathBuf::new()
            }),
            ("templates.teyvat_profile_adventure_rank", |c| {
                c.teyvat_profile_adventure_rank = PathBuf::new()
            }),
            ("templates.teyvat_profile_world_level", |c| {
                c.teyvat_profile_world_level = PathBuf::new()
            }),
            ("templates.teyvat_profile_add_friend", |c| {
                c.teyvat_profile_add_friend = PathBuf::new()
            }),
            ("templates.qianxing_profile_domain_level", |c| {
                c.qianxing_profile_domain_level = PathBuf::new()
            }),
            ("templates.qianxing_profile_add_friend", |c| {
                c.qianxing_profile_add_friend = PathBuf::new()
            }),
            ("templates.qianxing_profile_view_profile", |c| {
                c.qianxing_profile_view_profile = PathBuf::new()
            }),
        ];
        for (field, clear) in fields {
            let mut config = TemplateConfig::default();
            clear(&mut config);
            let error = config
                .validate()
                .expect_err("空模板路径必须被拒绝")
                .to_string();
            assert!(
                error.contains(field),
                "错误信息必须点名 {field}，实际：{error}"
            );
        }
    }

    /// 默认模板（含 `main_menu`）必须齐全，否则 `AppConfig::default()` 无法建库。
    #[test]
    fn default_template_config_passes_validation() {
        let config = TemplateConfig::default();
        config.validate().expect("默认模板配置必须通过校验");
        assert!(
            !config.main_menu.as_os_str().is_empty(),
            "main_menu 必须有默认值"
        );
    }
}
