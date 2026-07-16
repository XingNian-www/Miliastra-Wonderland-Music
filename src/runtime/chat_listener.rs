use serde::{Deserialize, Serialize};

use crate::features::chat_text::{CommandSyntax, parse_prefixed_command};

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

    pub(crate) fn parse(text: &str) -> Option<CommandSyntax<'_, Self>> {
        let argument = parse_prefixed_command(text, "监听模式", true)?;
        let command = match argument {
            "一级" => Self::Primary,
            "二级" => Self::Secondary,
            "状态" => Self::Status,
            _ => return None,
        };
        Some(CommandSyntax {
            matched: "监听模式",
            argument,
            command,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChatListenerMode {
    Primary,
    Secondary,
}

impl ChatListenerMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Primary => "一级监听",
            Self::Secondary => "二级监听",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatListenerSnapshot {
    pub(crate) mode: ChatListenerMode,
    pub(crate) pending_mode: Option<ChatListenerMode>,
    pub(crate) temporary_primary: bool,
    pub(crate) initial_unread_clear: bool,
    pub(crate) unread_task_pending: bool,
    pub(crate) hall_round_required: bool,
}

impl ChatListenerSnapshot {
    pub(crate) fn display_mode(&self) -> String {
        if self.temporary_primary {
            format!("{}（临时一级阶段）", self.mode.label())
        } else {
            self.mode.label().to_string()
        }
    }
}

#[derive(Debug)]
pub(crate) struct ChatListenerState {
    mode: ChatListenerMode,
    pending_mode: Option<ChatListenerMode>,
    temporary_primary_holds: usize,
    initial_unread_clear: bool,
    unread_task_pending: bool,
    hall_round_required: bool,
}

impl ChatListenerState {
    pub(crate) const fn new() -> Self {
        Self {
            mode: ChatListenerMode::Primary,
            pending_mode: None,
            temporary_primary_holds: 0,
            initial_unread_clear: false,
            unread_task_pending: false,
            hall_round_required: false,
        }
    }

    pub(crate) fn snapshot(&self) -> ChatListenerSnapshot {
        ChatListenerSnapshot {
            mode: self.mode,
            pending_mode: self.pending_mode,
            temporary_primary: self.temporary_primary_holds > 0,
            initial_unread_clear: self.initial_unread_clear,
            unread_task_pending: self.unread_task_pending,
            hall_round_required: self.hall_round_required,
        }
    }

    pub(crate) fn request_mode(&mut self, target: ChatListenerMode) -> bool {
        if self.pending_mode.is_some() || self.mode == target {
            return false;
        }
        self.pending_mode = Some(target);
        true
    }

    pub(crate) fn complete_mode_switch(&mut self, mode: ChatListenerMode) {
        self.mode = mode;
        self.pending_mode = None;
        self.temporary_primary_holds = 0;
        self.initial_unread_clear = mode == ChatListenerMode::Secondary;
        self.unread_task_pending = false;
        self.hall_round_required = false;
    }

    pub(crate) fn cancel_mode_request(&mut self, target: ChatListenerMode) {
        if self.pending_mode == Some(target) {
            self.pending_mode = None;
        }
    }

    pub(crate) fn fail_mode_switch_to_primary(&mut self) {
        self.complete_mode_switch(ChatListenerMode::Primary);
    }

    pub(crate) fn begin_temporary_primary(&mut self) {
        self.temporary_primary_holds = self.temporary_primary_holds.saturating_add(1);
    }

    pub(crate) fn end_temporary_primary(&mut self) {
        self.temporary_primary_holds = self.temporary_primary_holds.saturating_sub(1);
    }

    pub(crate) fn claim_unread_task(&mut self) -> bool {
        if self.mode != ChatListenerMode::Secondary || self.unread_task_pending {
            return false;
        }
        self.unread_task_pending = true;
        true
    }

    pub(crate) fn finish_unread_task(&mut self, processed_message: bool) {
        self.unread_task_pending = false;
        if !self.initial_unread_clear && processed_message {
            self.hall_round_required = true;
        }
    }

    pub(crate) fn release_unread_task(&mut self) {
        self.unread_task_pending = false;
    }

    pub(crate) fn finish_initial_unread_clear(&mut self) {
        if self.mode == ChatListenerMode::Secondary && !self.unread_task_pending {
            self.initial_unread_clear = false;
            self.hall_round_required = true;
        }
    }

    pub(crate) fn finish_hall_round(&mut self) {
        self.hall_round_required = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_temporary_primary_holds_release_independently() {
        let mut state = ChatListenerState::new();
        state.begin_temporary_primary();
        state.begin_temporary_primary();
        state.end_temporary_primary();
        assert!(state.snapshot().temporary_primary);
        state.end_temporary_primary();
        assert!(!state.snapshot().temporary_primary);
    }
}
