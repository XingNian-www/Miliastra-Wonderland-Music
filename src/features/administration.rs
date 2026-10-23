use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::features::chat_text::{
    CommandSyntax, is_command_boundary, parse_prefixed_command, strip_ascii_case_prefix,
};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::features::reply_template::{self, ReplyVars};
use crate::runtime::chat_listener::{ChatListenerMode, ChatListenerSnapshot};

const IDLE_EXIT_MIN_MINUTES: u32 = 15;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdministrationCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

pub(crate) trait AdministrationImmediatePort {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()>;
    fn enqueue_hall_reply(&mut self, message: &str) -> Result<()>;
    /// 按模板排队一条大厅回复。
    ///
    /// 默认实现只渲染内置文案，保证不接配置的测试替身也能工作；
    /// 组装层会覆盖它以便查用户设置的覆盖文案。
    fn enqueue_hall_template(
        &mut self,
        spec: &reply_template::ReplyTemplate,
        vars: &ReplyVars,
    ) -> Result<()> {
        self.enqueue_hall_reply(&reply_template::render(spec, None, vars).text)
    }
    fn configure_idle_exit(&mut self, minutes: u32) -> Result<()>;
    fn record_command_activity(&mut self) -> Result<()>;
    fn log_executed(
        &mut self,
        context: &AdministrationCommandContext,
        final_command: &str,
    ) -> Result<()>;
}

pub(crate) trait AdministrationApplicationPort: AdministrationImmediatePort {
    fn send_hall(&mut self, message: &str) -> Result<()>;

    /// 按模板发送一条大厅回复；默认实现只渲染内置文案。
    fn send_hall_template(
        &mut self,
        spec: &reply_template::ReplyTemplate,
        vars: &ReplyVars,
    ) -> Result<()> {
        self.send_hall(&reply_template::render(spec, None, vars).text)
    }
    /// 按顺序发送一条帮助回复。
    ///
    /// 帮助文案是单条模板、多行内容：一行发一条消息由发送路径负责拆条，
    /// 因此这里不再接收模板数组。
    fn send_command_reply_batch(
        &mut self,
        _command_id: &str,
        template: &reply_template::ReplyTemplate,
        vars: &ReplyVars,
        delay_ms: u64,
    ) -> Result<()> {
        let rendered = self.render_template(template, vars);
        let lines = crate::text::split_configured_lines(&rendered);
        let borrowed = lines.iter().map(String::as_str).collect::<Vec<_>>();
        self.send_hall_batch(&borrowed, delay_ms)
    }
    fn send_hall_batch(&mut self, messages: &[&str], delay_ms: u64) -> Result<()>;

    /// 按模板渲染（不发送）。
    ///
    /// 帮助文案必须走这里而不是直接 render，否则 replies.overrides 不生效：
    /// 覆盖文案只有组装层能查到，features 层不知道配置存在。
    /// 默认实现只渲染内置文案，保证不接配置的测试替身也能工作。
    ///
    /// 命名说明：叫 render_template 而不是 render_reply，是为了避开
    /// ApplicationRuntime 上已有的固有方法 render_reply(key, default, vars)
    /// ——同名不同签名在两个 trait 同时可见时会产生方法解析歧义。
    fn render_template(
        &mut self,
        spec: &reply_template::ReplyTemplate,
        vars: &ReplyVars,
    ) -> String {
        reply_template::render(spec, None, vars).text
    }
}

/// 管理命令：帮助、开关命令识别、闲置退出、通告和运行状态控制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImmediateAdministrationOutcome {
    ContinueFormal,
    Handled,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdministrationApplication {
    help_batch_ms: u64,
}

impl AdministrationApplication {
    pub(crate) const fn new(help_batch_ms: u64) -> Self {
        Self { help_batch_ms }
    }

    pub(crate) fn execute<P: AdministrationApplicationPort + ?Sized>(
        &self,
        context: &AdministrationCommandContext,
        command: &AdministrationCommand,
        port: &mut P,
    ) -> Result<()> {
        match command {
            AdministrationCommand::Help => {
                port.log_executed(context, "help")?;
                port.send_command_reply_batch(
                    "@帮助",
                    &reply_template::HELP_SONG_REQUEST,
                    &ReplyVars::new(),
                    self.help_batch_ms,
                )
            }
            AdministrationCommand::EntertainmentHelp => {
                port.log_executed(context, "entertainment help")?;
                port.send_command_reply_batch(
                    "@娱乐帮助",
                    &reply_template::HELP_ENTERTAINMENT,
                    &ReplyVars::new(),
                    self.help_batch_ms,
                )
            }
            AdministrationCommand::SetCommandsEnabled { enabled, .. } => {
                log::info!("收到{}命令", if *enabled { "启用" } else { "禁用" });
                port.set_commands_enabled(*enabled)?;
                port.log_executed(
                    context,
                    if *enabled {
                        "enable commands"
                    } else {
                        "disable commands"
                    },
                )?;
                port.send_hall_template(
                    command_availability_feedback(*enabled),
                    &ReplyVars::new(),
                )
            }
            AdministrationCommand::IdleExit { minutes } => {
                port.configure_idle_exit((*minutes).max(IDLE_EXIT_MIN_MINUTES))?;
                port.log_executed(context, &format!("idle exit {}", minutes))
            }
            AdministrationCommand::ChatListenerMode(command) => {
                port.log_executed(context, &format!("chat listener {}", command.label()))?;
                log::warn!(
                    "值守模式命令未经过专用队列分发，已只记录: {}",
                    command.label()
                );
                Ok(())
            }
        }
    }

    pub(crate) fn apply_immediate<P: AdministrationImmediatePort + ?Sized>(
        &self,
        context: &AdministrationCommandContext,
        command: &AdministrationCommand,
        propagate_log_error: bool,
        port: &mut P,
    ) -> Result<ImmediateAdministrationOutcome> {
        match command.dispatch() {
            AdministrationDispatch::ConfigureCommandAvailability { enabled } => {
                port.record_command_activity()?;
                port.set_commands_enabled(enabled)?;
                let log_result = port.log_executed(
                    context,
                    if enabled {
                        "enable commands"
                    } else {
                        "disable commands"
                    },
                );
                if propagate_log_error {
                    log_result?;
                } else if let Err(error) = log_result {
                    log::error!("写入执行命令日志失败: {error:#}");
                }
                port.enqueue_hall_template(
                    command_availability_feedback(enabled),
                    &ReplyVars::new(),
                )?;
                Ok(ImmediateAdministrationOutcome::Handled)
            }
            AdministrationDispatch::ConfigureIdleExit { minutes } => {
                port.record_command_activity()?;
                port.configure_idle_exit(minutes.max(IDLE_EXIT_MIN_MINUTES))?;
                let log_result = port.log_executed(context, &format!("idle exit {minutes}"));
                if propagate_log_error {
                    log_result?;
                } else if let Err(error) = log_result {
                    log::error!("写入执行命令日志失败: {error:#}");
                }
                Ok(ImmediateAdministrationOutcome::Handled)
            }
            AdministrationDispatch::FormalTask | AdministrationDispatch::ChatListenerMode(_) => {
                Ok(ImmediateAdministrationOutcome::ContinueFormal)
            }
        }
    }
}

/// 按模板目录渲染一组帮助行。
///
/// 目前帮助行没有业务变量，占位符解析仍走同一条路径，
/// 这样后续给帮助加变量（如版本号、当前音源）时不需要再改这里。

/// 大厅命令开关的回执模板。
///
/// 返回模板而不是成品文案：文案要能在命令面板里改，渲染留给组装层。
fn command_availability_feedback(enabled: bool) -> &'static reply_template::ReplyTemplate {
    if enabled {
        &reply_template::ADMIN_COMMANDS_ENABLED
    } else {
        &reply_template::ADMIN_COMMANDS_DISABLED
    }
}

pub(crate) enum AdministrationMutationIntent {
    RequestChatListenerMode(ChatListenerMode),
    CancelChatListenerModeRequest(ChatListenerMode),
}

pub(crate) enum AdministrationMutationOutcome {
    ChatListenerModeRequested {
        queued: bool,
        snapshot: ChatListenerSnapshot,
    },
    ChatListenerModeRequestCancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ChatListenerModeCommand {
    Primary,
    Secondary,
    Status,
}

impl ChatListenerModeCommand {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Primary => "一级",
            Self::Secondary => "二级",
            Self::Status => "状态",
        }
    }

    fn parse(text: &str) -> Option<CommandSyntax<'_, Self>> {
        let argument = parse_prefixed_command(text, "值守模式", true)?;
        let command = match argument {
            "一级" => Self::Primary,
            "二级" => Self::Secondary,
            "状态" => Self::Status,
            _ => return None,
        };
        Some(CommandSyntax {
            matched: "值守模式",
            argument,
            command,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum AdministrationCommand {
    Help,
    EntertainmentHelp,
    SetCommandsEnabled { enabled: bool, username: String },
    IdleExit { minutes: u32 },
    ChatListenerMode(ChatListenerModeCommand),
}

pub(crate) struct FriendAdministrationMatch {
    pub(crate) matched: &'static str,
    pub(crate) raw: String,
    pub(crate) command: AdministrationCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdministrationDispatch {
    FormalTask,
    ConfigureCommandAvailability { enabled: bool },
    ConfigureIdleExit { minutes: u32 },
    ChatListenerMode(ChatListenerModeCommand),
}

impl AdministrationCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        match (envelope.prefix(), envelope.authority()) {
            (CommandPrefix::At, CommandAuthority::HallMember) => ["娱乐帮助", "帮助"]
                .iter()
                .any(|prefix| envelope.command_text().starts_with(prefix)),
            (CommandPrefix::At, CommandAuthority::Friend) => {
                ["值守模式", "禁用", "启用", "闲置退出"]
                    .iter()
                    .any(|prefix| {
                        strip_ascii_case_prefix(envelope.command_text(), prefix).is_some()
                    })
            }
            (CommandPrefix::Hash, CommandAuthority::HallMember) => {
                matches!(
                    crate::features::chat_text::compact_command(envelope.command_text()).as_str(),
                    "娱乐" | "帮助" | "娱乐帮助"
                )
            }
            _ => false,
        }
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        match (envelope.prefix(), envelope.authority()) {
            (CommandPrefix::At, CommandAuthority::HallMember) => {
                let parsed = Self::parse_hall(envelope.command_text())?;
                let raw = if parsed.argument.is_empty() {
                    parsed.matched.to_string()
                } else {
                    format!("{} {}", parsed.matched, parsed.argument)
                };
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    raw,
                    parsed.command,
                ))
            }
            (CommandPrefix::At, CommandAuthority::Friend) => {
                let parsed = Self::parse_friend(envelope.command_text(), envelope.username())?;
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    parsed.raw,
                    parsed.command,
                ))
            }
            (CommandPrefix::Hash, CommandAuthority::HallMember) => {
                Self::parse_entertainment_start(envelope.command_text())
                    .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
            }
            _ => None,
        }
    }

    pub(crate) const fn dispatch(&self) -> AdministrationDispatch {
        match self {
            Self::Help | Self::EntertainmentHelp => AdministrationDispatch::FormalTask,
            Self::SetCommandsEnabled { enabled, .. } => {
                AdministrationDispatch::ConfigureCommandAvailability { enabled: *enabled }
            }
            Self::IdleExit { minutes } => {
                AdministrationDispatch::ConfigureIdleExit { minutes: *minutes }
            }
            Self::ChatListenerMode(command) => AdministrationDispatch::ChatListenerMode(*command),
        }
    }

    pub(crate) fn parse_hall(text: &str) -> Option<CommandSyntax<'_, Self>> {
        for prefix in ["娱乐帮助", "帮助"] {
            let Some(argument) = parse_prefixed_command(text, prefix, false) else {
                continue;
            };
            let command = match prefix {
                "娱乐帮助" => Self::EntertainmentHelp,
                "帮助" => Self::Help,
                _ => return None,
            };
            return Some(CommandSyntax {
                matched: prefix,
                argument,
                command,
            });
        }
        None
    }

    pub(crate) fn parse_entertainment_start(payload: &str) -> Option<Self> {
        matches!(
            crate::features::chat_text::compact_command(payload).as_str(),
            // `#` 与 `@` 两种前缀都接受完整写法“娱乐帮助”，方便玩家记住同一句。
            "娱乐" | "帮助" | "娱乐帮助"
        )
        .then_some(Self::EntertainmentHelp)
    }

    pub(crate) fn parse_friend(text: &str, username: &str) -> Option<FriendAdministrationMatch> {
        if let Some(parsed) = ChatListenerModeCommand::parse(text) {
            return Some(FriendAdministrationMatch {
                matched: parsed.matched,
                raw: format!("值守模式 {}", parsed.command.label()),
                command: Self::ChatListenerMode(parsed.command),
            });
        }
        for (prefix, enabled) in [("禁用", false), ("启用", true)] {
            let Some(rest) = strip_ascii_case_prefix(text, prefix) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with([']', '】']) {
                return None;
            }
            return Some(FriendAdministrationMatch {
                matched: prefix,
                raw: format!("{} {}", prefix, username),
                command: Self::SetCommandsEnabled {
                    enabled,
                    username: username.to_string(),
                },
            });
        }
        let rest = strip_ascii_case_prefix(text, "闲置退出")?;
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(FriendAdministrationMatch {
                matched: "闲置退出",
                raw: "闲置退出 30".to_string(),
                command: Self::IdleExit { minutes: 30 },
            });
        }
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        let suffix = rest[digits.len()..].trim_start();
        let suffix = suffix
            .strip_prefix("分钟")
            .or_else(|| suffix.strip_prefix('分'))
            .unwrap_or(suffix);
        if !is_command_boundary(suffix.chars().next()) {
            return None;
        }
        let minutes = digits.parse::<u32>().ok()?.max(15);
        Some(FriendAdministrationMatch {
            matched: "闲置退出",
            raw: format!("闲置退出 {}", minutes),
            command: Self::IdleExit { minutes },
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Help => "help".to_string(),
            Self::EntertainmentHelp => "entertainment_help".to_string(),
            Self::SetCommandsEnabled { enabled, .. } => {
                if *enabled {
                    "enable_commands".to_string()
                } else {
                    "disable_commands".to_string()
                }
            }
            Self::IdleExit { minutes } => format!("idle_exit:{minutes}"),
            Self::ChatListenerMode(mode) => format!("chat_listener:{}", mode.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[derive(Default)]
    struct RecordingPort {
        batches: Vec<Vec<String>>,
        commands_enabled: bool,
        replies: Vec<String>,
    }

    impl AdministrationImmediatePort for RecordingPort {
        fn set_commands_enabled(&mut self, enabled: bool) -> Result<()> {
            self.commands_enabled = enabled;
            Ok(())
        }

        fn enqueue_hall_reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn configure_idle_exit(&mut self, _minutes: u32) -> Result<()> {
            Ok(())
        }

        fn record_command_activity(&mut self) -> Result<()> {
            Ok(())
        }

        fn log_executed(
            &mut self,
            _context: &AdministrationCommandContext,
            _final_command: &str,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn alternating_command_switches_complete_without_queued_state_replay() {
        let mut port = RecordingPort::default();
        let application = AdministrationApplication::new(150);
        for enabled in [false, true, false] {
            let context = AdministrationCommandContext {
                message_type: "pink".to_string(),
                username: "好友".to_string(),
                user_command: if enabled { "@启用" } else { "@禁用" }.to_string(),
            };
            let outcome = application
                .apply_immediate(
                    &context,
                    &AdministrationCommand::SetCommandsEnabled {
                        enabled,
                        username: context.username.clone(),
                    },
                    true,
                    &mut port,
                )
                .unwrap();
            assert_eq!(outcome, ImmediateAdministrationOutcome::Handled);
            assert_eq!(port.commands_enabled, enabled);
        }
        assert_eq!(
            port.replies,
            [
                "管理员已禁用大厅命令识别功能",
                "管理员已启用大厅命令识别功能",
                "管理员已禁用大厅命令识别功能",
            ]
        );
        assert!(!port.commands_enabled);
    }

    impl AdministrationApplicationPort for RecordingPort {
        fn send_hall(&mut self, _message: &str) -> Result<()> {
            Ok(())
        }

        fn send_hall_batch(&mut self, messages: &[&str], _delay_ms: u64) -> Result<()> {
            self.batches.push(
                messages
                    .iter()
                    .map(|message| (*message).to_string())
                    .collect(),
            );
            Ok(())
        }
    }

    #[test]
    fn ordinary_help_contains_only_song_request_guidance() {
        let mut port = RecordingPort::default();
        let context = AdministrationCommandContext {
            message_type: "blue".to_string(),
            username: "测试".to_string(),
            user_command: "@帮助".to_string(),
        };
        let application = AdministrationApplication::new(150);

        application
            .execute(&context, &AdministrationCommand::Help, &mut port)
            .expect("help command");

        assert_eq!(port.batches.len(), 1);
        assert_eq!(port.batches[0].len(), 3);
        assert!(port.batches[0].iter().all(|line| line.contains("点歌")));
        assert!(port.batches[0].iter().all(|line| !line.contains("斗地主")));
    }

    /// 帮助文案必须经过 port.render_template，否则 replies.overrides 不生效。
    ///
    /// 这是回归测试：修复前 help_lines 直接调 render(spec, None, ..)，
    /// 导致 12 个 help.* key 登记在册、面板可见，但改了完全不生效。
    #[test]
    fn help_lines_go_through_the_port_so_overrides_can_reach_the_player() {
        #[derive(Default)]
        struct OverridingPort {
            batches: Vec<Vec<String>>,
            rendered_keys: Vec<String>,
        }

        impl AdministrationImmediatePort for OverridingPort {
            fn set_commands_enabled(&mut self, _enabled: bool) -> Result<()> {
                Ok(())
            }
            fn enqueue_hall_reply(&mut self, _message: &str) -> Result<()> {
                Ok(())
            }
            fn configure_idle_exit(&mut self, _minutes: u32) -> Result<()> {
                Ok(())
            }
            fn record_command_activity(&mut self) -> Result<()> {
                Ok(())
            }
            fn log_executed(&mut self, _c: &AdministrationCommandContext, _f: &str) -> Result<()> {
                Ok(())
            }
        }

        impl AdministrationApplicationPort for OverridingPort {
            fn send_hall(&mut self, _message: &str) -> Result<()> {
                Ok(())
            }
            fn send_hall_batch(&mut self, messages: &[&str], _delay_ms: u64) -> Result<()> {
                self.batches
                    .push(messages.iter().map(|m| (*m).to_string()).collect());
                Ok(())
            }
            /// 模拟组装层：为每条模板返回一条"覆盖文案"，
            /// 并记录被请求的 key，用来证明 features 层确实按 key 走了 port。
            fn render_template(
                &mut self,
                spec: &reply_template::ReplyTemplate,
                _vars: &ReplyVars,
            ) -> String {
                self.rendered_keys.push(spec.key.to_string());
                format!("覆盖:{}", spec.key)
            }
        }

        let mut port = OverridingPort::default();
        let context = AdministrationCommandContext {
            message_type: "blue".to_string(),
            username: "测试".to_string(),
            user_command: "@帮助".to_string(),
        };
        AdministrationApplication::new(150)
            .execute(&context, &AdministrationCommand::Help, &mut port)
            .expect("help command");

        // 帮助是单条模板：必须经 port 渲染过一次，才能让用户覆盖生效。
        assert_eq!(port.rendered_keys.len(), 1, "帮助应经 port 渲染");
        assert_eq!(
            port.rendered_keys[0], "help.song_request",
            "应按合并后的 help.song_request 键渲染"
        );
        assert!(
            port.batches[0].iter().all(|line| line.starts_with("覆盖:")),
            "玩家应收到覆盖文案，而不是内置默认: {:?}",
            port.batches[0]
        );
    }
}
