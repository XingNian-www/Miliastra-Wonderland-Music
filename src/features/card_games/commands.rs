//! 牌局命令的解析与锁键。
//!
//! 从 `card_games.rs` 拆出：只处理「一条聊天文本对应哪条命令」，
//! 与对局状态机解耦，便于单独覆盖命令词表。

use crate::features::chat_text::{command_identity, compact_command};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LandlordCommand {
    Start,
    RunFastStart,
    Join,
    /// 叫地主（传统抢地主流程第一阶段）。
    Call,
    Rob,
    Decline,
    Status,
    Play(String),
    Pass,
    Hand,
    Retry,
    Exit,
}

impl LandlordCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && matches!(
                compact_command(envelope.command_text()).as_str(),
                "斗地主" | "跑得快"
            )
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && match envelope.authority() {
                CommandAuthority::HallMember => true,
                CommandAuthority::Friend => compact_command(envelope.command_text()) == "手牌",
            }
    }

    pub(crate) fn parse_start_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_start_chat(envelope) {
            return None;
        }
        Self::parse_start(envelope.command_text())
            .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
    }

    pub(crate) fn parse_active_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_active_chat(envelope) {
            return None;
        }
        let command = match envelope.authority() {
            CommandAuthority::HallMember => Self::parse_hall(envelope.command_text())?,
            CommandAuthority::Friend => Self::parse_friend(envelope.command_text())?,
        };
        Some(FeatureCommandMatch::new(
            "#",
            envelope.command_text(),
            command,
        ))
    }

    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        match compact_command(payload).as_str() {
            "斗地主" => Some(Self::Start),
            "跑得快" => Some(Self::RunFastStart),
            _ => None,
        }
    }

    pub(crate) fn parse_hall(payload: &str) -> Option<Self> {
        let command = match compact_command(payload).as_str() {
            "加入" => Self::Join,
            "叫地主" | "叫" => Self::Call,
            "不叫" | "不抢" => Self::Decline,
            "抢" | "抢地主" => Self::Rob,
            "过" | "要不起" => Self::Pass,
            "状态" => Self::Status,
            "重试" => Self::Retry,
            "结束" => Self::Exit,
            "手牌" => return None,
            _ => {
                let cards = payload
                    .strip_prefix('出')
                    .unwrap_or(payload)
                    .trim_start_matches(['：', ':', ' ', '\t'])
                    .trim();
                if cards.is_empty() {
                    return None;
                }
                Self::Play(cards.to_string())
            }
        };
        Some(command)
    }

    pub(crate) fn parse_friend(payload: &str) -> Option<Self> {
        (compact_command(payload) == "手牌").then_some(Self::Hand)
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Start => "landlord:start".to_string(),
            Self::RunFastStart => "run_fast:start".to_string(),
            Self::Join => "landlord:join".to_string(),
            Self::Call => "landlord:call".to_string(),
            Self::Rob => "landlord:rob".to_string(),
            Self::Decline => "landlord:decline".to_string(),
            Self::Status => "landlord:status".to_string(),
            Self::Play(cards) => format!("landlord:play:{}", command_identity(cards)),
            Self::Pass => "landlord:pass".to_string(),
            Self::Hand => "landlord:hand".to_string(),
            Self::Retry => "landlord:retry".to_string(),
            Self::Exit => "landlord:exit".to_string(),
        }
    }

    pub(crate) fn requires_executor(&self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::RunFastStart
                | Self::Join
                | Self::Call
                | Self::Rob
                | Self::Decline
                | Self::Hand
                | Self::Retry
        )
    }

    pub(super) fn reports_entertainment_conflict(&self) -> bool {
        matches!(self, Self::Play(_) | Self::Pass | Self::Exit)
    }
}
