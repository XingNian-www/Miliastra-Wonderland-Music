use anyhow::{Result, anyhow};

use crate::config::OutputConfig;
#[cfg(test)]
use crate::features::chat_text::split_numbered_chat_message;
use crate::features::chat_text::{MAX_CHAT_WIDTH, char_width, display_width};
use crate::ui::routines::{
    HallBatchStatus, HallBatchUi, SendHallBatch, UiResidencyOutcome, UiResidencyTarget,
};

const OMIT: &str = "...";
use crate::privacy::redacted_chat_text;
#[cfg(test)]
use crate::privacy::{REDACTED_TURTLE_SOUP_BOTTOM, REDACTED_UNDERCOVER_INPUT};

#[derive(Debug)]
pub(crate) struct ChatBatchSendOutcome {
    pub sent: usize,
    pub status: ChatBatchSendStatus,
}

#[derive(Debug)]
pub(crate) enum ChatBatchSendStatus {
    Complete,
    Failed(anyhow::Error),
}

impl ChatBatchSendOutcome {
    pub(crate) fn complete(sent: usize) -> Self {
        Self {
            sent,
            status: ChatBatchSendStatus::Complete,
        }
    }

    pub(crate) fn failed(sent: usize, error: anyhow::Error) -> Self {
        Self {
            sent,
            status: ChatBatchSendStatus::Failed(error),
        }
    }

    fn into_result(self, expected: usize) -> Result<()> {
        match self.status {
            ChatBatchSendStatus::Complete if self.sent == expected => Ok(()),
            ChatBatchSendStatus::Complete => Err(anyhow!(
                "批量发送提前完成: sent={} expected={}",
                self.sent,
                expected
            )),
            ChatBatchSendStatus::Failed(error) => Err(error),
        }
    }
}

#[derive(Clone)]
pub struct ChatOutput {
    enabled: bool,
    hall_batch_ui: HallBatchUi,
}

impl ChatOutput {
    pub(crate) fn new(config: &OutputConfig, hall_batch_ui: HallBatchUi) -> Self {
        Self {
            enabled: config.send_enabled,
            hall_batch_ui,
        }
    }

    pub fn send(&self, message: &str) -> Result<()> {
        self.send_primary(message, false)
    }

    pub fn send_for_command(&self, message: &str) -> Result<()> {
        self.send_primary(message, true)
    }

    fn send_primary(&self, message: &str, restore_after_task: bool) -> Result<()> {
        let message = fit_chat_message(message);
        log::info!("游戏内回复: {}", redacted_chat_text(&message));
        if !self.enabled {
            log::info!("游戏内回复发送已关闭，仅记录日志");
            return Ok(());
        }
        self.send_with_input(&message, restore_after_task)
    }

    pub fn send_current_chat(&self, message: &str) -> Result<()> {
        let message = fit_chat_message(message);
        log::info!("当前聊天回复: {}", redacted_chat_text(&message));
        if !self.enabled {
            log::info!("当前聊天回复发送已关闭，仅记录日志");
            return Ok(());
        }
        self.send_current_chat_with_input(&message)
    }

    pub fn send_current_chat_batch(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        for message in &messages {
            log::info!("当前聊天回复: {}", redacted_chat_text(message));
        }
        if !self.enabled {
            log::info!("当前聊天回复发送已关闭，仅记录日志");
            return Ok(());
        }
        let expected = messages.len();
        self.send_current_chat_batch_with_input(&messages, delay_ms)
            .into_result(expected)
    }

    pub fn send_batch_for_command(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        self.send_primary_batch(messages, delay_ms, true)
    }

    pub fn send_batch_for_command_redacted(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        log::info!("游戏内批量回复: [谁是卧底内容已隐藏] {}条", messages.len());
        if !self.enabled {
            log::info!("游戏内回复发送已关闭，仅记录脱敏日志");
            return Ok(());
        }
        let expected = messages.len();
        self.send_batch_with_input(&messages, delay_ms, true)
            .into_result(expected)
    }

    pub fn send_current_chat_batch_redacted(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        log::info!(
            "当前聊天批量回复: [谁是卧底内容已隐藏] {}条",
            messages.len()
        );
        if !self.enabled {
            log::info!("当前聊天回复发送已关闭，仅记录脱敏日志");
            return Ok(());
        }
        let expected = messages.len();
        self.send_current_chat_batch_with_input(&messages, delay_ms)
            .into_result(expected)
    }

    fn send_primary_batch(
        &self,
        messages: &[&str],
        delay_ms: u64,
        restore_after_task: bool,
    ) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        for message in &messages {
            log::info!("游戏内回复: {}", redacted_chat_text(message));
        }
        if !self.enabled {
            log::info!("游戏内回复发送已关闭，仅记录日志");
            return Ok(());
        }
        let expected = messages.len();
        self.send_batch_with_input(&messages, delay_ms, restore_after_task)
            .into_result(expected)
    }

    pub(crate) fn send_current_chat_batch_outcome(
        &self,
        messages: &[&str],
        delay_ms: u64,
    ) -> ChatBatchSendOutcome {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        if !self.enabled {
            for message in &messages {
                log::info!("当前聊天回复: {}", redacted_chat_text(message));
            }
            if !messages.is_empty() {
                log::info!("当前聊天回复发送已关闭，仅记录日志");
            }
            return ChatBatchSendOutcome::complete(messages.len());
        }
        let outcome = self.send_current_chat_batch_with_input(&messages, delay_ms);
        for message in messages.iter().take(outcome.sent) {
            log::info!("当前聊天回复: {}", redacted_chat_text(message));
        }
        outcome
    }

    pub(crate) fn send_batch_outcome(
        &self,
        messages: &[&str],
        delay_ms: u64,
    ) -> ChatBatchSendOutcome {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        if !self.enabled {
            for message in &messages {
                log::info!("游戏内回复: {}", redacted_chat_text(message));
            }
            if !messages.is_empty() {
                log::info!("游戏内回复发送已关闭，仅记录日志");
            }
            return ChatBatchSendOutcome::complete(messages.len());
        }
        let outcome = self.send_batch_with_input(&messages, delay_ms, false);
        for message in messages.iter().take(outcome.sent) {
            log::info!("游戏内回复: {}", redacted_chat_text(message));
        }
        outcome
    }

    fn send_with_input(&self, message: &str, restore_after_task: bool) -> Result<()> {
        let messages = [message.to_string()];
        self.send_batch_with_input(&messages, 0, restore_after_task)
            .into_result(messages.len())
    }

    fn send_current_chat_with_input(&self, message: &str) -> Result<()> {
        let messages = [message.to_string()];
        self.send_current_chat_batch_with_input(&messages, 0)
            .into_result(messages.len())
    }

    fn send_current_chat_batch_with_input(
        &self,
        messages: &[String],
        delay_ms: u64,
    ) -> ChatBatchSendOutcome {
        self.send_target_batch(messages, delay_ms, UiResidencyTarget::SecondaryCurrentHall)
    }

    fn send_batch_with_input(
        &self,
        messages: &[String],
        delay_ms: u64,
        _restore_after_task: bool,
    ) -> ChatBatchSendOutcome {
        self.send_target_batch(messages, delay_ms, UiResidencyTarget::Primary)
    }

    fn send_target_batch(
        &self,
        messages: &[String],
        delay_ms: u64,
        residency: UiResidencyTarget,
    ) -> ChatBatchSendOutcome {
        let operation = match self.hall_batch_ui.submit(SendHallBatch::new(
            messages.iter().cloned(),
            residency,
            delay_ms,
        )) {
            Ok(operation) => operation,
            Err(error) => return ChatBatchSendOutcome::failed(0, anyhow!(error)),
        };
        let outcome = match operation.wait() {
            Ok(outcome) => outcome,
            Err(error) => return ChatBatchSendOutcome::failed(0, anyhow!(error)),
        };
        let sent = outcome.sent();
        match outcome.status() {
            HallBatchStatus::Failed(failure) => {
                if let UiResidencyOutcome::Failed(residency) = outcome.residency() {
                    log::error!("大厅批量发送失败后驻留恢复也失败: {residency}");
                }
                ChatBatchSendOutcome::failed(sent, anyhow!(failure.to_string()))
            }
            HallBatchStatus::Complete => match outcome.residency() {
                UiResidencyOutcome::Confirmed(_) => ChatBatchSendOutcome::complete(sent),
                UiResidencyOutcome::Failed(failure) => {
                    ChatBatchSendOutcome::failed(sent, anyhow!(failure.to_string()))
                }
            },
        }
    }
}

pub(crate) fn fit_chat_message(message: &str) -> String {
    let message = message.trim();
    if display_width(message) <= MAX_CHAT_WIDTH {
        return message.to_string();
    }

    if let Some(output) = fit_invite_message(message) {
        return output;
    }
    if let Some(output) = fit_microphone_message(message) {
        return output;
    }
    if let Some(output) = fit_colon_message(message) {
        return output;
    }
    truncate_display_start(message, MAX_CHAT_WIDTH)
}

fn fit_invite_message(message: &str) -> Option<String> {
    let marker = "邀请BOT前往大厅";
    let index = message.find(marker)?;
    Some(fit_parts("", &message[..index], &message[index..]))
}

fn fit_microphone_message(message: &str) -> Option<String> {
    let marker = " 执行了";
    let index = message.find(marker)?;
    if !message.starts_with('@') {
        return None;
    }
    Some(fit_parts("", &message[..index], &message[index..]))
}

fn fit_colon_message(message: &str) -> Option<String> {
    let (prefix, rest) = split_colon_prefix(message)?;
    if let Some((value, suffix)) = split_priority_suffix(rest) {
        return Some(fit_parts(prefix, value, suffix));
    }
    if let Some((value, suffix)) = split_tail_suffix(rest) {
        return Some(fit_parts(prefix, value, suffix));
    }
    Some(fit_parts(prefix, rest, ""))
}

fn split_colon_prefix(message: &str) -> Option<(&str, &str)> {
    if let Some(index) = message.find(": ") {
        let split = index + 2;
        return Some((&message[..split], &message[split..]));
    }
    if let Some(index) = message.find('：') {
        let split = index + '：'.len_utf8();
        return Some((&message[..split], message[split..].trim_start()));
    }
    None
}

fn split_priority_suffix(value: &str) -> Option<(&str, &str)> {
    let index = value
        .char_indices()
        .find(|(_, ch)| *ch == '，' || *ch == ',')
        .map(|(index, _)| index)?;
    let suffix = &value[index..];
    if suffix.contains('@') || suffix.contains("确认") || suffix.contains("跳过") {
        Some((&value[..index], suffix))
    } else {
        None
    }
}

fn split_tail_suffix(value: &str) -> Option<(&str, &str)> {
    for marker in [" (", "（"] {
        if let Some(index) = value.rfind(marker) {
            return Some((&value[..index], &value[index..]));
        }
    }
    None
}

fn fit_parts(prefix: &str, value: &str, suffix: &str) -> String {
    let fixed_width = display_width(prefix) + display_width(suffix);
    if fixed_width >= MAX_CHAT_WIDTH {
        return truncate_display_start(&format!("{}{}", prefix, suffix), MAX_CHAT_WIDTH);
    }
    let value_width = MAX_CHAT_WIDTH - fixed_width;
    let value = abbreviate_middle(value.trim(), value_width);
    format!("{}{}{}", prefix, value, suffix)
}

fn abbreviate_middle(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let omit_width = display_width(OMIT);
    if max_width <= omit_width {
        return take_display_start(value, max_width);
    }
    let remaining = max_width - omit_width;
    let start_width = remaining.div_ceil(2);
    let end_width = remaining - start_width;
    format!(
        "{}{}{}",
        take_display_start(value, start_width),
        OMIT,
        take_display_end(value, end_width)
    )
}

fn truncate_display_start(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        value.to_string()
    } else {
        abbreviate_middle(value, max_width)
    }
}

fn take_display_start(value: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let next_width = char_width(ch);
        if width + next_width > max_width {
            break;
        }
        output.push(ch);
        width += next_width;
    }
    output
}

fn take_display_end(value: &str, max_width: usize) -> String {
    let mut output = Vec::new();
    let mut width = 0;
    for ch in value.chars().rev() {
        let next_width = char_width(ch);
        if width + next_width > max_width {
            break;
        }
        output.push(ch);
        width += next_width;
    }
    output.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_split_preserves_all_content_without_truncation() {
        let source = "这是一个用于验证海龟汤分段发送的较长汤面。".repeat(8);
        let messages = split_numbered_chat_message("汤面", &source);

        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
        let rebuilt = messages
            .iter()
            .map(|message| message.split_once('：').unwrap().1)
            .collect::<String>();
        assert_eq!(rebuilt, source);
        let total = messages.len();
        assert!(
            messages
                .iter()
                .enumerate()
                .all(|(index, message)| message.starts_with(&format!(
                    "汤面{}/{}：",
                    index + 1,
                    total
                )))
        );
    }

    #[test]
    fn numbered_split_counts_ascii_as_half_width() {
        let source = "A".repeat(160);
        let messages = split_numbered_chat_message("汤底", &source);

        assert!(messages.len() >= 3);
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
    }

    #[test]
    fn turtle_soup_bottom_is_redacted_only_in_logs() {
        assert_eq!(
            redacted_chat_text("汤底1/2：秘密"),
            REDACTED_TURTLE_SOUP_BOTTOM
        );
        assert_eq!(
            redacted_chat_text("汤 底1/2：秘密"),
            REDACTED_TURTLE_SOUP_BOTTOM
        );
        assert_eq!(redacted_chat_text("汤面1/2：线索"), "汤面1/2：线索");
    }

    #[test]
    fn undercover_word_delivery_is_redacted_only_in_logs() {
        assert_eq!(
            redacted_chat_text("你的位置：A；你的词语：苹果"),
            "[谁是卧底秘密内容已隐藏]"
        );
        assert_eq!(
            redacted_chat_text("[玩家]：#一种常见的水果"),
            "[玩家]：#一种常见的水果"
        );
        assert_eq!(
            redacted_chat_text("[玩家]：#投 C"),
            REDACTED_UNDERCOVER_INPUT
        );
        assert_eq!(redacted_chat_text("[玩家]：＃c"), REDACTED_UNDERCOVER_INPUT);
        assert_eq!(
            redacted_chat_text("请存活玩家好友私聊 #A"),
            "请存活玩家好友私聊 #A"
        );
    }
}
