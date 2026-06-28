use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use super::config::{OutputConfig, WindowConfig};
use super::notification;
use super::window::GameWindow;

const MAX_CHAT_WIDTH: usize = 80;
const OMIT: &str = "...";

#[derive(Clone, Debug)]
pub struct ChatOutput {
    enabled: bool,
    notify: bool,
    config: OutputConfig,
    window: WindowConfig,
}

impl ChatOutput {
    pub fn new(config: &OutputConfig, window: &WindowConfig) -> Self {
        Self {
            enabled: config.send_enabled,
            notify: config.notify,
            config: config.clone(),
            window: window.clone(),
        }
    }

    pub fn send(&self, message: &str) -> Result<()> {
        let message = fit_chat_message(message);
        log::info!("游戏内回复: {}", message);
        if self.notify {
            let _ = notification::send_windows_notification("点歌命令待处理", &message);
        }
        if !self.enabled {
            log::info!("游戏内回复发送已关闭，仅记录日志");
            return Ok(());
        }
        self.send_with_input(&message)
    }

    pub fn send_batch(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        for message in &messages {
            log::info!("游戏内回复: {}", message);
            if self.notify {
                let _ = notification::send_windows_notification("点歌命令待处理", message);
            }
        }
        if !self.enabled {
            log::info!("游戏内回复发送已关闭，仅记录日志");
            return Ok(());
        }
        self.send_batch_with_input(&messages, delay_ms)
    }

    fn send_with_input(&self, message: &str) -> Result<()> {
        self.send_batch_with_input(&[message.to_string()], 0)
    }

    fn send_batch_with_input(&self, messages: &[String], delay_ms: u64) -> Result<()> {
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        let mut window = GameWindow::find(&self.window)?;
        window.focus_for_keyboard(&mut enigo)?;
        sleep_ms(self.config.focus_delay_ms);

        window.click(&mut enigo, self.config.focus_point)?;
        sleep_ms(self.config.focus_delay_ms);
        enigo
            .key(Key::Return, Direction::Click)
            .context("open chat")?;
        sleep_ms(self.config.open_chat_delay_ms);

        for (index, message) in messages.iter().enumerate() {
            if index > 0 && delay_ms > 0 {
                sleep_ms(delay_ms);
            }
            window.click(&mut enigo, self.config.chat_click_1)?;
            sleep_ms(self.config.click_delay_ms);
            window.click(&mut enigo, self.config.chat_click_2)?;
            sleep_ms(self.config.open_chat_delay_ms);

            enigo.text(message).context("input message text")?;
            sleep_ms(self.config.input_delay_ms);
            enigo
                .key(Key::Return, Direction::Click)
                .context("send message")?;
            sleep_ms(self.config.send_delay_ms);
        }
        window.click(&mut enigo, self.config.focus_point)?;
        Ok(())
    }
}

fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
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

fn display_width(value: &str) -> usize {
    value.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    if ch.is_ascii() {
        1
    } else {
        2
    }
}
