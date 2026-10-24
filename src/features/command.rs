use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::administration::AdministrationCommand;
use super::card_games::LandlordCommand;
use super::custom_workflow::CustomWorkflowCommand;
use super::hall::HallCommand;
use super::identity::IdentityRole;
use super::idiom_chain::IdiomChainCommand;
use super::invite::InviteCommand;
use super::moderation::ModerationCommand;
use super::playback::PlaybackCommand;
use super::song_request::SongCommand;
use super::turtle_soup::TurtleSoupCommand;
use super::undercover::UndercoverCommand;
use crate::observation::chat::{ObservationFrameId, ObservedChatMessageId};

/// 命令来源：大厅成员或好友私聊；身份角色在路由层单独判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAuthority {
    HallMember,
    Friend,
}

/// 命令前缀：@ 用于功能命令，# 用于娱乐玩法或玩法中的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandPrefix {
    At,
    Hash,
    /// 测试专用前缀（& / ＆）：只用于在观察/新鲜度层造样本，**永不路由到任何业务模块**。
    Amp,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandObservation {
    pub(crate) frame_id: Option<ObservationFrameId>,
    pub(crate) captured_at: Option<Instant>,
    pub(crate) message_id: Option<ObservedChatMessageId>,
    /// 被观察消息自身的原始文本；命令锁只认这份数据，不回头查 tracker。
    pub(crate) message_text: Option<String>,
    /// 被观察消息自身的屏幕 y（矩形顶部）；命令锁用它做锚点位置判定。
    pub(crate) message_y: Option<i32>,
}

/// 尚未选择或解析业务模块前的聊天输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandEnvelope {
    original_text: String,
    user_command: String,
    command_text: String,
    message_type: String,
    username: String,
    prefix: CommandPrefix,
    authority: CommandAuthority,
    observation: CommandObservation,
    /// 消息来源的账号（仅外部聊天 API 提供）。
    ///
    /// OCR 路径为 None——它只有昵称；API 路径带上 UID，
    /// 权限解析时 UID 优先于昵称。
    account: Option<CommandAccount>,
}

/// 一条消息对应的游戏账号（定义在 kernel，观察层也要用）。
pub(crate) use miliastra_kernel::chat_source::CommandAccount;

/// 是否有足以承担命令身份的标识。
///
/// 身份来源按可信度排序：账号 UID 优先，昵称次之。有 UID 或 有非空昵称都算
/// 满足；两者都没有就是 Unknown。Unknown 不授权任何命令——MUST_READ §2 的
/// 「未确认发送者不能用于授权命令」是硬约束，不能为了「能跑」放宽。
///
/// 主动作用：外部聊天源的昵称可能为 null（API 文档 §5：异步到达、服务端不
/// 补发），但 `from.uid` 必填。只按昵称判断会让这类消息整条丢失。
pub(crate) fn has_known_identity(nickname: Option<&str>, account: Option<&CommandAccount>) -> bool {
    if account.is_some_and(|account| !account.uid.trim().is_empty()) {
        return true;
    }
    nickname.is_some_and(|nickname| !nickname.trim().is_empty())
}

impl CommandEnvelope {
    /// 附加账号信息（外部聊天 API 路径用）。
    pub(crate) fn with_account(mut self, account: Option<CommandAccount>) -> Self {
        self.account = account;
        self
    }

    pub(crate) fn account(&self) -> Option<&CommandAccount> {
        self.account.as_ref()
    }

    pub(crate) fn new(
        original_text: impl Into<String>,
        username: impl Into<String>,
        message_type: impl Into<String>,
        user_command: impl Into<String>,
        observation: CommandObservation,
    ) -> Option<Self> {
        let username = username.into();
        if username.trim().is_empty() {
            return None;
        }
        let message_type = message_type.into();
        let authority = match message_type.as_str() {
            "blue" => CommandAuthority::HallMember,
            "pink" => CommandAuthority::Friend,
            _ => return None,
        };
        let user_command = user_command.into();
        let trimmed_command = user_command.trim();
        // 通用 OCR 清理会删除末尾括号，但 `J` 也可能被识别成 `]`/`】`。
        // 含 BV 片段时保留该字符，让 B 站归一化器负责修复；其他命令继续沿用
        // 原有的括号清理行为。
        let user_command = if trimmed_command.ends_with([']', '】'])
            && !miliastra_playback::bilibili_normalize_bvid(trimmed_command).is_some()
        {
            trimmed_command
                .trim_end_matches([']', '】'])
                .trim_end()
                .to_string()
        } else {
            trimmed_command.to_string()
        };
        // 默认无账号：OCR 路径不提供；API 路径在构造后用 `with_account` 补上。
        let account = None;
        let (prefix, command_text) = if let Some(text) = user_command
            .strip_prefix('@')
            .or_else(|| user_command.strip_prefix('＠'))
        {
            (CommandPrefix::At, text)
        } else if let Some(text) = user_command
            .strip_prefix('&')
            .or_else(|| user_command.strip_prefix('＆'))
        {
            // 测试专用前缀：保留信封以便观察层记录，但路由层不会认领。
            (CommandPrefix::Amp, text)
        } else {
            (
                CommandPrefix::Hash,
                user_command
                    .strip_prefix('#')
                    .or_else(|| user_command.strip_prefix('＃'))?,
            )
        };
        let command_text = command_text.trim_start().to_string();
        if command_text.is_empty() {
            return None;
        }
        Some(Self {
            original_text: original_text.into(),
            user_command,
            command_text,
            message_type,
            username,
            prefix,
            authority,
            observation,
            account,
        })
    }

    /// 生成仅替换命令来源权限的副本，用于跨来源权限路由。
    pub(crate) fn with_authority(&self, authority: CommandAuthority) -> Self {
        Self {
            message_type: match authority {
                CommandAuthority::HallMember => "blue".to_string(),
                CommandAuthority::Friend => "pink".to_string(),
            },
            authority,
            ..self.clone()
        }
    }

    pub(crate) fn with_command_text(&self, command_text: impl Into<String>) -> Self {
        let command_text = command_text.into();
        let user_command = match self.prefix {
            CommandPrefix::At => format!("@{command_text}"),
            CommandPrefix::Hash => format!("#{command_text}"),
            CommandPrefix::Amp => format!("&{command_text}"),
        };
        Self {
            original_text: self.original_text.clone(),
            user_command,
            command_text,
            message_type: self.message_type.clone(),
            username: self.username.clone(),
            prefix: self.prefix,
            authority: self.authority,
            observation: self.observation.clone(),
            account: self.account.clone(),
        }
    }

    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn user_command(&self) -> &str {
        &self.user_command
    }

    pub(crate) fn command_text(&self) -> &str {
        &self.command_text
    }

    pub(crate) fn message_type(&self) -> &str {
        &self.message_type
    }

    pub(crate) const fn prefix(&self) -> CommandPrefix {
        self.prefix
    }

    pub(crate) const fn authority(&self) -> CommandAuthority {
        self.authority
    }

    pub(crate) fn observation(&self) -> &CommandObservation {
        &self.observation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FeatureCommandMatch<T> {
    pub(crate) matched: String,
    pub(crate) raw: String,
    pub(crate) command: T,
}

impl<T> FeatureCommandMatch<T> {
    pub(crate) fn new(matched: impl Into<String>, raw: impl Into<String>, command: T) -> Self {
        Self {
            matched: matched.into(),
            raw: raw.into(),
            command,
        }
    }

    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> FeatureCommandMatch<U> {
        FeatureCommandMatch {
            matched: self.matched,
            raw: self.raw,
            command: map(self.command),
        }
    }
}

/// 认领该命令的业务模块。
///
/// 住在命令模型里（而不是路由层），是因为它描述的是"这条命令属于谁"这一领域事实；
/// 面板也要用它做分组，而 features 层不该反向依赖 interfaces。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatCommandModule {
    SongRequest,
    Playback,
    Hall,
    Administration,
    IdiomChain,
    CardGame,
    TurtleSoup,
    Tarot,
    Playlist,
    Undercover,
    Invite,
    Moderation,
    CustomWorkflow,
    AiChat,
}

/// ADR 0059 定义的顶层小型路由枚举。
///
/// 每种载荷类型由对应业务模块拥有。聊天路由完成后，此枚举标识选中的模块，也允许非聊天适配器直接提交类型化模块命令。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ModuleCommand {
    SongRequest(SongCommand),
    Playback(PlaybackCommand),
    Hall(HallCommand),
    Administration(AdministrationCommand),
    IdiomChain(IdiomChainCommand),
    CardGame(LandlordCommand),
    TurtleSoup(TurtleSoupCommand),
    Tarot(super::tarot::TarotCommand),
    Playlist(super::playlist::PlaylistCommand),
    Undercover(UndercoverCommand),
    Invite(InviteCommand),
    Moderation(ModerationCommand),
    CustomWorkflow(CustomWorkflowCommand),
    AiChat(super::ai_chat::AiChatCommand),
}

/// 命令是否携带不能进证据流的私密内容。
///
/// 这是**必须回答**的问题：返回 `None` 表示"参数可以公开"，会原样进入
/// 可导出的证据流。新增玩法时若这个 match 没有穷尽覆盖，编译器会报错——
/// 之前它在 `debug/read_only.rs` 里是个带 `_ => None` 的手写 match，
/// 漏改不会编译失败、不会测试失败，只会静默把私密内容写进证据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivatePayload {
    /// 审核/拉黑的私聊输入。
    ModerationInput,
    /// 塔罗的私密提问。
    TarotQuestion,
    /// 谁是卧底的投票/弃权/揭示。
    UndercoverInput,
    /// AI 聊天的提问。
    AiChatQuestion,
}

impl PrivatePayload {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::ModerationInput => "private moderation input",
            Self::TarotQuestion => "private AI input",
            Self::UndercoverInput => "private game input",
            Self::AiChatQuestion => "private AI chat input",
        }
    }
}

impl ModuleCommand {
    /// 这条命令是否携带私密内容。
    ///
    /// **不要加 `_ =>` 兜底**：那会让新增的 `ModuleCommand` 变体静默落进
    /// "可公开"分支。宁可让编译器逼着写一行 `=> None`。
    pub(crate) fn private_payload(&self, message_type: &str) -> Option<PrivatePayload> {
        match self {
            Self::Moderation(_) if message_type == "pink" => Some(PrivatePayload::ModerationInput),
            Self::Tarot(super::tarot::TarotCommand::Draw { ai: true, .. })
                if message_type == "pink" =>
            {
                Some(PrivatePayload::TarotQuestion)
            }
            Self::Undercover(
                UndercoverCommand::Vote(_) | UndercoverCommand::Abstain | UndercoverCommand::Reveal,
            ) => Some(PrivatePayload::UndercoverInput),
            // 开局/加入等不含私密输入（玩家自己的身份由游戏内展示）。
            Self::Undercover(_) => None,
            Self::AiChat(_) => Some(PrivatePayload::AiChatQuestion),
            // 公屏里的审核触发不含私密内容；只有私聊输入才需要脱敏（上面已匹配）。
            Self::Moderation(_) => None,
            // 以下玩法不含需要脱敏的用户输入。
            Self::SongRequest(_)
            | Self::Playback(_)
            | Self::Hall(_)
            | Self::Administration(_)
            | Self::IdiomChain(_)
            | Self::CardGame(_)
            | Self::TurtleSoup(_)
            | Self::Tarot(_)
            | Self::Playlist(_)
            | Self::Invite(_)
            | Self::CustomWorkflow(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutedCommand {
    pub(crate) matched: String,
    pub(crate) raw: String,
    pub(crate) user_command: String,
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) authority: CommandAuthority,
    pub(crate) role: Option<IdentityRole>,
    pub(crate) permission_required: Option<IdentityRole>,
    pub(crate) command: ModuleCommand,
    pub(crate) observation: CommandObservation,
}

impl RoutedCommand {
    pub(crate) fn from_envelope(
        envelope: &CommandEnvelope,
        matched: FeatureCommandMatch<ModuleCommand>,
    ) -> Self {
        debug_assert!(!envelope.original_text.trim().is_empty());
        Self {
            matched: matched.matched,
            raw: matched.raw,
            user_command: envelope.user_command().to_string(),
            message_type: envelope.message_type.clone(),
            username: envelope.username.clone(),
            authority: envelope.authority,
            role: None,
            permission_required: None,
            command: matched.command,
            observation: envelope.observation().clone(),
        }
    }

    pub(crate) fn console(
        matched: impl Into<String>,
        raw: impl Into<String>,
        command: ModuleCommand,
    ) -> Self {
        let raw = raw.into();
        Self {
            matched: matched.into(),
            user_command: format!("@{raw}"),
            raw,
            message_type: "控制台".to_string(),
            username: "控制台".to_string(),
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            command,
            observation: CommandObservation {
                captured_at: Some(Instant::now()),
                ..CommandObservation::default()
            },
        }
    }
}

impl ModuleCommand {
    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::SongRequest(command) => command.lock_key(),
            Self::Playback(command) => command.lock_key(),
            Self::Hall(command) => command.lock_key(),
            Self::Administration(command) => command.lock_key(),
            Self::IdiomChain(command) => command.lock_key(),
            Self::CardGame(command) => command.lock_key(),
            Self::TurtleSoup(command) => command.lock_key().to_string(),
            Self::Undercover(command) => command.lock_key(),
            Self::Tarot(command) => command.lock_key(),
            Self::Playlist(command) => command.lock_key(),
            Self::Invite(command) => command.lock_key(),
            Self::Moderation(command) => command.lock_key(),
            Self::CustomWorkflow(command) => command.lock_key(),
            // AI 聊天按人加锁：同一人重复触发被拦，不同人互不影响，
            // 由 scopes_lock_to_actor 追加发送者身份。
            Self::AiChat(command) => command.lock_key(),
        }
    }

    pub(crate) fn scopes_lock_to_actor(&self) -> bool {
        matches!(
            self,
            Self::CardGame(_)
                | Self::Undercover(_)
                | Self::Tarot(_)
                | Self::Playlist(_)
                | Self::AiChat(_)
        )
    }

    /// 当前大厅观察到的命令是否需要真实发言者身份。
    ///
    /// 日志和审计元数据不属于身份依赖。此标志仅用于业务结果、授权、回合归属或投递目标会随发言者变化的命令。
    pub(crate) fn requires_hall_sender(&self) -> bool {
        match self {
            Self::SongRequest(_) | Self::Playback(_) | Self::Hall(_) | Self::Administration(_) => {
                false
            }
            Self::IdiomChain(command) => matches!(
                command,
                IdiomChainCommand::Start { .. }
                    | IdiomChainCommand::Submit(_)
                    | IdiomChainCommand::Stop
            ),
            Self::CardGame(command) => {
                !matches!(command, LandlordCommand::Status | LandlordCommand::Retry)
            }
            Self::TurtleSoup(command) => matches!(command, TurtleSoupCommand::Start),
            Self::Tarot(command) => !matches!(command, super::tarot::TarotCommand::Help),
            Self::Playlist(command) => !matches!(command, super::playlist::PlaylistCommand::Help),
            Self::Undercover(command) => !matches!(command, UndercoverCommand::Retry),
            // 这些模块目前仅对好友开放，或将触发用户作为执行契约的一部分。后续增加大厅路由时仍保持保守处理。
            Self::Invite(_) | Self::Moderation(_) | Self::CustomWorkflow(_) => true,
            Self::AiChat(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str) -> CommandAccount {
        CommandAccount::new(miliastra_kernel::chat_source::GAME_GENSHIN, uid)
    }

    #[test]
    fn account_uid_alone_counts_as_a_known_identity() {
        // 外部聊天源的昵称是异步到达的（API 文档 §5），缺失时只剩 UID。
        // 这时必须仍算「已知身份」，否则 @ 命令会被整条丢弃。
        assert!(has_known_identity(None, Some(&account("127138439"))));
    }

    #[test]
    fn nickname_alone_still_counts_as_a_known_identity() {
        // OCR 路径没有账号，昵称就是唯一身份来源。
        assert!(has_known_identity(Some("芦荟"), None));
    }

    #[test]
    fn unknown_identity_is_not_authorized() {
        // MUST_READ §2：未确认发送者不能用于授权命令。这里守住「不放宽」。
        assert!(!has_known_identity(None, None));
        assert!(!has_known_identity(Some("   "), None));
        assert!(!has_known_identity(Some(""), Some(&account("  "))));
    }

    #[test]
    fn account_takes_precedence_but_nickname_is_accepted_as_fallback() {
        // 两者都缺失的账号结构不应被当成身份；有昵称时仍成立。
        assert!(!has_known_identity(None, Some(&account(""))));
        assert!(has_known_identity(Some("香菜"), Some(&account(""))));
    }
}
