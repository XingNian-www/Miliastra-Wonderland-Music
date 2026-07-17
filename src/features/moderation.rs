use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PointConfig, RectConfig, validate_rect};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationCommand {
    pub action: ModerationAction,
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

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        self.action == other.action && self.uid == other.uid
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
    fn acquire(&self, key: ModerationWorkflowKey) -> Result<bool>;
    fn release(&self, key: ModerationWorkflowKey) -> Result<bool>;
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
    hold: ModerationHoldLease,
}

pub enum ModerationResultExecution {
    Completed,
}

struct ModerationWorkflowLease {
    ledger: Arc<dyn ModerationWorkflowLedger>,
    key: ModerationWorkflowKey,
    active: bool,
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
        port: &mut dyn ModerationCommandPort,
    ) -> Result<ModerationStart> {
        let Some(lease) = self.try_acquire(command)? else {
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

    pub fn run_vote(
        &self,
        mut work: ModerationVoteWork,
        vote_port: &mut dyn ModerationVotePort,
        task_port: &dyn ModerationTaskPort,
    ) -> Result<()> {
        let approved = match self.wait_for_votes(&work.command, vote_port) {
            Ok(approved) => approved,
            Err(error) => {
                log::error!("{}后台投票失败: {error:#}", work.command.action.label());
                false
            }
        };
        vote_port.finish();
        if !vote_port.is_running() {
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
        if !task_port.is_running() {
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

        if let Err(error) = port.send_hall(&format!(
            "@UID{}的{}请求已通过,开始执行",
            task.command.uid,
            task.command.action.label()
        )) {
            task.release_hold();
            port.sync_listener_state();
            task.release_lease();
            return Err(error);
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
        port: &mut dyn ModerationVotePort,
    ) -> Result<bool> {
        let deadline = port.now() + self.policy.vote_timeout;
        let mut stable_votes: HashMap<String, bool> = HashMap::new();
        let mut samples: HashMap<(String, bool), u32> = HashMap::new();
        while port.is_running() && port.now() < deadline {
            port.wait(self.policy.vote_poll_interval);
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
        if !port.is_running() {
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

    fn try_acquire(&self, command: &ModerationCommand) -> Result<Option<ModerationWorkflowLease>> {
        let key = workflow_key(command);
        if !self.ledger.acquire(key.clone())? {
            return Ok(None);
        }
        Ok(Some(ModerationWorkflowLease {
            ledger: self.ledger.clone(),
            key,
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
            hold: self.hold,
        }
    }

    pub fn cancel(&mut self) {
        self.hold.release();
        self.lease.release();
    }
}

impl ModerationResultTask {
    pub fn label(&self) -> String {
        format!(
            "{} UID{} 投票{}",
            self.command.action.label(),
            self.command.uid,
            if self.approved { "通过" } else { "未通过" }
        )
    }

    pub fn dedup_key(&self) -> String {
        self.command.lock_key()
    }

    pub fn cancel(&mut self) {
        self.release_hold();
        self.release_lease();
    }

    fn release_hold(&mut self) {
        self.hold.release();
    }

    fn release_lease(&mut self) {
        self.lease.release();
    }
}

impl ModerationWorkflowLease {
    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Err(error) = self.ledger.release(self.key.clone()) {
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
    use std::collections::{HashSet, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use anyhow::anyhow;

    #[derive(Default)]
    struct TestLedger {
        active: Mutex<HashSet<ModerationWorkflowKey>>,
    }

    impl ModerationWorkflowLedger for TestLedger {
        fn acquire(&self, key: ModerationWorkflowKey) -> Result<bool> {
            Ok(self.active.lock().unwrap().insert(key))
        }

        fn release(&self, key: ModerationWorkflowKey) -> Result<bool> {
            Ok(self.active.lock().unwrap().remove(&key))
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
        hold_active: Arc<AtomicBool>,
        events: Vec<String>,
    }

    impl FakeExecutionPort {
        fn ready(hold_active: Arc<AtomicBool>, action_result: Result<bool>) -> Self {
            Self {
                action_result,
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
            Ok(())
        }

        fn execute_action(&mut self, _command: &ModerationCommand) -> Result<bool> {
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
        let ModerationStart::Started(work) = service.start(&command(), port).unwrap() else {
            panic!("moderation vote should start");
        };
        work
    }

    #[test]
    fn duplicate_workflow_is_rejected_until_the_lease_is_released() {
        let service = service(2, 2);
        let mut port = FakeCommandPort::new();
        let mut work = start(&service, &mut port);

        assert!(matches!(
            service.start(&command(), &mut port).unwrap(),
            ModerationStart::Duplicate
        ));
        assert!(service.is_active(&command()).unwrap());
        assert!(port.hold_active.load(Ordering::SeqCst));

        work.cancel();

        assert!(!service.is_active(&command()).unwrap());
        assert!(!port.hold_active.load(Ordering::SeqCst));
        assert!(matches!(
            service.start(&command(), &mut port).unwrap(),
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
