use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use super::FrameArgs;
use super::config::{
    InviteConfig, OutputConfig, ScreenConfig, TemplateConfig, TimingConfig, WindowConfig,
};
use super::frame_source::Canvas;
use super::geometry::Rect;
use super::input_actions;
use super::ui_locator::UiLocator;
use super::window::GameWindow;
use super::workflow_actions::{self, ScrollTemplateOptions};

pub(super) const MAX_CHAT_WIDTH: usize = 80;
const OMIT: &str = "...";
const REDACTED_TURTLE_SOUP_BOTTOM: &str = "[海龟汤汤底已隐藏]";
const REDACTED_UNDERCOVER_SECRET: &str = "[谁是卧底秘密内容已隐藏]";
const REDACTED_UNDERCOVER_INPUT: &str = "[谁是卧底私聊内容已隐藏]";

#[derive(Debug)]
pub(super) struct ChatBatchSendOutcome {
    pub sent: usize,
    pub status: ChatBatchSendStatus,
}

#[derive(Debug)]
pub(super) enum ChatBatchSendStatus {
    Complete,
    Interrupted,
    Failed(anyhow::Error),
}

impl ChatBatchSendOutcome {
    fn complete(sent: usize) -> Self {
        Self {
            sent,
            status: ChatBatchSendStatus::Complete,
        }
    }

    fn interrupted(sent: usize) -> Self {
        Self {
            sent,
            status: ChatBatchSendStatus::Interrupted,
        }
    }

    pub(super) fn failed(sent: usize, error: anyhow::Error) -> Self {
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
            ChatBatchSendStatus::Interrupted => Err(anyhow!(
                "不可中断的批量发送意外让行: sent={} expected={}",
                self.sent,
                expected
            )),
            ChatBatchSendStatus::Failed(error) => Err(error),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatOutput {
    enabled: bool,
    config: OutputConfig,
    timing: TimingConfig,
    window: WindowConfig,
    canvas: Canvas,
    secondary_hall_template: PathBuf,
    secondary_hall_search_region: Rect,
    friend_list_region: Rect,
    template_threshold: f32,
}

impl ChatOutput {
    pub fn new(
        config: &OutputConfig,
        timing: &TimingConfig,
        window: &WindowConfig,
        screen: &ScreenConfig,
        templates: &TemplateConfig,
        invite: &InviteConfig,
    ) -> Self {
        let hall_anchor: Rect = screen.secondary_hall_rect.into();
        let friend_list_region: Rect = invite.friend_list_region.into();
        Self {
            enabled: config.send_enabled,
            config: config.clone(),
            timing: timing.clone(),
            window: window.clone(),
            canvas: Canvas {
                width: screen.expected_width,
                height: screen.expected_height,
                resize: true,
            },
            secondary_hall_template: templates.secondary_hall.clone(),
            secondary_hall_search_region: bounding_rect(hall_anchor, friend_list_region),
            friend_list_region,
            template_threshold: templates.marker_threshold,
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
        self.send_current_chat_batch_interruptible_with_input(&messages, delay_ms, || true)
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
        self.send_batch_interruptible_with_input(&messages, delay_ms, true, || true)
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
        self.send_current_chat_batch_interruptible_with_input(&messages, delay_ms, || true)
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
        self.send_batch_interruptible_with_input(&messages, delay_ms, restore_after_task, || true)
            .into_result(expected)
    }

    pub fn send_current_chat_batch_interruptible(
        &self,
        messages: &[&str],
        delay_ms: u64,
        should_continue: impl FnMut() -> bool,
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
        let outcome = self.send_current_chat_batch_interruptible_with_input(
            &messages,
            delay_ms,
            should_continue,
        );
        for message in messages.iter().take(outcome.sent) {
            log::info!("当前聊天回复: {}", redacted_chat_text(message));
        }
        outcome
    }

    pub fn send_batch_interruptible(
        &self,
        messages: &[&str],
        delay_ms: u64,
        should_continue: impl FnMut() -> bool,
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
        let outcome =
            self.send_batch_interruptible_with_input(&messages, delay_ms, false, should_continue);
        for message in messages.iter().take(outcome.sent) {
            log::info!("游戏内回复: {}", redacted_chat_text(message));
        }
        outcome
    }

    fn send_with_input(&self, message: &str, restore_after_task: bool) -> Result<()> {
        let messages = [message.to_string()];
        self.send_batch_interruptible_with_input(&messages, 0, restore_after_task, || true)
            .into_result(messages.len())
    }

    fn send_current_chat_with_input(&self, message: &str) -> Result<()> {
        let messages = [message.to_string()];
        self.send_current_chat_batch_interruptible_with_input(&messages, 0, || true)
            .into_result(messages.len())
    }

    fn send_current_chat_batch_interruptible_with_input(
        &self,
        messages: &[String],
        delay_ms: u64,
        mut should_continue: impl FnMut() -> bool,
    ) -> ChatBatchSendOutcome {
        if messages.is_empty() {
            return ChatBatchSendOutcome::complete(0);
        }
        if !should_continue() {
            return ChatBatchSendOutcome::interrupted(0);
        }

        let mut enigo = match Enigo::new(&Settings::default()).context("create enigo") {
            Ok(enigo) => enigo,
            Err(error) => return ChatBatchSendOutcome::failed(0, error),
        };
        let mut window = match GameWindow::find(&self.window) {
            Ok(window) => window,
            Err(error) => return ChatBatchSendOutcome::failed(0, error),
        };
        if let Err(error) = window.ensure_foreground() {
            return ChatBatchSendOutcome::failed(0, error);
        }

        send_messages_interruptibly(messages, delay_ms, should_continue, |message| {
            (|| -> Result<()> {
                window.click(&mut enigo, self.config.chat_click_2)?;
                sleep_ms(self.timing.input.click_ms);
                input_message(&mut enigo, message, self.timing.input.text_ms, &self.window)?;
                enigo
                    .key(Key::Return, Direction::Click)
                    .context("send message")?;
                sleep_ms(self.timing.input.send_ms);
                Ok(())
            })()
        })
    }

    fn send_batch_interruptible_with_input(
        &self,
        messages: &[String],
        delay_ms: u64,
        restore_after_task: bool,
        mut should_continue: impl FnMut() -> bool,
    ) -> ChatBatchSendOutcome {
        if messages.is_empty() {
            return ChatBatchSendOutcome::complete(0);
        }
        if !should_continue() {
            return ChatBatchSendOutcome::interrupted(0);
        }

        let mut enigo = match Enigo::new(&Settings::default()).context("create enigo") {
            Ok(enigo) => enigo,
            Err(error) => return ChatBatchSendOutcome::failed(0, error),
        };
        let mut window = match GameWindow::find(&self.window) {
            Ok(window) => window,
            Err(error) => return ChatBatchSendOutcome::failed(0, error),
        };
        if let Err(error) = window.ensure_foreground() {
            return ChatBatchSendOutcome::failed(0, error);
        }
        if let Err(error) = enigo
            .key(Key::Return, Direction::Click)
            .context("open chat")
        {
            return ChatBatchSendOutcome::failed(0, error);
        }
        sleep_ms(self.timing.input.open_chat_ms);

        let locator = UiLocator::new(
            self.canvas.clone(),
            FrameArgs { image: None },
            self.window.clone(),
            self.timing.workflow.default_poll_ms,
        );
        let hall_hit = workflow_actions::click_scrollable_template(
            &locator,
            &self.secondary_hall_template,
            self.secondary_hall_search_region,
            self.friend_list_region,
            self.template_threshold,
            ScrollTemplateOptions {
                max_scrolls: 3,
                scroll_length: -8,
                settle_ms: self.timing.input.click_ms,
            },
            &mut should_continue,
        );
        match hall_hit {
            Ok(Some(_)) => sleep_ms(self.timing.input.click_ms),
            Ok(None) => {
                return ChatBatchSendOutcome::failed(0, anyhow!("发送前未找到当前大厅模板"));
            }
            Err(error) => return ChatBatchSendOutcome::failed(0, error),
        }

        let outcome = send_messages_interruptibly(messages, delay_ms, should_continue, |message| {
            (|| -> Result<()> {
                window.click(&mut enigo, self.config.chat_click_2)?;
                sleep_ms(self.timing.input.open_chat_ms);
                input_message(&mut enigo, message, self.timing.input.text_ms, &self.window)?;
                enigo
                    .key(Key::Return, Direction::Click)
                    .context("send message")?;
                sleep_ms(self.timing.input.send_ms);
                Ok(())
            })()
        });
        if !primary_chat_should_close_directly(restore_after_task) {
            return outcome;
        }
        let ChatBatchSendOutcome { sent, status } = outcome;
        match status {
            ChatBatchSendStatus::Complete => match self.close_batch_chat(&mut enigo, &mut window) {
                Ok(()) => ChatBatchSendOutcome::complete(sent),
                Err(error) => ChatBatchSendOutcome::failed(sent, error),
            },
            ChatBatchSendStatus::Interrupted => {
                match self.close_batch_chat(&mut enigo, &mut window) {
                    Ok(()) => ChatBatchSendOutcome::interrupted(sent),
                    Err(error) => ChatBatchSendOutcome::failed(sent, error),
                }
            }
            ChatBatchSendStatus::Failed(error) => {
                if let Err(close_error) = self.close_batch_chat(&mut enigo, &mut window) {
                    log::error!("批量回复失败后关闭聊天界面也失败: {close_error:#}");
                }
                ChatBatchSendOutcome::failed(sent, error)
            }
        }
    }

    fn close_batch_chat(&self, enigo: &mut Enigo, window: &mut GameWindow) -> Result<()> {
        window.ensure_foreground()?;
        enigo
            .key(Key::Escape, Direction::Click)
            .context("close chat")?;
        sleep_ms(self.timing.input.click_ms);
        Ok(())
    }
}

fn bounding_rect(left: Rect, right: Rect) -> Rect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let far_right = left.right().max(right.right());
    let bottom = left.bottom().max(right.bottom());
    Rect::new(x, y, (far_right - x) as u32, (bottom - y) as u32)
}

fn primary_chat_should_close_directly(restore_after_task: bool) -> bool {
    !restore_after_task
}

fn input_message(
    enigo: &mut Enigo,
    message: &str,
    input_settle_ms: u64,
    window: &WindowConfig,
) -> Result<()> {
    if let Err(error) = input_actions::paste_text(message, window, input_settle_ms) {
        log::error!("粘贴输入失败，回退到文字输入: {error:#}");
        enigo.text(message).context("input message text")?;
        sleep_ms(input_settle_ms);
    }
    Ok(())
}

fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
}

fn send_messages_interruptibly<T>(
    messages: &[T],
    delay_ms: u64,
    mut should_continue: impl FnMut() -> bool,
    mut send_one: impl FnMut(&T) -> Result<()>,
) -> ChatBatchSendOutcome {
    let mut sent = 0;
    for (index, message) in messages.iter().enumerate() {
        if !should_continue() {
            return ChatBatchSendOutcome::interrupted(sent);
        }
        if index > 0 && delay_ms > 0 {
            sleep_ms(delay_ms);
            if !should_continue() {
                return ChatBatchSendOutcome::interrupted(sent);
            }
        }
        if let Err(error) = send_one(message) {
            return ChatBatchSendOutcome::failed(sent, error);
        }
        sent += 1;
    }
    ChatBatchSendOutcome::complete(sent)
}

pub(super) fn redacted_chat_text(message: &str) -> &str {
    if contains_turtle_soup_bottom_marker(message) {
        REDACTED_TURTLE_SOUP_BOTTOM
    } else if message.contains("你的位置：") && message.contains("你的词语：") {
        REDACTED_UNDERCOVER_SECRET
    } else if contains_undercover_private_input(message) {
        REDACTED_UNDERCOVER_INPUT
    } else {
        message
    }
}

fn contains_undercover_private_input(message: &str) -> bool {
    let body = message
        .find(['：', ':', ']', '】'])
        .map_or(message, |index| {
            &message[index + message[index..].chars().next().map_or(0, char::len_utf8)..]
        })
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    let Some(command) = body.strip_prefix('@') else {
        return false;
    };
    let command = command.trim_start();
    command.starts_with("描述") || command.starts_with('投')
}

fn contains_turtle_soup_bottom_marker(message: &str) -> bool {
    let mut saw_soup = false;
    for ch in message.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if saw_soup && ch == '底' {
            return true;
        }
        saw_soup = ch == '汤';
    }
    false
}

fn fit_chat_message(message: &str) -> String {
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

pub(super) fn split_numbered_chat_message(label: &str, message: &str) -> Vec<String> {
    let source = normalize_segment_source(message);
    let mut expected_total = 1usize;
    for _ in 0..16 {
        let messages = split_numbered_with_total(label, &source, expected_total);
        if messages.len() == expected_total {
            return messages;
        }
        expected_total = messages.len().max(1);
    }
    split_numbered_with_total(label, &source, expected_total)
}

fn split_numbered_with_total(label: &str, source: &str, total: usize) -> Vec<String> {
    let chars = source.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return vec![format!("{}1/1：", label)];
    }

    let mut messages = Vec::new();
    let mut offset = 0usize;
    while offset < chars.len() {
        let index = messages.len() + 1;
        let prefix = format!("{}{}/{}：", label, index, total.max(1));
        let available = MAX_CHAT_WIDTH.saturating_sub(display_width(&prefix));
        let mut chunk = String::new();
        let mut width = 0usize;
        while offset < chars.len() {
            let next_width = char_width(chars[offset]);
            if !chunk.is_empty() && width + next_width > available {
                break;
            }
            if chunk.is_empty() && next_width > available {
                break;
            }
            chunk.push(chars[offset]);
            width += next_width;
            offset += 1;
        }
        if chunk.is_empty() {
            chunk.push(chars[offset]);
            offset += 1;
        }
        messages.push(format!("{}{}", prefix, chunk));
    }
    messages
}

fn normalize_segment_source(message: &str) -> String {
    let mut output = String::new();
    let mut previous_was_line_break = false;
    for ch in message.trim().chars() {
        match ch {
            '\r' => {}
            '\n' => {
                if !previous_was_line_break && !output.ends_with(' ') {
                    output.push(' ');
                }
                previous_was_line_break = true;
            }
            _ => {
                output.push(ch);
                previous_was_line_break = false;
            }
        }
    }
    output
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

pub(super) fn display_width(value: &str) -> usize {
    value.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    if ch.is_ascii() { 1 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_command_reply_leaves_chat_open_for_residency_restore() {
        assert!(!primary_chat_should_close_directly(true));
        assert!(primary_chat_should_close_directly(false));
    }

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
            redacted_chat_text("A、B描述已记录（2/4）"),
            "A、B描述已记录（2/4）"
        );
        assert_eq!(
            redacted_chat_text("[玩家]：@描述 一种常见的水果"),
            REDACTED_UNDERCOVER_INPUT
        );
        assert_eq!(
            redacted_chat_text("[玩家]：@投 C"),
            REDACTED_UNDERCOVER_INPUT
        );
        assert_eq!(
            redacted_chat_text("请存活玩家好友私聊 @投 A"),
            "请存活玩家好友私聊 @投 A"
        );
    }

    #[test]
    fn interruptible_batch_yields_before_the_next_message() {
        let messages = ["第一段", "第二段", "第三段"];
        let mut checks = 0;
        let mut delivered = Vec::new();

        let outcome = send_messages_interruptibly(
            &messages,
            0,
            || {
                checks += 1;
                checks == 1
            },
            |message| {
                delivered.push(*message);
                Ok(())
            },
        );

        assert_eq!(outcome.sent, 1);
        assert!(matches!(outcome.status, ChatBatchSendStatus::Interrupted));
        assert_eq!(delivered, vec!["第一段"]);
    }

    #[test]
    fn interruptible_batch_reports_partial_success_before_failure() {
        let messages = ["第一段", "第二段", "第三段"];
        let mut delivered = Vec::new();

        let outcome = send_messages_interruptibly(
            &messages,
            0,
            || true,
            |message| {
                if *message == "第二段" {
                    return Err(anyhow::anyhow!("模拟发送失败"));
                }
                delivered.push(*message);
                Ok(())
            },
        );

        assert_eq!(outcome.sent, 1);
        assert!(matches!(outcome.status, ChatBatchSendStatus::Failed(_)));
        assert_eq!(delivered, vec!["第一段"]);
    }
}
