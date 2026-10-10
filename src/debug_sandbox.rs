//! AI 可驱动的沙盒/插件协议核心模型。
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// 沙盒插件可以申请的能力。未出现在 manifest 中的能力不会自动获得。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Capability {
    Observe,
    Input,
    Task,
    Assert,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PluginApiVersion {
    pub major: u16,
    #[serde(default)]
    pub minor: u16,
}

impl PluginApiVersion {
    pub(crate) const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
    pub(crate) const fn compatible_with(self, host: Self) -> bool {
        self.major == host.major && self.minor <= host.minor
    }
}

fn default_read_only_capabilities() -> BTreeSet<Capability> {
    [Capability::Observe].into_iter().collect()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct PluginManifest {
    pub manifest_version: u16,
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub api: PluginApiVersion,
    /// 未声明时默认为 observe，只读插件不需要申请写能力。
    #[serde(default = "default_read_only_capabilities")]
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManifestError {
    EmptyField(&'static str),
    UnsupportedManifestVersion(u16),
    IncompatibleApi {
        plugin: PluginApiVersion,
        host: PluginApiVersion,
    },
    CapabilityDenied(Capability),
    InputRequiresDebugMode,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "manifest 字段不能为空: {field}"),
            Self::UnsupportedManifestVersion(v) => write!(f, "不支持的 manifest 版本: {v}"),
            Self::IncompatibleApi { plugin, host } => write!(
                f,
                "插件 API {}.{} 与宿主 API {}.{} 不兼容",
                plugin.major, plugin.minor, host.major, host.minor
            ),
            Self::CapabilityDenied(c) => write!(f, "插件能力未获授权: {c:?}"),
            Self::InputRequiresDebugMode => f.write_str("input 能力只能在调试模式下授权"),
        }
    }
}
impl std::error::Error for ManifestError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityPolicy {
    /// 宿主能力上限；默认仅允许 observe。
    pub allowed: BTreeSet<Capability>,
    pub debug_mode: bool,
}
impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self {
            allowed: [Capability::Observe].into_iter().collect(),
            debug_mode: false,
        }
    }
}

impl PluginManifest {
    pub(crate) fn is_read_only(&self) -> bool {
        !self
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::Input | Capability::Task | Capability::Assert))
    }
    pub(crate) fn requests(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
    /// 每次操作重新校验当前策略；task/assert 不隐含 input。
    pub(crate) fn authorize(
        &self,
        capability: Capability,
        host_api: PluginApiVersion,
        policy: &CapabilityPolicy,
    ) -> Result<(), ManifestError> {
        self.validate(host_api, policy)?;
        if !self.requests(capability) {
            return Err(ManifestError::CapabilityDenied(capability));
        }
        Ok(())
    }
    pub(crate) fn validate(
        &self,
        host_api: PluginApiVersion,
        policy: &CapabilityPolicy,
    ) -> Result<(), ManifestError> {
        if self.manifest_version != 1 {
            return Err(ManifestError::UnsupportedManifestVersion(
                self.manifest_version,
            ));
        }
        if self.plugin_id.trim().is_empty() {
            return Err(ManifestError::EmptyField("plugin_id"));
        }
        if self.name.trim().is_empty() {
            return Err(ManifestError::EmptyField("name"));
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::EmptyField("version"));
        }
        if !self.api.compatible_with(host_api) {
            return Err(ManifestError::IncompatibleApi {
                plugin: self.api,
                host: host_api,
            });
        }
        for capability in &self.capabilities {
            if !policy.allowed.contains(capability) {
                return Err(ManifestError::CapabilityDenied(*capability));
            }
            if *capability == Capability::Input && !policy.debug_mode {
                return Err(ManifestError::InputRequiresDebugMode);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AtomicCommand {
    Capture,
    EnsureWindow,
    EnsureForeground,
    EnsureReady {
        after_activate_ms: u64,
    },
    Activate {
        after_activate_ms: u64,
    },
    Focus {
        after_activate_ms: u64,
    },
    InputText {
        text: String,
        settle_ms: u64,
    },
    CloseWindow,
    PressKey {
        key: String,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    MouseDown {
        button: String,
    },
    MouseUp {
        button: String,
    },
    Click {
        x: i32,
        y: i32,
    },
    Scroll {
        x: i32,
        y: i32,
        amount: i32,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    Paste {
        text: String,
    },
    PlaybackQueueClear,
    PlaybackQueueRemoveFront,
    /// 恢复播放。业务动作走独立的窄边界（见 PlaybackAction），不经过 UI runtime。
    PlaybackPlay,
    PlaybackPause,
    PlaybackSkipNext,
    PlaybackSkipPrevious,
    PlaybackSetVolume {
        volume: u8,
    },
    /// 点歌入队：提交一次点歌请求，由应用自己的去重与排队逻辑处理。
    RequestSong {
        keyword: String,
        #[serde(default)]
        source: String,
        #[serde(default)]
        prefer_accompaniment: bool,
    },
    /// 聊天发送：提交一次游戏内聊天，由应用自己的控制台聊天入队逻辑处理。
    /// 默认沿用应用控制台前缀；显式传空前缀即表示"不带前缀"，两种需求都表达得出来。
    ChatSend {
        text: String,
        #[serde(default = "default_chat_prefix")]
        prefix: String,
    },
    /// 游戏启动：拉起游戏本体，或进入千星奇域。
    /// kind 只接受 start_game / enter_wonderland，其余在任务校验阶段显式拒绝。
    StartupTask {
        kind: String,
    },
    TaskQueueCancel {
        task_id: u64,
    },
    Wait {
        milliseconds: u64,
    },
}

impl AtomicCommand {
    /// 只观测白名单；确保窗口/前台仅检查，不激活、聚焦或恢复输入设备。
    /// 新增命令默认拒绝，业务命令即使不经过 UI runtime 也不属于只读。
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Capture | Self::Wait { .. } | Self::EnsureWindow | Self::EnsureForeground
        )
    }
}

/// 聊天发送的默认前缀，与应用控制台指令保持一致。
fn default_chat_prefix() -> String {
    "[控制台]: ".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SandboxFixture {
    pub width: u32,
    pub height: u32,
    pub rgba: [u8; 4],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SandboxTask {
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub description: String,
    pub commands: Vec<AtomicCommand>,
    #[serde(default)]
    pub capture_after_each: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SandboxStatus {
    Draft,
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxCommandResult {
    pub index: usize,
    pub ok: bool,
    pub status: String,
    pub error: Option<String>,
    pub frame_sequence: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxRun {
    pub run_id: String,
    pub task_id: String,
    pub task_version: u32,
    pub status: SandboxStatus,
    pub results: Vec<SandboxCommandResult>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SandboxLimits {
    pub max_commands: usize,
    pub max_text_bytes: usize,
    pub max_wait_ms: u64,
    pub max_coordinate: i32,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_commands: 512,
            max_text_bytes: 16 * 1024,
            max_wait_ms: 30_000,
            max_coordinate: 100_000,
        }
    }
}

impl SandboxTask {
    pub(crate) fn validate(&self, limits: SandboxLimits) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("task id 不能为空".into());
        }
        if self.version == 0 {
            return Err("task version 必须大于 0".into());
        }
        if self.commands.is_empty() {
            return Err("task 至少需要一个原子命令".into());
        }
        if self.commands.len() > limits.max_commands {
            return Err(format!("原子命令数量超过上限 {}", limits.max_commands));
        }
        for (index, command) in self.commands.iter().enumerate() {
            let point = |x: i32, y: i32| {
                x.checked_abs()
                    .is_some_and(|value| value <= limits.max_coordinate)
                    && y.checked_abs()
                        .is_some_and(|value| value <= limits.max_coordinate)
            };
            match command {
                AtomicCommand::PressKey { key }
                | AtomicCommand::KeyDown { key }
                | AtomicCommand::KeyUp { key }
                    if key.trim().is_empty() =>
                {
                    return Err(format!("命令 {index}: key 不能为空"));
                }
                AtomicCommand::MouseDown { button } | AtomicCommand::MouseUp { button }
                    if !matches!(button.as_str(), "left" | "right" | "middle") =>
                {
                    return Err(format!(
                        "命令 {index}: 鼠标按钮必须为 left、right 或 middle"
                    ));
                }
                AtomicCommand::Scroll { amount, .. } if amount.unsigned_abs() > 10_000 => {
                    return Err(format!("命令 {index}: 滚动量超过上限"));
                }
                AtomicCommand::Click { x, y } | AtomicCommand::Scroll { x, y, .. }
                    if !point(*x, *y) =>
                {
                    return Err(format!("命令 {index}: 坐标越界"));
                }
                AtomicCommand::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } if !point(*from_x, *from_y) || !point(*to_x, *to_y) => {
                    return Err(format!("命令 {index}: 拖拽坐标越界"));
                }
                AtomicCommand::Paste { text } | AtomicCommand::InputText { text, .. }
                    if text.len() > limits.max_text_bytes =>
                {
                    return Err(format!("命令 {index}: 文本超过上限"));
                }
                AtomicCommand::InputText { settle_ms, .. } if *settle_ms > limits.max_wait_ms => {
                    return Err(format!("命令 {index}: 输入等待时间超过上限"));
                }
                AtomicCommand::EnsureReady { after_activate_ms }
                | AtomicCommand::Activate { after_activate_ms }
                | AtomicCommand::Focus { after_activate_ms }
                    if *after_activate_ms > limits.max_wait_ms =>
                {
                    return Err(format!("命令 {index}: 激活等待时间超过上限"));
                }
                AtomicCommand::Wait { milliseconds } if *milliseconds > limits.max_wait_ms => {
                    return Err(format!("命令 {index}: 等待时间超过上限"));
                }
                AtomicCommand::PlaybackSetVolume { volume } if *volume > 100 => {
                    return Err(format!("命令 {index}: 音量必须在 0 到 100 之间"));
                }
                AtomicCommand::RequestSong { keyword, .. } if keyword.trim().is_empty() => {
                    return Err(format!("命令 {index}: 点歌关键词不能为空"));
                }
                AtomicCommand::RequestSong { keyword, .. }
                    if keyword.len() > limits.max_text_bytes =>
                {
                    return Err(format!("命令 {index}: 点歌关键词超过上限"));
                }
                AtomicCommand::ChatSend { text, .. } if text.trim().is_empty() => {
                    return Err(format!("命令 {index}: 聊天内容不能为空"));
                }
                AtomicCommand::ChatSend { text, .. } if text.len() > limits.max_text_bytes => {
                    return Err(format!("命令 {index}: 聊天内容超过上限"));
                }
                AtomicCommand::ChatSend { prefix, .. } if prefix.len() > limits.max_text_bytes => {
                    return Err(format!("命令 {index}: 聊天前缀超过上限"));
                }
                AtomicCommand::StartupTask { kind }
                    if !matches!(kind.as_str(), "start_game" | "enter_wonderland") =>
                {
                    return Err(format!(
                        "命令 {index}: 启动类型必须是 start_game 或 enter_wonderland"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> PluginManifest {
        serde_json::from_str(r#"{"manifest_version":1,"plugin_id":"demo","name":"Demo","version":"1.0.0","api":{"major":1}}"#).unwrap()
    }
    /// 点歌命令的线格式与关键词校验：可选字段必须能省略，空关键词必须被拒绝。
    #[test]
    fn request_song_wire_shape_and_keyword_validation() {
        let parse = |json: &str| serde_json::from_str::<AtomicCommand>(json).unwrap();
        assert_eq!(
            parse(r#"{"request_song":{"keyword":"a"}}"#),
            AtomicCommand::RequestSong {
                keyword: "a".into(),
                source: String::new(),
                prefer_accompaniment: false,
            },
            "source 与 prefer_accompaniment 必须可省略"
        );
        let task = |keyword: &str| SandboxTask {
            id: "song".into(),
            version: 1,
            description: String::new(),
            commands: vec![AtomicCommand::RequestSong {
                keyword: keyword.into(),
                source: String::new(),
                prefer_accompaniment: false,
            }],
            capture_after_each: false,
        };
        task("千本桜").validate(SandboxLimits::default()).unwrap();
        assert!(
            task("   ").validate(SandboxLimits::default()).is_err(),
            "空关键词必须被拒绝"
        );
    }

    /// 聊天命令的线格式与校验：前缀可省略且默认沿用控制台前缀，
    /// 空前缀必须能表达"不带前缀"，空正文必须被显式拒绝而不是静默失败。
    #[test]
    fn chat_send_wire_shape_and_text_validation() {
        let parse = |json: &str| serde_json::from_str::<AtomicCommand>(json).unwrap();
        assert_eq!(
            parse(r#"{"chat_send":{"text":"你好"}}"#),
            AtomicCommand::ChatSend {
                text: "你好".into(),
                prefix: "[控制台]: ".into(),
            },
            "prefix 省略时必须沿用应用控制台前缀"
        );
        assert_eq!(
            parse(r#"{"chat_send":{"text":"你好","prefix":""}}"#),
            AtomicCommand::ChatSend {
                text: "你好".into(),
                prefix: String::new(),
            },
            "空前缀必须能表达不带前缀"
        );
        let limits = SandboxLimits::default();
        let task = |text: String| SandboxTask {
            id: "chat".into(),
            version: 1,
            description: String::new(),
            commands: vec![AtomicCommand::ChatSend {
                text,
                prefix: String::new(),
            }],
            capture_after_each: false,
        };
        task("你好".into()).validate(limits).unwrap();
        assert!(
            task("   ".into()).validate(limits).is_err(),
            "空聊天内容必须被拒绝，而不是静默失败"
        );
        assert!(
            task("x".repeat(limits.max_text_bytes + 1))
                .validate(limits)
                .is_err(),
            "超长聊天内容必须被拒绝"
        );
    }

    /// 游戏启动命令的线格式与类型校验：只接受两个合法取值，
    /// 其余必须在**任务校验阶段**被显式拒绝，而不是下发后静默失败。
    #[test]
    fn startup_task_wire_shape_and_kind_validation() {
        let parse = |json: &str| serde_json::from_str::<AtomicCommand>(json).unwrap();
        assert_eq!(
            parse(r#"{"startup_task":{"kind":"start_game"}}"#),
            AtomicCommand::StartupTask {
                kind: "start_game".into(),
            }
        );
        let limits = SandboxLimits::default();
        let task = |kind: &str| SandboxTask {
            id: "startup".into(),
            version: 1,
            description: String::new(),
            commands: vec![AtomicCommand::StartupTask { kind: kind.into() }],
            capture_after_each: false,
        };
        task("start_game").validate(limits).unwrap();
        task("enter_wonderland").validate(limits).unwrap();
        for invalid in ["", "start", "StartGame", "enter_wonderland "] {
            assert!(
                task(invalid).validate(limits).is_err(),
                "非法启动类型 {invalid:?} 必须被拒绝"
            );
        }
    }

    #[test]
    fn manifest_defaults_do_not_grant_write_capabilities() {
        let value = manifest();
        assert!(value.is_read_only());
        let host = PluginApiVersion::new(1, 0);
        let policy = CapabilityPolicy::default();
        value.validate(host, &policy).unwrap();
        for capability in [Capability::Input, Capability::Task, Capability::Assert] {
            assert_eq!(
                value.authorize(capability, host, &policy),
                Err(ManifestError::CapabilityDenied(capability))
            );
        }
    }
    #[test]
    fn manifest_rejects_unknown_fields_and_capabilities() {
        let base = serde_json::to_value(manifest()).unwrap();
        let mut value = base.clone();
        value["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PluginManifest>(value).is_err());
        let mut value = base.clone();
        value["api"]["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PluginManifest>(value).is_err());
        let mut value = base;
        value["capabilities"] = serde_json::json!(["shell"]);
        assert!(serde_json::from_value::<PluginManifest>(value).is_err());
    }
    #[test]
    fn plugin_api_requires_same_major_and_supported_minor() {
        let mut value = manifest();
        value.api = PluginApiVersion::new(1, 2);
        value
            .validate(PluginApiVersion::new(1, 3), &CapabilityPolicy::default())
            .unwrap();
        for host in [PluginApiVersion::new(1, 1), PluginApiVersion::new(2, 3)] {
            assert!(matches!(
                value.validate(host, &CapabilityPolicy::default()),
                Err(ManifestError::IncompatibleApi { .. })
            ));
        }
        value.manifest_version = 2;
        assert!(matches!(
            value.validate(PluginApiVersion::new(1, 3), &CapabilityPolicy::default()),
            Err(ManifestError::UnsupportedManifestVersion(2))
        ));
    }
    #[test]
    fn task_does_not_imply_input_and_input_requires_debug() {
        let mut value = manifest();
        value.capabilities.insert(Capability::Task);
        let host = PluginApiVersion::new(1, 0);
        let mut policy = CapabilityPolicy {
            allowed: [Capability::Observe, Capability::Task, Capability::Input]
                .into_iter()
                .collect(),
            debug_mode: false,
        };
        value.authorize(Capability::Task, host, &policy).unwrap();
        assert_eq!(
            value.authorize(Capability::Input, host, &policy),
            Err(ManifestError::CapabilityDenied(Capability::Input))
        );
        value.capabilities.insert(Capability::Input);
        assert_eq!(
            value.authorize(Capability::Input, host, &policy),
            Err(ManifestError::InputRequiresDebugMode)
        );
        policy.debug_mode = true;
        value.authorize(Capability::Input, host, &policy).unwrap();
        policy.allowed.remove(&Capability::Input);
        assert_eq!(
            value.authorize(Capability::Input, host, &policy),
            Err(ManifestError::CapabilityDenied(Capability::Input))
        );
    }
    fn task(commands: Vec<AtomicCommand>) -> SandboxTask {
        SandboxTask {
            id: "demo".into(),
            version: 1,
            description: String::new(),
            commands,
            capture_after_each: false,
        }
    }
    #[test]
    fn rejects_unbounded_input() {
        assert!(
            task(vec![AtomicCommand::Paste {
                text: "x".repeat(20_000)
            }])
            .validate(SandboxLimits::default())
            .is_err()
        );
    }
    #[test]
    fn rejects_unbounded_text_input_and_activation_delays() {
        let limits = SandboxLimits::default();
        assert!(
            task(vec![AtomicCommand::InputText {
                text: "x".repeat(limits.max_text_bytes + 1),
                settle_ms: 0
            }])
            .validate(limits)
            .is_err()
        );
        assert!(
            task(vec![AtomicCommand::InputText {
                text: "ok".into(),
                settle_ms: limits.max_wait_ms + 1
            }])
            .validate(limits)
            .is_err()
        );
        for command in [
            AtomicCommand::EnsureReady {
                after_activate_ms: limits.max_wait_ms + 1,
            },
            AtomicCommand::Activate {
                after_activate_ms: limits.max_wait_ms + 1,
            },
            AtomicCommand::Focus {
                after_activate_ms: limits.max_wait_ms + 1,
            },
        ] {
            assert!(task(vec![command]).validate(limits).is_err());
        }
        assert!(
            task(vec![AtomicCommand::InputText {
                text: "x".repeat(limits.max_text_bytes),
                settle_ms: limits.max_wait_ms
            }])
            .validate(limits)
            .is_ok()
        );
    }

    #[test]
    fn rejects_extreme_scroll_amount() {
        for amount in [i32::MIN, i32::MAX, 10_001] {
            assert!(
                task(vec![AtomicCommand::Scroll { x: 0, y: 0, amount }])
                    .validate(SandboxLimits::default())
                    .is_err()
            );
        }
    }
    #[test]
    fn accepts_small_atomic_task() {
        assert!(
            task(vec![
                AtomicCommand::Click { x: 10, y: 20 },
                AtomicCommand::Capture
            ])
            .validate(SandboxLimits::default())
            .is_ok()
        );
    }
    #[test]
    fn rejects_integer_min_coordinate_without_panicking() {
        assert!(
            task(vec![AtomicCommand::Click { x: i32::MIN, y: 0 }])
                .validate(SandboxLimits::default())
                .is_err()
        );
    }
}
