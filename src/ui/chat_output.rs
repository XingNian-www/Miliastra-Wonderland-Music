use anyhow::{Result, anyhow};
use std::sync::{Arc, RwLock};

#[cfg(test)]
use crate::text::split_numbered_chat_message;
use crate::text::{MAX_CHAT_WIDTH, char_width, display_width};
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

/// 一次批量回复的发送目标。日志文案只依赖它，三条发送路径因此共用同一套措辞。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatSendTarget {
    /// 一级大厅。
    PrimaryHall,
    /// 二级聊天里的「当前聊天」。
    CurrentChat,
}

impl ChatSendTarget {
    fn label(self) -> &'static str {
        match self {
            Self::PrimaryHall => "一级大厅",
            Self::CurrentChat => "当前聊天",
        }
    }

}

/// 发送前的提交记录：**只写数量，不写正文**。
///
/// 正文可能含歌名、玩家昵称或玩法秘密；数量足够定位「这批到底提交了几条」。
/// 做成纯函数是为了让测试能直接断言条数与措辞，不必安装全局 logger。
fn submission_log_line(target: ChatSendTarget, total: usize) -> Option<(log::Level, String)> {
    if total == 0 {
        return None;
    }
    Some((
        log::Level::Debug,
        format!("向{}提交 {} 条回复", target.label(), total),
    ))
}

/// 发送后的结果摘要。
///
/// 关键点：**成功数与提交总数一起写**。旧实现只遍历 `outcome.sent`，
/// 提交 5 条成功 3 条时日志里只有 3 行，读起来像「只准备了 3 条」，
/// 完全看不出还有 2 条没送达。
struct ChatSendReport<'a> {
    target: ChatSendTarget,
    total: usize,
    sent: usize,
    error: Option<&'a anyhow::Error>,
}

impl<'a> ChatSendReport<'a> {
    fn new(
        target: ChatSendTarget,
        outcome: &'a ChatBatchSendOutcome,
        total: usize,
    ) -> Self {
        Self {
            target,
            total,
            sent: outcome.sent,
            error: match &outcome.status {
                ChatBatchSendStatus::Complete => None,
                ChatBatchSendStatus::Failed(error) => Some(error),
            },
        }
    }

    /// 结果日志行：`(级别, 文案)`。纯函数，测试直接断言。
    ///
    /// 分级口径：
    /// - 全部送达 → `info`（玩家能感知的结果）
    /// - 部分送达 → `info` 说清 N/M，再补 `warn` 说明有几条玩家看不到
    /// - 一条都没送达 → `error`（本篇功能不可用）
    fn lines(&self) -> Vec<(log::Level, String)> {
        if self.total == 0 {
            return Vec::new();
        }
        let label = self.target.label();
        let failure = self
            .error
            .map(|error| format!("：{error}"))
            .unwrap_or_default();
        if self.sent >= self.total {
            let mut lines = vec![(
                log::Level::Info,
                format!("已在{label}发送 {}/{} 条回复", self.sent, self.total),
            )];
            // 正文都已发出，但发送后恢复界面驻留失败：不能谎称整批没问题。
            if self.error.is_some() {
                lines.push((
                    log::Level::Warn,
                    format!(
                        "{label}的 {} 条回复已全部送达，但发送后未能恢复到原界面，当前界面位置未知，后续截图与点击可能识别错界面{failure}",
                        self.sent
                    ),
                ));
            }
            return lines;
        }
        if self.sent == 0 {
            return vec![(
                log::Level::Error,
                format!(
                    "在{label}发送 {}/{} 条回复全部失败，这批回复玩家看不到{failure}",
                    self.sent, self.total
                ),
            )];
        }
        vec![
            (
                log::Level::Info,
                format!("已在{label}发送 {}/{} 条回复", self.sent, self.total),
            ),
            (
                log::Level::Warn,
                format!(
                    "{label}有 {} 条回复未送达，玩家看不到这几条{failure}",
                    self.total - self.sent
                ),
            ),
        ]
    }

    fn emit(&self) {
        for (level, text) in self.lines() {
            log::log!(level, "{text}");
        }
    }
}

/// 回复捕获守卫：drop 时结束捕获并取出结果。
pub(crate) struct ReplyCapture {
    slot: Arc<RwLock<Option<Vec<String>>>>,
}

impl ReplyCapture {
    /// 结束捕获，返回期间发出的全部回复文本。
    pub(crate) fn finish(self) -> Vec<String> {
        let _ = &self.slot;
        std::mem::take(
            &mut *self
                .slot
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .unwrap_or_default()
    }
}

impl Drop for ReplyCapture {
    fn drop(&mut self) {
        // 未显式 finish 时也要关掉，避免捕获槽长期打开。
        let _ = self
            .slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

#[derive(Clone)]
pub struct ChatOutput {
    enabled: Arc<RwLock<bool>>,
    hall_batch_ui: HallBatchUi,
    /// 命令执行期间的回复捕获槽。
    ///
    /// AI 代执行命令时，把 BOT 实际发出的文本记下来回传给模型——
    /// 否则它只知道「命令已执行完成」，无法判断点歌到底成没成
    /// （歌名不存在时业务会回「没找到」，但回执原来一模一样）。
    capture: Arc<RwLock<Option<Vec<String>>>>,
}

impl ChatOutput {
    pub(crate) fn with_live_enabled(
        enabled: Arc<RwLock<bool>>,
        hall_batch_ui: HallBatchUi,
    ) -> Self {
        Self {
            enabled,
            hall_batch_ui,
            capture: Arc::new(RwLock::new(None)),
        }
    }

    /// 开始捕获回复，返回一个守卫；守卫 drop 时结束捕获。
    ///
    /// 用守卫而不是成对的 start/stop：命令执行中途出错提前返回时，
    /// 成对调用很容易漏掉 stop，捕获槽会一直开着把无关回复也记进去。
    pub(crate) fn capture_replies(&self) -> ReplyCapture {
        *self
            .capture
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
        ReplyCapture {
            slot: self.capture.clone(),
        }
    }

    /// 捕获通道打开时把这条回复记下来。
    fn record_captured(&self, message: &str) {
        let mut guard = self
            .capture
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(replies) = guard.as_mut() {
            replies.push(message.to_string());
        }
    }

    /// 结束捕获并取回期间发出的全部回复。
    fn take_captured(&self) -> Vec<String> {
        self.capture
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default()
    }

    fn enabled(&self) -> bool {
        *self
            .enabled
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn send(&self, message: &str) -> Result<()> {
        self.send_primary(message, false)
    }

    pub(crate) fn send_invite_ready(
        &self,
        message: &str,
        residency: UiResidencyTarget,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let message = fit_chat_message(message);
        if !self.enabled() {
            log::info!("游戏内回复发送已关闭，邀请就绪通知不发送，邀请流程继续");
            return Ok(());
        }
        let request = SendHallBatch::new([message.clone()], residency, 0)
            .after_invite_primary_stability(running);
        let outcome = self.send_batch_request(request);
        // 邀请就绪通知走的是自定义驻留跟随，不是三条标准发送路径，
        // 但结果记账沿用同一套措辞，避免同一件事出现两种日志形状。
        ChatSendReport::new(ChatSendTarget::PrimaryHall, &outcome, 1).emit();
        outcome.into_result(1)
    }

    pub fn send_for_command(&self, message: &str) -> Result<()> {
        self.send_primary(message, true)
    }

    fn send_primary(&self, message: &str, restore_after_task: bool) -> Result<()> {
        let message = fit_chat_message(message);
        self.record_captured(&message);
        let messages = [message];
        self.send_batch_logged(
            ChatSendTarget::PrimaryHall,
            &messages,
            0,
            restore_after_task,
        )
        .into_result(1)
    }

    pub fn send_current_chat(&self, message: &str) -> Result<()> {
        let message = fit_chat_message(message);
        self.record_captured(&message);
        let messages = [message];
        self.send_batch_logged(ChatSendTarget::CurrentChat, &messages, 0, false)
            .into_result(1)
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
            self.record_captured(message);
        }
        let total = messages.len();
        self.send_batch_logged(ChatSendTarget::CurrentChat, &messages, delay_ms, false)
            .into_result(total)
    }

    pub fn send_batch_for_command(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        self.send_primary_batch(messages, delay_ms, true)
    }

    pub fn send_batch_for_command_redacted(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        let total = messages.len();
        self.send_batch_logged(ChatSendTarget::PrimaryHall, &messages, delay_ms, true)
            .into_result(total)
    }

    pub fn send_current_chat_batch_redacted(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        let messages = messages
            .iter()
            .map(|message| fit_chat_message(message))
            .collect::<Vec<_>>();
        let total = messages.len();
        self.send_batch_logged(ChatSendTarget::CurrentChat, &messages, delay_ms, false)
            .into_result(total)
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
        let total = messages.len();
        self.send_batch_logged(ChatSendTarget::PrimaryHall, &messages, delay_ms, restore_after_task)
            .into_result(total)
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
        self.send_batch_logged(ChatSendTarget::CurrentChat, &messages, delay_ms, false)
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
        self.send_batch_logged(ChatSendTarget::PrimaryHall, &messages, delay_ms, false)
    }

    /// 全部发送路径唯一的共同实现：**先记提交数量，再发，再按结果记账**。
    ///
    /// 旧实现里三条路径各自编排日志：`send_primary_batch` 发送**前**逐条打 info，
    /// 另两条发送**后**只遍历 `outcome.sent`。于是同一次「提交 5 条」在两种路径上
    /// 一是「发了 5 行」、一是「发了 3 行」，后者读起来像只准备了 3 条，
    /// 完全看不出还有 2 条没送达。集中到这里之后，一级大厅与当前聊天的
    /// 措辞、级别与时序完全一致。
    fn send_batch_logged(
        &self,
        target: ChatSendTarget,
        messages: &[String],
        delay_ms: u64,
        restore_after_task: bool,
    ) -> ChatBatchSendOutcome {
        let total = messages.len();
        // 关闭时仍要走一遍「提交 N 条 + 结果」的记录，避免日志里凭空少一段；
        // 但结果按「全部就绪、未实际发送」处理——玩家看不到、也不算失败。
        if !self.enabled() {
            if let Some((level, text)) = submission_log_line(target, total) {
                log::log!(level, "{text}");
            }
            if total > 0 {
                log::info!(
                    "游戏内回复发送已关闭，{}的 {} 条回复只记录日志、不发送",
                    target.label(),
                    total
                );
            }
            return ChatBatchSendOutcome::complete(total);
        }
        if let Some((level, text)) = submission_log_line(target, total) {
            log::log!(level, "{text}");
        }
        // 正文只留在 trace：需要逐条核对发的是什么时才开。
        // 脱敏不能省——歌名、昵称、玩法秘密都可能出现在正文里。
        for message in messages {
            log::trace!(
                "{}待发回复：{}",
                target.label(),
                redacted_chat_text(message)
            );
        }
        let outcome = match target {
            ChatSendTarget::PrimaryHall => {
                self.send_batch_with_input(messages, delay_ms, restore_after_task)
            }
            ChatSendTarget::CurrentChat => {
                self.send_current_chat_batch_with_input(messages, delay_ms)
            }
        };
        ChatSendReport::new(target, &outcome, total).emit();
        outcome
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
        self.send_batch_request(SendHallBatch::new(
            messages.iter().cloned(),
            residency,
            delay_ms,
        ))
    }

    fn send_batch_request(&self, request: SendHallBatch) -> ChatBatchSendOutcome {
        let operation = match self.hall_batch_ui.submit(request) {
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
                    log::error!(
                        "大厅批量回复发送失败，之后恢复界面驻留也失败（步骤：发送后回到原界面），当前界面位置未知，后续截图与点击可能识别错界面：{residency}"
                    );
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

    /// 捕获器只记「守卫存活期间」的回复，finish 之后自动关闭。
    ///
    /// 直接测捕获槽本身：`ChatOutput` 的完整构造需要 UI 句柄，
    /// 而这里要验证的语义与发送通道无关。
    #[test]
    fn reply_capture_records_only_while_open() {
        let slot: Arc<RwLock<Option<Vec<String>>>> = Arc::new(RwLock::new(None));
        let capture = ReplyCapture { slot: slot.clone() };

        // 打开之前：没有槽，写入被忽略。
        *slot.write().unwrap() = None;

        // 模拟捕获期间的回复。
        *slot.write().unwrap() = Some(vec!["已加入队列".to_string(), "第二段".to_string()]);
        let replies = capture.finish();
        assert_eq!(
            replies,
            vec!["已加入队列".to_string(), "第二段".to_string()]
        );
        // finish 之后槽已清空，后续写入不会带回来。
        assert!(slot.read().unwrap().is_none(), "finish 应关闭捕获槽");

        // 守卫 drop 而未 finish 时同样关闭。
        *slot.write().unwrap() = Some(vec!["未 finish".to_string()]);
        {
            let _guard = ReplyCapture { slot: slot.clone() };
        }
        assert!(slot.read().unwrap().is_none(), "守卫 drop 应关闭捕获槽");
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

    /// 发送前的提交记录：**只有数量、没有正文**，级别固定 debug。
    ///
    /// 正文可能在「提交 5 条」这一步就泄出歌名/昵称/玩法秘密，所以提交日志
    /// 刻意不带内容；内容另走 trace（见 send_batch_logged）。
    #[test]
    fn submission_line_reports_only_the_count_at_debug() {
        let (level, text) = submission_log_line(ChatSendTarget::PrimaryHall, 5).unwrap();
        assert_eq!(level, log::Level::Debug);
        assert_eq!(text, "向一级大厅提交 5 条回复");

        let (level, text) = submission_log_line(ChatSendTarget::CurrentChat, 2).unwrap();
        assert_eq!(level, log::Level::Debug);
        assert_eq!(text, "向当前聊天提交 2 条回复");

        // 没有提交内容时不该产出日志行，避免刷出「提交 0 条」。
        assert!(submission_log_line(ChatSendTarget::PrimaryHall, 0).is_none());
        assert!(submission_log_line(ChatSendTarget::CurrentChat, 0).is_none());
    }

    /// 全成功：一条 info，写明 `成功数/总数`，不额外告警。
    #[test]
    fn full_delivery_reports_sent_over_total_at_info() {
        let outcome = ChatBatchSendOutcome::complete(5);
        let lines = ChatSendReport::new(ChatSendTarget::PrimaryHall, &outcome, 5).lines();
        assert_eq!(lines.len(), 1, "全部送达只应有一条结果日志: {lines:?}");
        assert_eq!(lines[0].0, log::Level::Info);
        assert_eq!(lines[0].1, "已在一级大厅发送 5/5 条回复");
    }

    /// **核心回归**：提交 5 条成功 3 条——必须同时写出「3/5」和「2 条未送达」。
    ///
    /// 旧实现只遍历 `outcome.sent`，于是日志只有 3 行「准备发送」，
    /// 读起来像「只准备了 3 条」，完全看不出还有 2 条玩家没收到。
    #[test]
    fn partial_delivery_reports_sent_total_and_the_undelivered_count() {
        let outcome = ChatBatchSendOutcome::failed(3, anyhow!("第二条超时"));
        let lines = ChatSendReport::new(ChatSendTarget::CurrentChat, &outcome, 5).lines();
        assert_eq!(lines.len(), 2, "部分失败必须有 info + warn 两条: {lines:?}");
        assert_eq!(lines[0].0, log::Level::Info);
        assert_eq!(lines[0].1, "已在当前聊天发送 3/5 条回复");
        assert_eq!(lines[1].0, log::Level::Warn);
        assert!(
            lines[1].1.contains("有 2 条回复未送达"),
            "必须写明未送达条数（5-3=2），实际：{}",
            lines[1].1
        );
        assert!(
            lines[1].1.contains("玩家看不到"),
            "必须说明影响：玩家看不到这几条，实际：{}",
            lines[1].1
        );
        assert!(
            lines[1].1.contains("第二条超时"),
            "必须带出失败原因，实际：{}",
            lines[1].1
        );
    }

    /// 一条都没送到：error 级别，且说明整批玩家都看不到。
    #[test]
    fn zero_delivery_is_reported_as_error() {
        let outcome = ChatBatchSendOutcome::failed(0, anyhow!("一级大厅不可用"));
        let lines = ChatSendReport::new(ChatSendTarget::PrimaryHall, &outcome, 4).lines();
        assert_eq!(lines.len(), 1, "全失败只报一条 error: {lines:?}");
        assert_eq!(lines[0].0, log::Level::Error);
        assert!(lines[0].1.contains("发送 0/4 条回复全部失败"), "{}", lines[0].1);
        assert!(lines[0].1.contains("玩家看不到"), "{}", lines[0].1);
        assert!(lines[0].1.contains("一级大厅不可用"), "{}", lines[0].1);
    }

    /// 正文都发出去了但发送后恢复驻留失败：不能谎称整批没问题。
    ///
    /// 这类失败 `sent == total`，但界面位置未知、后续截图可能识别错，
    /// 必须在 info 之外补一条 warn。
    #[test]
    fn delivered_but_residency_failed_still_warns() {
        let outcome = ChatBatchSendOutcome::failed(3, anyhow!("恢复驻留超时"));
        let lines = ChatSendReport::new(ChatSendTarget::PrimaryHall, &outcome, 3).lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].0, log::Level::Info);
        assert_eq!(lines[0].1, "已在一级大厅发送 3/3 条回复");
        assert_eq!(lines[1].0, log::Level::Warn);
        assert!(lines[1].1.contains("已全部送达"), "{}", lines[1].1);
        assert!(lines[1].1.contains("未能恢复到原界面"), "{}", lines[1].1);
        assert!(lines[1].1.contains("恢复驻留超时"), "{}", lines[1].1);
    }

    /// 三条发送路径共用同一套措辞：换目标只能改称谓，不能改句式与级别。
    #[test]
    fn every_send_target_uses_the_same_log_shape() {
        for (target, label) in [
            (ChatSendTarget::PrimaryHall, "一级大厅"),
            (ChatSendTarget::CurrentChat, "当前聊天"),
        ] {
            assert_eq!(target.label(), label);
            let (level, text) = submission_log_line(target, 1).unwrap();
            assert_eq!(level, log::Level::Debug);
            assert_eq!(text, format!("向{label}提交 1 条回复"));

            let lines =
                ChatSendReport::new(target, &ChatBatchSendOutcome::complete(1), 1).lines();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].0, log::Level::Info);
            assert_eq!(lines[0].1, format!("已在{label}发送 1/1 条回复"));
        }
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
            redacted_chat_text("【谁是卧底谜底·仅私聊】平民词：苹果"),
            "[谁是卧底秘密内容已隐藏]"
        );
        assert_eq!(
            redacted_chat_text("平民词:苹果,卧底词:梨,卧底:B"),
            "[谁是卧底秘密内容已隐藏]"
        );
        assert_eq!(
            redacted_chat_text("请存活玩家好友私聊 #A"),
            "请存活玩家好友私聊 #A"
        );
    }
}
