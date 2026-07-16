use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::features::chat_text::{command_identity, strip_ascii_case_prefix};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InviteCommand {
    pub(crate) username: String,
    pub(crate) seq: Option<u32>,
    pub(crate) password: Option<String>,
}

pub(crate) struct InviteCommandMatch {
    pub(crate) raw_parameter: String,
    pub(crate) command: InviteCommand,
}

impl InviteCommand {
    pub(crate) fn parse_friend(text: &str, username: &str) -> Option<InviteCommandMatch> {
        let rest = strip_ascii_case_prefix(text, "邀请")?.trim_start();
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() || !invite_arg_trailing_is_empty(&rest[digits.len()..]) {
            return None;
        }
        let (seq, password, raw_parameter) = parse_invite_arg(&digits)?;
        Some(InviteCommandMatch {
            raw_parameter,
            command: Self {
                username: username.to_string(),
                seq,
                password,
            },
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        if let Some(sequence) = self.seq {
            format!("invite:{sequence}")
        } else {
            format!(
                "invite_password:{}:{}",
                command_identity(&self.username),
                self.password.as_deref().unwrap_or_default()
            )
        }
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self.seq, other.seq) {
            (Some(left), Some(right)) => left == right,
            (None, None) => {
                command_identity(&self.username) == command_identity(&other.username)
                    && self.password == other.password
            }
            _ => false,
        }
    }
}

fn invite_arg_trailing_is_empty(value: &str) -> bool {
    value
        .trim_start_matches([' ', '\t'])
        .trim_end_matches([']', '】'])
        .trim()
        .is_empty()
}

fn parse_invite_arg(digits: &str) -> Option<(Option<u32>, Option<String>, String)> {
    match digits.len() {
        1..=3 => {
            let seq = digits.parse::<u32>().ok()?;
            (seq != 0).then(|| (Some(seq), None, seq.to_string()))
        }
        6 => Some((None, Some(digits.to_string()), "6位密码".to_string())),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteDecision {
    Approve,
    Reject,
    Timeout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteRequest {
    pub username: String,
    pub sequence: Option<u32>,
    pub password: Option<String>,
}

impl InviteRequest {
    pub fn new(
        username: impl Into<String>,
        sequence: Option<u32>,
        password: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            sequence,
            password,
        }
    }
}

pub enum InviteStart {
    Duplicate { sequence: u32 },
    Ready(InviteExecution),
}

pub struct InviteExecution {
    username: String,
    password: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteUiOutcome {
    entered: bool,
    notification_warning: Option<String>,
}

impl InviteUiOutcome {
    pub fn new(entered: bool, notification_warning: Option<String>) -> Self {
        Self {
            entered,
            notification_warning,
        }
    }

    pub fn entered(&self) -> bool {
        self.entered
    }

    pub fn notification_warning(&self) -> Option<&str> {
        self.notification_warning.as_deref()
    }
}

pub trait InviteExecutionPort {
    fn is_public_hall(&self) -> Result<bool>;
    fn notify_friend(&self, username: &str, message: &str) -> bool;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn wait_for_decision(&self) -> Result<InviteDecision>;
    fn execute_invite_ui(
        &self,
        username: &str,
        password: Option<&str>,
        notification: &str,
    ) -> Result<InviteUiOutcome>;
}

#[derive(Default)]
pub struct InviteService {
    executed_sequences: HashSet<u32>,
}

impl InviteService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn should_accept(&self, sequence: Option<u32>) -> bool {
        let Some(sequence) = sequence else {
            return true;
        };
        !self.executed_sequences.contains(&sequence)
    }

    pub fn begin(&mut self, request: InviteRequest) -> InviteStart {
        if let Some(sequence) = request.sequence
            && !self.executed_sequences.insert(sequence)
        {
            return InviteStart::Duplicate { sequence };
        }
        InviteStart::Ready(InviteExecution {
            username: request.username,
            password: request.password,
        })
    }
}

impl InviteExecution {
    pub fn run(self, port: &dyn InviteExecutionPort) -> Result<bool> {
        let username = self.username;
        let password = self.password;
        log::info!("邀请: 先检测是否公共大厅");
        if port.is_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            return execute_approved_invite(
                port,
                &username,
                password.as_deref(),
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
            );
        }
        let announce = format!(
            "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
            username
        );
        if let Err(error) = port.send_hall(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return execute_approved_invite(
                port,
                &username,
                password.as_deref(),
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
            );
        }
        match port.wait_for_decision()? {
            InviteDecision::Approve => execute_approved_invite(
                port,
                &username,
                password.as_deref(),
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
            ),
            InviteDecision::Timeout => execute_approved_invite(
                port,
                &username,
                password.as_deref(),
                "已默认同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
            ),
            InviteDecision::Reject => {
                log::info!("收到邀请拒绝，取消邀请");
                port.notify_friend(&username, "大厅成员已拒绝邀请");
                Ok(false)
            }
        }
    }
}

fn execute_approved_invite(
    port: &dyn InviteExecutionPort,
    username: &str,
    password: Option<&str>,
    notification: &str,
) -> Result<bool> {
    let outcome = port.execute_invite_ui(username, password, notification)?;
    if outcome.entered()
        && let Some(warning) = outcome.notification_warning()
    {
        let message = format!("邀请已完成，但好友通知失败：{warning}");
        if let Err(error) = port.send_hall(&message) {
            log::error!("邀请通知警告发送失败: {error:#}");
        }
    }
    Ok(outcome.entered())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    struct FakeInvitePort {
        public_hall: bool,
        decision: InviteDecision,
        notifications: StdMutex<Vec<String>>,
        runs: StdMutex<Vec<String>>,
    }

    impl FakeInvitePort {
        fn new(public_hall: bool, decision: InviteDecision) -> Self {
            Self {
                public_hall,
                decision,
                notifications: StdMutex::new(Vec::new()),
                runs: StdMutex::new(Vec::new()),
            }
        }
    }

    impl InviteExecutionPort for FakeInvitePort {
        fn is_public_hall(&self) -> Result<bool> {
            Ok(self.public_hall)
        }

        fn notify_friend(&self, _username: &str, message: &str) -> bool {
            self.notifications.lock().unwrap().push(message.to_string());
            true
        }

        fn send_hall(&self, _message: &str) -> Result<()> {
            Ok(())
        }

        fn wait_for_decision(&self) -> Result<InviteDecision> {
            Ok(self.decision)
        }

        fn execute_invite_ui(
            &self,
            _username: &str,
            _password: Option<&str>,
            notification: &str,
        ) -> Result<InviteUiOutcome> {
            self.runs.lock().unwrap().push(notification.to_string());
            Ok(InviteUiOutcome::new(true, None))
        }
    }

    fn begin_ready(
        service: &mut InviteService,
        username: &str,
        sequence: Option<u32>,
    ) -> InviteExecution {
        let InviteStart::Ready(execution) =
            service.begin(InviteRequest::new(username, sequence, None))
        else {
            panic!("invite should be ready");
        };
        execution
    }

    #[test]
    fn execution_ledger_accepts_each_sequence_once() {
        let mut service = InviteService::new();

        assert!(service.should_accept(Some(7)));
        drop(begin_ready(&mut service, "甲", Some(7)));
        assert!(!service.should_accept(Some(7)));
        assert!(matches!(
            service.begin(InviteRequest::new("乙", Some(7), None)),
            InviteStart::Duplicate { sequence: 7 }
        ));
    }

    #[test]
    fn unsequenced_invites_can_start_repeatedly() {
        let mut service = InviteService::new();

        drop(begin_ready(&mut service, "甲", None));
        drop(begin_ready(&mut service, "甲", None));

        assert!(service.should_accept(None));
    }

    #[test]
    fn public_hall_runs_notification_inside_the_invite_ui_transaction() {
        let mut service = InviteService::new();
        let port = FakeInvitePort::new(true, InviteDecision::Reject);

        assert!(begin_ready(&mut service, "甲", None).run(&port).unwrap());

        assert!(port.runs.lock().unwrap()[0].starts_with("已同意"));
        assert!(
            port.notifications.lock().unwrap().is_empty(),
            "the approval notification must be sent inside the invite UI transaction"
        );
    }

    #[test]
    fn rejected_invite_notifies_friend_without_running_ui() {
        let mut service = InviteService::new();
        let port = FakeInvitePort::new(false, InviteDecision::Reject);

        assert!(!begin_ready(&mut service, "甲", None).run(&port).unwrap());

        assert!(port.runs.lock().unwrap().is_empty());
        assert_eq!(
            *port.notifications.lock().unwrap(),
            vec!["大厅成员已拒绝邀请".to_string()]
        );
    }

    #[test]
    fn rejected_execution_does_not_release_its_sequence() {
        let mut service = InviteService::new();
        let port = FakeInvitePort::new(false, InviteDecision::Reject);

        assert!(
            !begin_ready(&mut service, "甲", Some(11))
                .run(&port)
                .unwrap()
        );

        assert!(!service.should_accept(Some(11)));
        assert!(matches!(
            service.begin(InviteRequest::new("甲", Some(11), None)),
            InviteStart::Duplicate { sequence: 11 }
        ));
    }
}
