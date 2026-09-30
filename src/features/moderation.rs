use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PointConfig, RectConfig, validate_rect};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::features::identity::IdentityRole;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationConfig {
    pub stable_vote_samples: u32,
    pub required_vote_margin: i32,
    pub friend_panel_region: RectConfig,
    pub search_panel_region: RectConfig,
    pub search_input_point: PointConfig,
    pub search_button_point: PointConfig,
    pub more_settings_region: RectConfig,
    pub block_chat_region: RectConfig,
    pub blacklist_region: RectConfig,
    pub confirm_region: RectConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationTimingConfig {
    pub vote_timeout_ms: u64,
    pub vote_poll_ms: u64,
    pub search_result_timeout_ms: u64,
    pub confirm_wait_ms: u64,
}

impl Default for ModerationTimingConfig {
    fn default() -> Self {
        Self {
            vote_timeout_ms: 120000,
            vote_poll_ms: 2000,
            search_result_timeout_ms: 5000,
            confirm_wait_ms: 2000,
        }
    }
}

impl ModerationConfig {
    pub(crate) fn validate(&self, timing: &ModerationTimingConfig) -> Result<()> {
        if self.stable_vote_samples == 0 || self.required_vote_margin <= 0 {
            bail!("管理投票稳定次数和通过票差必须大于 0");
        }
        for (rect, field) in [
            (self.friend_panel_region, "moderation.friend_panel_region"),
            (self.search_panel_region, "moderation.search_panel_region"),
            (self.more_settings_region, "moderation.more_settings_region"),
            (self.block_chat_region, "moderation.block_chat_region"),
            (self.blacklist_region, "moderation.blacklist_region"),
            (self.confirm_region, "moderation.confirm_region"),
        ] {
            validate_rect(rect, field)?;
        }
        if timing.vote_timeout_ms == 0
            || timing.vote_poll_ms == 0
            || timing.search_result_timeout_ms == 0
            || timing.confirm_wait_ms == 0
        {
            bail!("管理投票和执行超时必须大于 0");
        }
        Ok(())
    }
}

/// 好友管理命令：按游戏内 UID 执行拉黑或屏蔽聊天。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationCommand {
    pub action: ModerationAction,
    /// 游戏内好友 UID，不是身份映射模块使用的 UUID。
    /// 该 UID 直接用于游戏好友搜索、拉黑和屏蔽操作。
    pub uid: String,
    pub requester: String,
}

impl ModerationCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::At
            && envelope.authority() == CommandAuthority::Friend
            && ["拉黑UID", "屏蔽UID", "拉黑", "屏蔽"]
                .iter()
                .any(|prefix| strip_ascii_case_prefix(envelope.command_text(), prefix).is_some())
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        let parsed = parse_command(envelope.command_text(), envelope.username())?;
        let raw = format!(
            "{} {} {}",
            parsed.matched,
            envelope.username(),
            parsed.command.uid
        );
        Some(FeatureCommandMatch::new(
            parsed.matched,
            raw,
            parsed.command,
        ))
    }

    pub fn lock_key(&self) -> String {
        format!("moderation:{}:{}", self.action.label(), self.uid)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ModerationAction {
    Blacklist,
    BlockChat,
}

impl ModerationAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blacklist => "拉黑",
            Self::BlockChat => "屏蔽",
        }
    }
}

pub struct ModerationCommandMatch {
    pub matched: &'static str,
    pub command: ModerationCommand,
}

pub fn parse_command(command_text: &str, username: &str) -> Option<ModerationCommandMatch> {
    for (prefix, action) in [
        ("拉黑UID", ModerationAction::Blacklist),
        ("屏蔽UID", ModerationAction::BlockChat),
        ("拉黑", ModerationAction::Blacklist),
        ("屏蔽", ModerationAction::BlockChat),
    ] {
        let Some(rest) = strip_ascii_case_prefix(command_text, prefix) else {
            continue;
        };
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.len() != 9 || !command_boundary(rest[digits.len()..].chars().next()) {
            return None;
        }
        return Some(ModerationCommandMatch {
            matched: prefix,
            command: ModerationCommand {
                action,
                uid: digits,
                requester: username.to_string(),
            },
        });
    }
    None
}

#[derive(Clone, Copy, Debug)]
pub struct ModerationPolicy {
    vote_timeout: Duration,
    vote_poll_interval: Duration,
    stable_vote_samples: u32,
    required_vote_margin: i32,
}

impl ModerationPolicy {
    pub fn new(
        vote_timeout: Duration,
        vote_poll_interval: Duration,
        stable_vote_samples: u32,
        required_vote_margin: i32,
    ) -> Self {
        Self {
            vote_timeout,
            vote_poll_interval,
            stable_vote_samples,
            required_vote_margin,
        }
    }
}

pub trait ModerationPrimaryHold: Send {
    fn release(&mut self);
}

pub trait ModerationCommandPort {
    fn send_hall(&mut self, message: &str) -> Result<()>;
    fn prepare_vote_hold(&mut self) -> Result<Box<dyn ModerationPrimaryHold>>;
}

pub trait ModerationVotePort {
    fn now(&self) -> Instant;
    fn wait(&mut self, duration: Duration);
    fn is_running(&self) -> bool;
    fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>>;
    fn finish(&mut self);
}

pub trait ModerationTaskPort {
    fn is_running(&self) -> bool;
    fn submit_result(&self, task: ModerationResultTask) -> Result<()>;
    fn sync_listener_state(&self);
}

pub trait ModerationExecutionPort {
    fn send_hall(&mut self, message: &str) -> Result<()>;
    fn execute_action(&mut self, command: &ModerationCommand) -> Result<bool>;
    fn sync_listener_state(&mut self);
    fn wait_after_action(&mut self);
}

pub(crate) trait ModerationWorkflowLedger: Send + Sync {
    fn acquire(
        &self,
        key: ModerationWorkflowKey,
        direct: bool,
    ) -> Result<Option<Arc<ModerationWorkflowToken>>>;
    fn release(
        &self,
        key: ModerationWorkflowKey,
        token: Arc<ModerationWorkflowToken>,
    ) -> Result<bool>;
    #[cfg(test)]
    fn contains(&self, key: ModerationWorkflowKey) -> Result<bool>;
}

#[derive(Clone)]
pub struct ModerationService {
    ledger: Arc<dyn ModerationWorkflowLedger>,
    policy: ModerationPolicy,
}

pub enum ModerationStart {
    Duplicate,
    Started(ModerationVoteWork),
    Ready(ModerationResultTask),
}

pub struct ModerationVoteWork {
    command: ModerationCommand,
    lease: ModerationWorkflowLease,
    hold: ModerationHoldLease,
}

pub struct ModerationResultTask {
    command: ModerationCommand,
    approved: bool,
    lease: ModerationWorkflowLease,
    // 直接执行不创建投票驻留；UI 事务仍负责定位、确认及恢复。
    hold: Option<ModerationHoldLease>,
    // 组合层从路由发送者绑定，正式任务执行前再次检查实时身份。
    direct_sender: Option<String>,
}

pub enum ModerationResultExecution {
    Completed,
}

struct ModerationWorkflowLease {
    ledger: Arc<dyn ModerationWorkflowLedger>,
    key: ModerationWorkflowKey,
    token: Arc<ModerationWorkflowToken>,
    active: bool,
}

const WORKFLOW_PENDING: u8 = 0;
const WORKFLOW_EXECUTING: u8 = 1;
const WORKFLOW_RETIRED: u8 = 2;

/// 令牌身份区分同一 UID 的历次流程；CAS 将投票接管和开始执行串行化。
#[derive(Debug)]
pub(crate) struct ModerationWorkflowToken {
    id: uuid::Uuid,
    phase: AtomicU8,
}

#[derive(Default)]
pub(crate) struct ModerationWorkflowRegistry {
    active: HashMap<ModerationWorkflowKey, Arc<ModerationWorkflowToken>>,
}

impl ModerationWorkflowRegistry {
    pub(crate) fn acquire(
        &mut self,
        key: ModerationWorkflowKey,
        direct: bool,
    ) -> Option<Arc<ModerationWorkflowToken>> {
        if let Some(previous) = self.active.get(&key) {
            // 仅管理员直接请求可接管待决工作；开始执行后永不接管，避免重复 UI。
            if !direct
                || previous
                    .phase
                    .compare_exchange(
                        WORKFLOW_PENDING,
                        WORKFLOW_RETIRED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
            {
                return None;
            }
        }
        let token = Arc::new(ModerationWorkflowToken {
            id: uuid::Uuid::new_v4(),
            phase: AtomicU8::new(if direct {
                WORKFLOW_EXECUTING
            } else {
                WORKFLOW_PENDING
            }),
        });
        self.active.insert(key, token.clone());
        Some(token)
    }

    pub(crate) fn release(
        &mut self,
        key: &ModerationWorkflowKey,
        token: &Arc<ModerationWorkflowToken>,
    ) -> bool {
        if !self
            .active
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, token))
        {
            return false;
        }
        token.phase.store(WORKFLOW_RETIRED, Ordering::Release);
        self.active.remove(key);
        true
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &ModerationWorkflowKey) -> bool {
        self.active.contains_key(key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModerationWorkflowKey {
    action: ModerationAction,
    uid: String,
}

struct ModerationHoldLease {
    hold: Box<dyn ModerationPrimaryHold>,
    active: bool,
}

impl ModerationService {
    pub(crate) fn new(policy: ModerationPolicy, ledger: Arc<dyn ModerationWorkflowLedger>) -> Self {
        Self { ledger, policy }
    }

    pub fn start(
        &self,
        command: &ModerationCommand,
        requester_role: Option<IdentityRole>,
        port: &mut dyn ModerationCommandPort,
    ) -> Result<ModerationStart> {
        // 即使绕过聊天解析构造命令，也不能把非法 UID 送入 UI。
        if command.uid.len() != 9 || !command.uid.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("管理命令 UID 必须为 9 位 ASCII 数字");
        }
        if let Some(direct) = self.reserve_direct_blacklist(command, requester_role)? {
            return Ok(direct);
        }
        let Some(lease) = self.try_acquire(command, false)? else {
            log::info!(
                "{} UID{} 已有投票或执行流程，跳过重复请求",
                command.action.label(),
                command.uid
            );
            port.send_hall(&format!(
                "@UID{}的{}请求正在处理中",
                command.uid,
                command.action.label()
            ))?;
            return Ok(ModerationStart::Duplicate);
        };

        let vote_timeout_seconds = self.policy.vote_timeout.as_millis().saturating_add(999) / 1000;
        port.send_hall(&format!(
            "管理员发起了对@UID{}的{}请求,请好友{}s内使用@同意/不同意进行判决",
            command.uid,
            command.action.label(),
            vote_timeout_seconds,
        ))?;
        let hold = port.prepare_vote_hold()?;
        Ok(ModerationStart::Started(ModerationVoteWork {
            command: command.clone(),
            lease,
            hold: ModerationHoldLease::new(hold),
        }))
    }

    /// 只预占/接管租约，不发送消息、不操作 UI，供可信管理员在正式任务入队前使用。
    pub(crate) fn reserve_direct_blacklist(
        &self,
        command: &ModerationCommand,
        requester_role: Option<IdentityRole>,
    ) -> Result<Option<ModerationStart>> {
        if command.action != ModerationAction::Blacklist
            || !matches!(
                requester_role,
                Some(IdentityRole::Admin | IdentityRole::Owner)
            )
        {
            return Ok(None);
        }
        if command.uid.len() != 9 || !command.uid.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("管理命令 UID 必须为 9 位 ASCII 数字");
        }
        let Some(lease) = self.try_acquire(command, true)? else {
            return Ok(Some(ModerationStart::Duplicate));
        };
        Ok(Some(ModerationStart::Ready(ModerationResultTask {
            command: command.clone(),
            approved: true,
            lease,
            hold: None,
            direct_sender: None,
        })))
    }

    pub fn run_vote(
        &self,
        mut work: ModerationVoteWork,
        vote_port: &mut dyn ModerationVotePort,
        task_port: &dyn ModerationTaskPort,
    ) -> Result<()> {
        let approved = match self.wait_for_votes(&work.command, &work.lease, vote_port) {
            Ok(approved) => approved,
            Err(error) => {
                log::error!("{}后台投票失败: {error:#}", work.command.action.label());
                false
            }
        };
        vote_port.finish();
        if !vote_port.is_running() || !work.lease.is_current() {
            work.cancel();
            task_port.sync_listener_state();
            return Ok(());
        }
        self.submit_vote_result(work.finish(approved), task_port)
    }

    pub fn fail_vote(
        &self,
        mut work: ModerationVoteWork,
        task_port: &dyn ModerationTaskPort,
    ) -> Result<()> {
        if !task_port.is_running() || !work.lease.is_current() {
            work.cancel();
            task_port.sync_listener_state();
            return Ok(());
        }
        self.submit_vote_result(work.finish(false), task_port)
    }

    pub fn execute_result(
        &self,
        mut task: ModerationResultTask,
        port: &mut dyn ModerationExecutionPort,
    ) -> Result<ModerationResultExecution> {
        // 管理员接管与普通结果开始执行通过同一 token CAS 竞争；失效结果只能清理自身。
        if !task.lease.begin_result(task.hold.is_none()) {
            task.release_hold();
            port.sync_listener_state();
            task.release_lease();
            return Ok(ModerationResultExecution::Completed);
        }
        if !task.approved {
            task.release_hold();
            port.sync_listener_state();
            let result = port.send_hall(&format!(
                "@UID{}的{}请求未通过",
                task.command.uid,
                task.command.action.label()
            ));
            task.release_lease();
            result?;
            return Ok(ModerationResultExecution::Completed);
        }

        let direct = task.hold.is_none();
        let message = if direct {
            format!(
                "管理员直接执行对@UID{}的{}",
                task.command.uid,
                task.command.action.label()
            )
        } else {
            format!(
                "@UID{}的{}请求已通过,开始执行",
                task.command.uid,
                task.command.action.label()
            )
        };
        if let Err(error) = port.send_hall(&message) {
            if !direct {
                task.release_hold();
                port.sync_listener_state();
                task.release_lease();
                return Err(error);
            }
            // 通知不是管理员直接拉黑的审批条件，发送失败也只执行一次 UI 动作。
            log::error!("管理员直接拉黑开始通告发送失败: {error:#}");
        }

        let result = port.execute_action(&task.command);
        task.release_hold();
        port.sync_listener_state();
        port.wait_after_action();
        match &result {
            Ok(true) => {
                if let Err(error) = port.send_hall(&format!(
                    "已对@UID{}执行{}",
                    task.command.uid,
                    task.command.action.label()
                )) {
                    log::error!("{}成功通告发送失败: {error:#}", task.command.action.label());
                }
            }
            Ok(false) => {
                let _ = port.send_hall(&format!(
                    "@UID{}的{}流程出错",
                    task.command.uid,
                    task.command.action.label()
                ));
            }
            Err(error) => {
                log::error!(
                    "{}执行结果未知，禁止重放: {error:#}",
                    task.command.action.label()
                );
                let _ = port.send_hall(&format!(
                    "@UID{}的{}执行结果未知,请勿重复操作",
                    task.command.uid,
                    task.command.action.label()
                ));
            }
        }
        task.release_lease();
        result.map(|_| ModerationResultExecution::Completed)
    }

    fn wait_for_votes(
        &self,
        command: &ModerationCommand,
        lease: &ModerationWorkflowLease,
        port: &mut dyn ModerationVotePort,
    ) -> Result<bool> {
        let deadline = port.now() + self.policy.vote_timeout;
        let mut stable_votes: HashMap<String, bool> = HashMap::new();
        let mut samples: HashMap<(String, bool), u32> = HashMap::new();
        while port.is_running() && lease.is_current() && port.now() < deadline {
            port.wait(self.policy.vote_poll_interval);
            if !lease.is_current() {
                return Ok(false);
            }
            match port.poll_visible_friend_messages() {
                Ok(messages) => {
                    for message in messages {
                        let Some((username, agreed)) = parse_friend_moderation_vote(&message)
                        else {
                            continue;
                        };
                        let key = (username.clone(), agreed);
                        let count = samples
                            .entry(key)
                            .and_modify(|value| *value += 1)
                            .or_insert(1);
                        if *count >= self.policy.stable_vote_samples {
                            stable_votes.insert(username, agreed);
                        }
                    }
                }
                Err(error) => {
                    log::error!("{}投票扫描失败: {error:#}", command.action.label());
                    continue;
                }
            }
            let agree = stable_votes.values().filter(|agreed| **agreed).count() as i32;
            let disagree = stable_votes.values().filter(|agreed| !**agreed).count() as i32;
            log::info!(
                "{}投票: 同意={} 不同意={} 差值={} 目标差值={}",
                command.action.label(),
                agree,
                disagree,
                agree - disagree,
                self.policy.required_vote_margin,
            );
            if agree - disagree >= self.policy.required_vote_margin {
                return Ok(true);
            }
        }
        if !port.is_running() || !lease.is_current() {
            return Ok(false);
        }
        let agree = stable_votes.values().filter(|agreed| **agreed).count() as i32;
        let disagree = stable_votes.values().filter(|agreed| !**agreed).count() as i32;
        if disagree == 0 {
            log::info!(
                "{}投票超时: 同意={} 不同意=0，无反对，按通过处理",
                command.action.label(),
                agree,
            );
            Ok(true)
        } else {
            log::info!(
                "{}投票超时: 同意={} 不同意={}，未达到目标差值，按未通过处理",
                command.action.label(),
                agree,
                disagree,
            );
            Ok(false)
        }
    }

    fn submit_vote_result(
        &self,
        task: ModerationResultTask,
        port: &dyn ModerationTaskPort,
    ) -> Result<()> {
        if let Err(error) = port.submit_result(task) {
            port.sync_listener_state();
            return Err(error);
        }
        Ok(())
    }

    fn try_acquire(
        &self,
        command: &ModerationCommand,
        direct: bool,
    ) -> Result<Option<ModerationWorkflowLease>> {
        let key = workflow_key(command);
        let Some(token) = self.ledger.acquire(key.clone(), direct)? else {
            return Ok(None);
        };
        Ok(Some(ModerationWorkflowLease {
            ledger: self.ledger.clone(),
            key,
            token,
            active: true,
        }))
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self, command: &ModerationCommand) -> Result<bool> {
        self.ledger.contains(workflow_key(command))
    }
}

impl ModerationVoteWork {
    pub fn command(&self) -> &ModerationCommand {
        &self.command
    }

    pub fn finish(self, approved: bool) -> ModerationResultTask {
        ModerationResultTask {
            command: self.command,
            approved,
            lease: self.lease,
            hold: Some(self.hold),
            direct_sender: None,
        }
    }

    pub fn cancel(&mut self) {
        self.hold.release();
        self.lease.release();
    }
}

impl ModerationResultTask {
    pub(crate) fn is_direct(&self) -> bool {
        self.hold.is_none()
    }

    pub(crate) fn bind_direct_sender(&mut self, sender: &str) {
        self.direct_sender = Some(sender.to_string());
    }

    pub(crate) fn direct_sender(&self) -> Option<&str> {
        self.direct_sender.as_deref()
    }

    pub fn label(&self) -> String {
        if self.hold.is_none() {
            return format!(
                "{} UID{} 管理员直接执行",
                self.command.action.label(),
                self.command.uid
            );
        }
        format!(
            "{} UID{} 投票{}",
            self.command.action.label(),
            self.command.uid,
            if self.approved { "通过" } else { "未通过" }
        )
    }

    pub fn dedup_key(&self) -> String {
        // 结果与命令分开，旧代次结果也不能在队列层吞掉管理员接管任务。
        format!(
            "moderation-result:{}:{}",
            self.command.lock_key(),
            self.lease.token.id
        )
    }

    pub fn cancel(&mut self) {
        self.release_hold();
        self.release_lease();
    }

    fn release_hold(&mut self) {
        if let Some(hold) = &mut self.hold {
            hold.release();
        }
    }

    fn release_lease(&mut self) {
        self.lease.release();
    }
}

impl ModerationWorkflowLease {
    fn is_current(&self) -> bool {
        self.active && self.token.phase.load(Ordering::Acquire) != WORKFLOW_RETIRED
    }

    fn begin_result(&self, direct: bool) -> bool {
        if !self.active {
            return false;
        }
        if direct {
            return self.token.phase.load(Ordering::Acquire) == WORKFLOW_EXECUTING;
        }
        self.token
            .phase
            .compare_exchange(
                WORKFLOW_PENDING,
                WORKFLOW_EXECUTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Err(error) = self.ledger.release(self.key.clone(), self.token.clone()) {
            log::error!(
                "无法释放管理工作流 {}:{}: {error:#}",
                self.key.action.label(),
                self.key.uid
            );
        }
    }
}

impl ModerationHoldLease {
    fn new(hold: Box<dyn ModerationPrimaryHold>) -> Self {
        Self { hold, active: true }
    }

    fn release(&mut self) {
        if self.active {
            self.hold.release();
            self.active = false;
        }
    }
}

impl Drop for ModerationHoldLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl Drop for ModerationWorkflowLease {
    fn drop(&mut self) {
        self.release();
    }
}

fn workflow_key(command: &ModerationCommand) -> ModerationWorkflowKey {
    ModerationWorkflowKey {
        action: command.action,
        uid: command.uid.clone(),
    }
}

fn parse_friend_moderation_vote(text: &str) -> Option<(String, bool)> {
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let username = text[..sep_index]
        .trim_matches(['[', '【', ']', '】', ' ', '\t'])
        .to_string();
    if username.trim().is_empty() {
        return None;
    }
    let sep_len = text[sep_index..].chars().next()?.len_utf8();
    let command_text = text[sep_index + sep_len..]
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】'])
        .strip_prefix('@')?
        .trim_start();
    if command_text
        .strip_prefix("不同意")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some((username, false))
    } else if command_text
        .strip_prefix("同意")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some((username, true))
    } else {
        None
    }
}

pub(crate) fn is_moderation_vote_message(text: &str) -> bool {
    parse_friend_moderation_vote(text).is_some()
}

fn decision_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
}

fn command_boundary(ch: Option<char>) -> bool {
    decision_boundary(ch)
}

fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use anyhow::anyhow;

    #[derive(Default)]
    struct TestLedger {
        active: Mutex<ModerationWorkflowRegistry>,
    }

    impl ModerationWorkflowLedger for TestLedger {
        fn acquire(
            &self,
            key: ModerationWorkflowKey,
            direct: bool,
        ) -> Result<Option<Arc<ModerationWorkflowToken>>> {
            Ok(self.active.lock().unwrap().acquire(key, direct))
        }

        fn release(
            &self,
            key: ModerationWorkflowKey,
            token: Arc<ModerationWorkflowToken>,
        ) -> Result<bool> {
            Ok(self.active.lock().unwrap().release(&key, &token))
        }

        fn contains(&self, key: ModerationWorkflowKey) -> Result<bool> {
            Ok(self.active.lock().unwrap().contains(&key))
        }
    }

    struct FakeHold {
        active: Arc<AtomicBool>,
    }

    impl ModerationPrimaryHold for FakeHold {
        fn release(&mut self) {
            self.active.store(false, Ordering::SeqCst);
        }
    }

    struct FakeCommandPort {
        messages: Vec<String>,
        hold_active: Arc<AtomicBool>,
    }

    impl FakeCommandPort {
        fn new() -> Self {
            Self {
                messages: Vec::new(),
                hold_active: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl ModerationCommandPort for FakeCommandPort {
        fn send_hall(&mut self, message: &str) -> Result<()> {
            self.messages.push(message.to_string());
            Ok(())
        }

        fn prepare_vote_hold(&mut self) -> Result<Box<dyn ModerationPrimaryHold>> {
            self.hold_active.store(true, Ordering::SeqCst);
            Ok(Box::new(FakeHold {
                active: self.hold_active.clone(),
            }))
        }
    }

    struct FakeVotePort {
        now: Instant,
        running: bool,
        finished: bool,
        polls: VecDeque<Result<Vec<String>>>,
    }

    impl FakeVotePort {
        fn new(polls: impl IntoIterator<Item = Vec<String>>) -> Self {
            Self {
                now: Instant::now(),
                running: true,
                finished: false,
                polls: polls.into_iter().map(Ok).collect(),
            }
        }
    }

    impl ModerationVotePort for FakeVotePort {
        fn now(&self) -> Instant {
            self.now
        }

        fn wait(&mut self, duration: Duration) {
            self.now += duration;
        }

        fn is_running(&self) -> bool {
            self.running
        }

        fn poll_visible_friend_messages(&mut self) -> Result<Vec<String>> {
            self.polls.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }

        fn finish(&mut self) {
            self.finished = true;
        }
    }

    struct FakeTaskPort {
        running: bool,
        fail_submit: bool,
        tasks: Mutex<Vec<ModerationResultTask>>,
        sync_count: AtomicUsize,
    }

    impl FakeTaskPort {
        fn new() -> Self {
            Self {
                running: true,
                fail_submit: false,
                tasks: Mutex::new(Vec::new()),
                sync_count: AtomicUsize::new(0),
            }
        }

        fn failing() -> Self {
            Self {
                fail_submit: true,
                ..Self::new()
            }
        }

        fn take(&self) -> ModerationResultTask {
            self.tasks.lock().unwrap().pop().expect("submitted task")
        }
    }

    impl ModerationTaskPort for FakeTaskPort {
        fn is_running(&self) -> bool {
            self.running
        }

        fn submit_result(&self, task: ModerationResultTask) -> Result<()> {
            if self.fail_submit {
                return Err(anyhow!("submit failed"));
            }
            self.tasks.lock().unwrap().push(task);
            Ok(())
        }

        fn sync_listener_state(&self) {
            self.sync_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeExecutionPort {
        action_result: Result<bool>,
        on_action: Option<Box<dyn FnMut()>>,
        fail_notification: bool,
        hold_active: Arc<AtomicBool>,
        events: Vec<String>,
    }

    impl FakeExecutionPort {
        fn ready(hold_active: Arc<AtomicBool>, action_result: Result<bool>) -> Self {
            Self {
                action_result,
                on_action: None,
                fail_notification: false,
                hold_active,
                events: Vec::new(),
            }
        }
    }

    impl ModerationExecutionPort for FakeExecutionPort {
        fn send_hall(&mut self, message: &str) -> Result<()> {
            self.events.push(format!(
                "send:{}:{}",
                message,
                self.hold_active.load(Ordering::SeqCst)
            ));
            if self.fail_notification {
                return Err(anyhow!("notification failed"));
            }
            Ok(())
        }

        fn execute_action(&mut self, _command: &ModerationCommand) -> Result<bool> {
            if let Some(on_action) = &mut self.on_action {
                on_action();
            }
            self.events.push(format!(
                "action:{}",
                self.hold_active.load(Ordering::SeqCst)
            ));
            self.action_result
                .as_ref()
                .copied()
                .map_err(|error| anyhow!(error.to_string()))
        }

        fn sync_listener_state(&mut self) {
            self.events
                .push(format!("sync:{}", self.hold_active.load(Ordering::SeqCst)));
        }

        fn wait_after_action(&mut self) {
            self.events.push("wait".to_string());
        }
    }

    fn service(samples: u32, margin: i32) -> ModerationService {
        ModerationService::new(
            ModerationPolicy::new(
                Duration::from_secs(4),
                Duration::from_secs(1),
                samples,
                margin,
            ),
            Arc::new(TestLedger::default()),
        )
    }

    fn command() -> ModerationCommand {
        ModerationCommand {
            action: ModerationAction::Blacklist,
            uid: "123456789".to_string(),
            requester: "发起者".to_string(),
        }
    }

    fn start(service: &ModerationService, port: &mut FakeCommandPort) -> ModerationVoteWork {
        let ModerationStart::Started(work) = service.start(&command(), None, port).unwrap() else {
            panic!("moderation vote should start");
        };
        work
    }

    #[test]
    fn admin_and_owner_blacklist_bypass_vote_without_a_hold() {
        for role in [IdentityRole::Admin, IdentityRole::Owner] {
            let service = service(99, 99);
            let mut port = FakeCommandPort::new();
            let request = command();
            let ModerationStart::Ready(task) =
                service.start(&request, Some(role), &mut port).unwrap()
            else {
                panic!("administrator blacklist must execute without a vote");
            };
            assert!(port.messages.is_empty());
            assert!(!port.hold_active.load(Ordering::SeqCst));
            assert_eq!(task.command.uid, request.uid);
            assert_ne!(task.dedup_key(), request.lock_key());
            assert!(task.dedup_key().starts_with("moderation-result:"));
            assert!(task.label().contains("管理员直接执行"));
            let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
            service.execute_result(task, &mut execution).unwrap();
            assert_eq!(
                execution.events,
                [
                    "send:管理员直接执行对@UID123456789的拉黑:false",
                    "action:false",
                    "sync:false",
                    "wait",
                    "send:已对@UID123456789执行拉黑:false",
                ]
            );
            assert!(!service.is_active(&request).unwrap());
        }
    }

    #[test]
    fn friends_and_unmapped_senders_keep_blacklist_vote_flow() {
        for role in [None, Some(IdentityRole::Friend)] {
            let service = service(1, 99);
            let mut port = FakeCommandPort::new();
            let mut request = command();
            request.requester = "管理员".to_string();
            let ModerationStart::Started(work) = service.start(&request, role, &mut port).unwrap()
            else {
                panic!("requester display text must not grant administrator privileges");
            };
            assert!(port.hold_active.load(Ordering::SeqCst));
            assert!(port.messages[0].contains("@同意/不同意"));
            let mut votes = FakeVotePort::new([vec!["[甲]：@不同意".to_string()]]);
            let tasks = FakeTaskPort::new();
            service.run_vote(work, &mut votes, &tasks).unwrap();
            let task = tasks.take();
            assert!(!task.approved);
            let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
            service.execute_result(task, &mut execution).unwrap();
            assert!(
                !execution
                    .events
                    .iter()
                    .any(|event| event.starts_with("action:"))
            );
        }
    }

    #[test]
    fn block_chat_keeps_vote_flow_for_every_role() {
        for role in [
            None,
            Some(IdentityRole::Friend),
            Some(IdentityRole::Admin),
            Some(IdentityRole::Owner),
        ] {
            let service = service(1, 1);
            let mut port = FakeCommandPort::new();
            let mut request = command();
            request.action = ModerationAction::BlockChat;
            let ModerationStart::Started(work) = service.start(&request, role, &mut port).unwrap()
            else {
                panic!("block chat must retain the original vote flow");
            };
            assert!(port.hold_active.load(Ordering::SeqCst));
            let mut votes = FakeVotePort::new([vec!["[甲]：@同意".to_string()]]);
            let tasks = FakeTaskPort::new();
            service.run_vote(work, &mut votes, &tasks).unwrap();
            let task = tasks.take();
            assert!(task.approved);
            assert_eq!(task.command.action, ModerationAction::BlockChat);
        }
    }

    #[test]
    fn direct_blacklist_still_validates_uid_before_any_effects() {
        for role in [
            None,
            Some(IdentityRole::Friend),
            Some(IdentityRole::Admin),
            Some(IdentityRole::Owner),
        ] {
            for uid in [
                "",
                "12345678",
                "1234567890",
                "12345678x",
                "１２３４５６７８９",
                "123456789 ",
            ] {
                let service = service(1, 1);
                let mut port = FakeCommandPort::new();
                let mut request = command();
                request.uid = uid.to_string();
                assert!(service.start(&request, role, &mut port).is_err());
                assert!(port.messages.is_empty());
                assert!(!port.hold_active.load(Ordering::SeqCst));
                assert!(!service.is_active(&request).unwrap());
            }
        }
    }

    #[test]
    fn direct_blacklist_and_vote_share_the_same_duplicate_guard() {
        let service = service(1, 1);
        let mut port = FakeCommandPort::new();
        let ModerationStart::Ready(mut task) = service
            .start(&command(), Some(IdentityRole::Admin), &mut port)
            .unwrap()
        else {
            panic!("direct task expected");
        };
        for role in [
            None,
            Some(IdentityRole::Friend),
            Some(IdentityRole::Admin),
            Some(IdentityRole::Owner),
        ] {
            assert!(matches!(
                service.start(&command(), role, &mut port).unwrap(),
                ModerationStart::Duplicate
            ));
        }
        assert!(service.is_active(&command()).unwrap());
        task.cancel();
        assert!(!service.is_active(&command()).unwrap());
        let mut work = start(&service, &mut port);
        let ModerationStart::Ready(mut replacement) = service
            .start(&command(), Some(IdentityRole::Owner), &mut port)
            .unwrap()
        else {
            panic!("administrator must take over pending vote");
        };
        work.cancel();
        assert!(service.is_active(&command()).unwrap());
        replacement.cancel();
        assert!(!service.is_active(&command()).unwrap());
    }

    #[test]
    fn admin_takes_over_running_vote_without_waiting_or_submitting_old_rejection() {
        for role in [IdentityRole::Admin, IdentityRole::Owner] {
            let service = service(1, 99);
            let mut port = FakeCommandPort::new();
            let work = start(&service, &mut port);
            let ModerationStart::Ready(task) =
                service.start(&command(), Some(role), &mut port).unwrap()
            else {
                panic!("pending vote must not block administrator");
            };
            let mut votes = FakeVotePort::new([vec!["[甲]：@不同意".to_string()]]);
            let before = votes.now;
            let tasks = FakeTaskPort::new();
            service.run_vote(work, &mut votes, &tasks).unwrap();
            assert_eq!(votes.now, before);
            assert_eq!(votes.polls.len(), 1);
            assert!(votes.finished);
            assert!(tasks.tasks.lock().unwrap().is_empty());
            assert!(service.is_active(&command()).unwrap());
            let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
            service.execute_result(task, &mut execution).unwrap();
            assert_eq!(
                execution
                    .events
                    .iter()
                    .filter(|event| event.starts_with("action:"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn admin_takeover_invalidates_both_approved_and_rejected_queued_results() {
        for approved in [false, true] {
            for finish_old_first in [false, true] {
                let service = service(1, 1);
                let mut port = FakeCommandPort::new();
                let old = start(&service, &mut port).finish(approved);
                let ModerationStart::Ready(direct) = service
                    .start(&command(), Some(IdentityRole::Admin), &mut port)
                    .unwrap()
                else {
                    panic!("pending result must be superseded regardless of approval");
                };
                let mut old_execution =
                    FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
                let mut direct_execution =
                    FakeExecutionPort::ready(Arc::new(AtomicBool::new(false)), Ok(true));
                if finish_old_first {
                    service.execute_result(old, &mut old_execution).unwrap();
                    assert!(
                        service.is_active(&command()).unwrap(),
                        "old result must not release successor"
                    );
                    service
                        .execute_result(direct, &mut direct_execution)
                        .unwrap();
                } else {
                    service
                        .execute_result(direct, &mut direct_execution)
                        .unwrap();
                    service.execute_result(old, &mut old_execution).unwrap();
                }
                assert!(
                    !old_execution
                        .events
                        .iter()
                        .any(|event| event.starts_with("send:") || event.starts_with("action:"))
                );
                assert_eq!(
                    direct_execution
                        .events
                        .iter()
                        .filter(|event| event.starts_with("action:"))
                        .count(),
                    1
                );
                assert!(!service.is_active(&command()).unwrap());
            }
        }
    }

    #[test]
    fn superseded_vote_drop_and_failure_cannot_release_new_lease() {
        for fail_vote in [false, true] {
            let service = service(1, 1);
            let mut port = FakeCommandPort::new();
            let old = start(&service, &mut port);
            let ModerationStart::Ready(mut direct) = service
                .start(&command(), Some(IdentityRole::Owner), &mut port)
                .unwrap()
            else {
                panic!("direct takeover expected");
            };
            if fail_vote {
                let tasks = FakeTaskPort::new();
                service.fail_vote(old, &tasks).unwrap();
                assert!(tasks.tasks.lock().unwrap().is_empty());
            } else {
                drop(old);
            }
            assert!(service.is_active(&command()).unwrap());
            assert!(!port.hold_active.load(Ordering::SeqCst));
            direct.cancel();
            assert!(!service.is_active(&command()).unwrap());
        }
    }

    #[test]
    fn administrator_cannot_take_over_an_action_already_executing() {
        for direct in [false, true] {
            let service = service(1, 1);
            let mut port = FakeCommandPort::new();
            let task = if direct {
                let ModerationStart::Ready(task) = service
                    .start(&command(), Some(IdentityRole::Admin), &mut port)
                    .unwrap()
                else {
                    panic!("direct task expected");
                };
                task
            } else {
                start(&service, &mut port).finish(true)
            };
            let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
            let concurrent_service = service.clone();
            execution.on_action = Some(Box::new(move || {
                let mut port = FakeCommandPort::new();
                for role in [IdentityRole::Admin, IdentityRole::Owner] {
                    assert!(matches!(
                        concurrent_service
                            .start(&command(), Some(role), &mut port)
                            .unwrap(),
                        ModerationStart::Duplicate
                    ));
                }
            }));
            service.execute_result(task, &mut execution).unwrap();
            assert_eq!(
                execution
                    .events
                    .iter()
                    .filter(|event| event.starts_with("action:"))
                    .count(),
                1
            );
            assert!(!service.is_active(&command()).unwrap());
        }
    }

    #[test]
    fn direct_blacklist_confirms_results_without_replaying_the_action() {
        for result in [Ok(true), Ok(false), Err(anyhow!("result unknown"))] {
            let service = service(1, 1);
            let mut port = FakeCommandPort::new();
            let ModerationStart::Ready(task) = service
                .start(&command(), Some(IdentityRole::Admin), &mut port)
                .unwrap()
            else {
                panic!("direct task expected");
            };
            let unknown = result.is_err();
            let expected = match &result {
                Ok(true) => "已对@UID123456789执行拉黑",
                Ok(false) => "流程出错",
                Err(_) => "执行结果未知,请勿重复操作",
            };
            let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), result);
            assert_eq!(
                service.execute_result(task, &mut execution).is_err(),
                unknown
            );
            assert_eq!(
                execution
                    .events
                    .iter()
                    .filter(|event| event.starts_with("action:"))
                    .count(),
                1
            );
            assert!(execution.events.last().unwrap().contains(expected));
            assert!(!service.is_active(&command()).unwrap());
        }
    }

    #[test]
    fn direct_blacklist_does_not_require_successful_notifications() {
        let service = service(1, 1);
        let mut port = FakeCommandPort::new();
        let ModerationStart::Ready(task) = service
            .start(&command(), Some(IdentityRole::Owner), &mut port)
            .unwrap()
        else {
            panic!("direct task expected");
        };
        let mut execution = FakeExecutionPort::ready(port.hold_active.clone(), Ok(true));
        execution.fail_notification = true;
        service.execute_result(task, &mut execution).unwrap();
        assert_eq!(
            execution
                .events
                .iter()
                .filter(|event| event.starts_with("action:"))
                .count(),
            1
        );
        assert!(!service.is_active(&command()).unwrap());
    }

    #[test]
    fn duplicate_workflow_is_rejected_until_the_lease_is_released() {
        let service = service(2, 2);
        let mut port = FakeCommandPort::new();
        let mut work = start(&service, &mut port);

        assert!(matches!(
            service.start(&command(), None, &mut port).unwrap(),
            ModerationStart::Duplicate
        ));
        assert!(service.is_active(&command()).unwrap());
        assert!(port.hold_active.load(Ordering::SeqCst));

        work.cancel();

        assert!(!service.is_active(&command()).unwrap());
        assert!(!port.hold_active.load(Ordering::SeqCst));
        assert!(matches!(
            service.start(&command(), None, &mut port).unwrap(),
            ModerationStart::Started(_)
        ));
    }

    #[test]
    fn stable_votes_reach_the_required_margin() {
        let service = service(2, 2);
        let mut command_port = FakeCommandPort::new();
        let work = start(&service, &mut command_port);
        let batch = vec!["[甲]：@同意".to_string(), "[乙]：@同意".to_string()];
        let mut vote_port = FakeVotePort::new([batch.clone(), batch]);
        let task_port = FakeTaskPort::new();

        service.run_vote(work, &mut vote_port, &task_port).unwrap();
        let task = task_port.take();

        assert!(task.approved);
        assert!(vote_port.finished);
        assert!(service.is_active(&command()).unwrap());
    }

    #[test]
    fn timeout_passes_only_when_no_stable_disagreement_exists() {
        let service = service(1, 3);
        let mut command_port = FakeCommandPort::new();
        let work = start(&service, &mut command_port);
        let mut vote_port = FakeVotePort::new([
            vec!["[甲]：@同意".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ]);
        let task_port = FakeTaskPort::new();
        service.run_vote(work, &mut vote_port, &task_port).unwrap();
        let task = task_port.take();
        assert!(task.approved);

        let mut command_port = FakeCommandPort::new();
        let mut first = task;
        first.cancel();
        let work = start(&service, &mut command_port);
        let mut vote_port = FakeVotePort::new([
            vec!["[甲]：@不同意".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ]);
        let task_port = FakeTaskPort::new();
        service.run_vote(work, &mut vote_port, &task_port).unwrap();
        let task = task_port.take();
        assert!(!task.approved);
    }

    #[test]
    fn failed_result_submission_releases_the_workflow_and_hold() {
        let service = service(1, 1);
        let mut command_port = FakeCommandPort::new();
        let hold_active = command_port.hold_active.clone();
        let work = start(&service, &mut command_port);
        let mut vote_port = FakeVotePort::new([vec!["[甲]：@同意".to_string()]]);
        let task_port = FakeTaskPort::failing();

        assert!(service.run_vote(work, &mut vote_port, &task_port).is_err());

        assert!(!service.is_active(&command()).unwrap());
        assert!(!hold_active.load(Ordering::SeqCst));
        assert_eq!(task_port.sync_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stopped_vote_does_not_submit_a_result() {
        let service = service(1, 1);
        let mut command_port = FakeCommandPort::new();
        let hold_active = command_port.hold_active.clone();
        let work = start(&service, &mut command_port);
        let mut vote_port = FakeVotePort::new(Vec::<Vec<String>>::new());
        vote_port.running = false;
        let task_port = FakeTaskPort::new();

        service.run_vote(work, &mut vote_port, &task_port).unwrap();

        assert!(task_port.tasks.lock().unwrap().is_empty());
        assert!(!service.is_active(&command()).unwrap());
        assert!(!hold_active.load(Ordering::SeqCst));
        assert_eq!(task_port.sync_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn approved_result_keeps_primary_hold_through_action_then_releases_before_feedback() {
        let service = service(1, 1);
        let mut command_port = FakeCommandPort::new();
        let hold_active = command_port.hold_active.clone();
        let task = start(&service, &mut command_port).finish(true);
        let mut execution = FakeExecutionPort::ready(hold_active.clone(), Ok(true));

        assert!(matches!(
            service.execute_result(task, &mut execution).unwrap(),
            ModerationResultExecution::Completed
        ));

        assert_eq!(
            execution.events,
            [
                "send:@UID123456789的拉黑请求已通过,开始执行:true",
                "action:true",
                "sync:false",
                "wait",
                "send:已对@UID123456789执行拉黑:false",
            ]
        );
        assert!(!service.is_active(&command()).unwrap());
        assert!(!hold_active.load(Ordering::SeqCst));
    }

    #[test]
    fn rejected_result_releases_primary_hold_before_feedback() {
        let service = service(1, 1);
        let mut command_port = FakeCommandPort::new();
        let hold_active = command_port.hold_active.clone();
        let task = start(&service, &mut command_port).finish(false);
        let mut execution = FakeExecutionPort::ready(hold_active.clone(), Ok(true));

        assert!(matches!(
            service.execute_result(task, &mut execution).unwrap(),
            ModerationResultExecution::Completed
        ));

        assert_eq!(
            execution.events,
            ["sync:false", "send:@UID123456789的拉黑请求未通过:false",]
        );
        assert!(!service.is_active(&command()).unwrap());
    }

    #[test]
    fn unknown_action_result_warns_against_repeating_the_operation() {
        let service = service(1, 1);
        let mut command_port = FakeCommandPort::new();
        let hold_active = command_port.hold_active.clone();
        let task = start(&service, &mut command_port).finish(true);
        let mut execution =
            FakeExecutionPort::ready(hold_active.clone(), Err(anyhow!("result unknown")));

        assert!(service.execute_result(task, &mut execution).is_err());

        assert_eq!(
            execution.events,
            [
                "send:@UID123456789的拉黑请求已通过,开始执行:true",
                "action:true",
                "sync:false",
                "wait",
                "send:@UID123456789的拉黑执行结果未知,请勿重复操作:false",
            ]
        );
        assert!(!service.is_active(&command()).unwrap());
        assert!(!hold_active.load(Ordering::SeqCst));
    }

    #[test]
    fn vote_parser_requires_a_complete_command_boundary() {
        assert_eq!(
            parse_friend_moderation_vote("[甲]：@同意"),
            Some(("甲".to_string(), true))
        );
        assert_eq!(
            parse_friend_moderation_vote("乙:@不同意！"),
            Some(("乙".to_string(), false))
        );
        assert_eq!(parse_friend_moderation_vote("[甲]：@同意执行"), None);
    }
}
