use std::collections::VecDeque;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::RectConfig;
use crate::config::validate_rect;
use crate::features::chat_text::{command_identity, strip_ascii_case_prefix};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteConfig {
    pub friend_name_stable_count: u32,
    /// 所有列表页精确匹配失败后，是否回到顶部启用候选评分匹配。
    #[serde(default)]
    pub fuzzy_friend_match: bool,
    pub friend_list_region: RectConfig,
    pub friend_chat_region: RectConfig,
    pub view_star_region: RectConfig,
    pub goto_hall_region: RectConfig,
    pub enter_hall_region: RectConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteTimingConfig {
    pub open_chat_ms: u64,
    pub step_ms: u64,
    pub confirm_timeout_ms: u64,
    pub confirm_poll_ms: u64,
}

impl Default for InviteTimingConfig {
    fn default() -> Self {
        Self {
            open_chat_ms: 400,
            step_ms: 800,
            confirm_timeout_ms: 30000,
            confirm_poll_ms: 2000,
        }
    }
}

impl InviteConfig {
    pub(crate) fn validate(&self, timing: &InviteTimingConfig) -> Result<()> {
        for (rect, field) in [
            (self.friend_list_region, "invite.friend_list_region"),
            (self.friend_chat_region, "invite.friend_chat_region"),
            (self.view_star_region, "invite.view_star_region"),
            (self.goto_hall_region, "invite.goto_hall_region"),
            (self.enter_hall_region, "invite.enter_hall_region"),
        ] {
            validate_rect(rect, field)?;
        }
        if timing.confirm_timeout_ms == 0 || timing.confirm_poll_ms == 0 {
            bail!("邀请确认超时和轮询间隔必须大于 0");
        }
        if timing.open_chat_ms == 0 || timing.step_ms == 0 {
            bail!("timing.invite.open_chat_ms 和 timing.invite.step_ms 必须大于 0");
        }
        Ok(())
    }
}

/// 邀请命令：读取邀请序号或六位密码，并发起入厅确认流程。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InviteCommand {
    /// 命令发起人（信封主语）。外部聊天源下这是**纯 UID**，不能拿去搜好友列表；
    /// 它仍参与权限判定与锁键，所以保持原样、不在这里替换。
    pub(crate) username: String,
    /// 好友列表里真正可以匹配的名字（备注 + 游戏内昵称），由 composition 层用 UID 反查填入。
    ///
    /// 为空时调用方回退到 `username`：OCR 路径的信封主语本来就是列表上看得见的昵称，
    /// 那时反查不到账号也不该让邀请失效。
    #[serde(default)]
    pub(crate) search_names: Vec<String>,
    /// 身份映射后的备注昵称，用于大厅通告和好友通知。
    pub(crate) display_name: String,
    pub(crate) seq: Option<u32>,
    pub(crate) password: Option<String>,
}

pub(crate) struct InviteCommandMatch {
    pub(crate) raw_parameter: String,
    pub(crate) command: InviteCommand,
}

impl InviteCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::At
            && envelope.authority() == CommandAuthority::Friend
            && strip_ascii_case_prefix(envelope.command_text(), "邀请").is_some()
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        let parsed = Self::parse_friend(envelope.command_text(), envelope.username())?;
        Some(FeatureCommandMatch::new(
            "邀请",
            format!("邀请 {} {}", envelope.username(), parsed.raw_parameter),
            parsed.command,
        ))
    }

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
                // 解析阶段只认识信封主语；能用 UID 反查出哪些名字，
                // 依赖 IdentityAccess，只有 composition 层拿得到。
                search_names: Vec::new(),
                display_name: username.to_string(),
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
    /// 好友列表可匹配名；空则回退到 `username`。
    pub search_names: Vec<String>,
    pub display_name: String,
    pub sequence: Option<u32>,
    pub password: Option<String>,
}

impl InviteRequest {
    pub fn new(
        username: impl Into<String>,
        display_name: impl Into<String>,
        sequence: Option<u32>,
        password: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            search_names: Vec::new(),
            display_name: display_name.into(),
            sequence,
            password,
        }
    }

    /// 实际拿去搜好友列表的名字：优先用 UID 反查出来的候选，没有再退回主语。
    pub fn recipients(&self) -> Vec<String> {
        if self.search_names.is_empty() {
            vec![self.username.clone()]
        } else {
            self.search_names.clone()
        }
    }
}

pub enum InviteStart {
    Duplicate { sequence: u32 },
    Ready(InviteExecution),
}

pub struct InviteExecution {
    username: String,
    /// 好友列表可匹配名（UID 反查得到），空则回退到 `username`。
    recipients: Vec<String>,
    display_name: String,
    password: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteUiOutcome {
    entered: bool,
    residency_failure: Option<String>,
    notification_warning: Option<String>,
}

impl InviteUiOutcome {
    pub fn new(
        entered: bool,
        residency_failure: Option<String>,
        notification_warning: Option<String>,
    ) -> Self {
        Self {
            entered,
            residency_failure,
            notification_warning,
        }
    }

    pub fn entered(&self) -> bool {
        self.entered
    }

    pub fn notification_warning(&self) -> Option<&str> {
        self.notification_warning.as_deref()
    }

    pub fn residency_failure(&self) -> Option<&str> {
        self.residency_failure.as_deref()
    }
}

pub trait InviteExecutionPort {
    fn is_public_hall(&self) -> Result<bool>;
    /// 拒绝邀请后给发起人发一条说明。
    ///
    /// 参数是**候选名**而不是 username：外部源下 username 是纯 UID，
    /// 拿去搜好友列表必然落空，拒绝通知会永远发不出去。
    fn notify_friend(&self, recipients: &[String], message: &str) -> bool;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn wait_for_decision(&self) -> Result<InviteDecision>;
    fn execute_invite_ui(
        &self,
        recipients: &[String],
        password: Option<&str>,
        notification: &str,
    ) -> Result<InviteUiOutcome>;
}

const EXECUTED_SEQUENCE_LIMIT: usize = 5;

#[derive(Default)]
pub struct InviteService {
    executed_sequences: VecDeque<u32>,
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
        if let Some(sequence) = request.sequence {
            if self.executed_sequences.contains(&sequence) {
                return InviteStart::Duplicate { sequence };
            }
            self.executed_sequences.push_back(sequence);
            if self.executed_sequences.len() > EXECUTED_SEQUENCE_LIMIT {
                self.executed_sequences.pop_front();
            }
        }
        let recipients = request.recipients();
        InviteStart::Ready(InviteExecution {
            username: request.username,
            recipients,
            display_name: request.display_name,
            password: request.password,
        })
    }
}

impl InviteExecution {
    pub fn run(self, port: &dyn InviteExecutionPort) -> Result<bool> {
        let username = self.username;
        let recipients = self.recipients;
        let display_name = self.display_name;
        let password = self.password;
        log::info!("邀请: 先检测是否公共大厅");
        if port.is_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            return execute_approved_invite(
                port,
                &recipients,
                password.as_deref(),
                crate::features::reply_template::moderation::MODERATION[2].default,
            );
        }
        let announce = crate::features::reply_template::render(
            &crate::features::reply_template::moderation::MODERATION[4],
            None,
            &crate::features::reply_template::ReplyVars::new().with("玩家", display_name.clone()),
        )
        .text;
        if let Err(error) = port.send_hall(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return execute_approved_invite(
                port,
                &recipients,
                password.as_deref(),
                crate::features::reply_template::moderation::MODERATION[2].default,
            );
        }
        match port.wait_for_decision()? {
            InviteDecision::Approve => execute_approved_invite(
                port,
                &recipients,
                password.as_deref(),
                crate::features::reply_template::moderation::MODERATION[2].default,
            ),
            InviteDecision::Timeout => execute_approved_invite(
                port,
                &recipients,
                password.as_deref(),
                "已默认同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
            ),
            InviteDecision::Reject => {
                log::info!("收到邀请拒绝，取消邀请");
                port.notify_friend(&recipients, "大厅成员已拒绝邀请");
                Ok(false)
            }
        }
    }
}

fn execute_approved_invite(
    port: &dyn InviteExecutionPort,
    recipients: &[String],
    password: Option<&str>,
    notification: &str,
) -> Result<bool> {
    let outcome = port.execute_invite_ui(recipients, password, notification)?;
    if let Some(failure) = outcome.residency_failure() {
        bail!("邀请 UI 事务未能恢复值守驻留: {failure}");
    }
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
        residency_failure: Option<String>,
        notifications: StdMutex<Vec<String>>,
        runs: StdMutex<Vec<String>>,
    }

    impl FakeInvitePort {
        fn new(public_hall: bool, decision: InviteDecision) -> Self {
            Self {
                public_hall,
                decision,
                residency_failure: None,
                notifications: StdMutex::new(Vec::new()),
                runs: StdMutex::new(Vec::new()),
            }
        }
    }

    impl InviteExecutionPort for FakeInvitePort {
        fn is_public_hall(&self) -> Result<bool> {
            Ok(self.public_hall)
        }

        fn notify_friend(&self, _recipients: &[String], message: &str) -> bool {
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
            _recipients: &[String],
            _password: Option<&str>,
            notification: &str,
        ) -> Result<InviteUiOutcome> {
            self.runs.lock().unwrap().push(notification.to_string());
            Ok(InviteUiOutcome::new(
                true,
                self.residency_failure.clone(),
                None,
            ))
        }
    }

    fn begin_ready(
        service: &mut InviteService,
        username: &str,
        sequence: Option<u32>,
    ) -> InviteExecution {
        let InviteStart::Ready(execution) =
            service.begin(InviteRequest::new(username, username, sequence, None))
        else {
            panic!("invite should be ready");
        };
        execution
    }

    #[test]
    fn execution_ledger_accepts_each_sequence_once_within_the_recent_window() {
        let mut service = InviteService::new();

        assert!(service.should_accept(Some(7)));
        drop(begin_ready(&mut service, "甲", Some(7)));
        assert!(!service.should_accept(Some(7)));
        assert!(matches!(
            service.begin(InviteRequest::new("乙", "乙", Some(7), None)),
            InviteStart::Duplicate { sequence: 7 }
        ));
    }

    #[test]
    fn execution_ledger_releases_the_oldest_sequence_after_five_newer_entries() {
        let mut service = InviteService::new();

        for sequence in 1..=5 {
            drop(begin_ready(&mut service, "甲", Some(sequence)));
        }
        assert!(!service.should_accept(Some(1)));
        assert!(!service.should_accept(Some(5)));

        drop(begin_ready(&mut service, "甲", Some(6)));

        assert!(service.should_accept(Some(1)));
        for sequence in 2..=6 {
            assert!(!service.should_accept(Some(sequence)));
        }
    }

    #[test]
    fn duplicate_sequence_does_not_advance_the_recent_window() {
        let mut service = InviteService::new();

        for sequence in 1..=5 {
            drop(begin_ready(&mut service, "甲", Some(sequence)));
        }
        assert!(matches!(
            service.begin(InviteRequest::new("甲", "甲", Some(1), None)),
            InviteStart::Duplicate { sequence: 1 }
        ));

        drop(begin_ready(&mut service, "甲", Some(6)));

        assert!(service.should_accept(Some(1)));
        assert!(!service.should_accept(Some(2)));
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
    fn entered_invite_with_failed_residency_is_not_reported_as_complete() {
        let mut service = InviteService::new();
        let mut port = FakeInvitePort::new(true, InviteDecision::Approve);
        port.residency_failure = Some("secondary hall not confirmed".to_string());

        let error = begin_ready(&mut service, "甲", None)
            .run(&port)
            .unwrap_err();

        assert!(error.to_string().contains("未能恢复值守驻留"));
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
            service.begin(InviteRequest::new("甲", "甲", Some(11), None)),
            InviteStart::Duplicate { sequence: 11 }
        ));
    }
}
