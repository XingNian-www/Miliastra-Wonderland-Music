use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

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

pub trait InviteExecutionPort {
    fn is_public_hall(&self) -> Result<bool>;
    fn notify_friend(&self, username: &str, message: &str, keep_chat_open: bool) -> bool;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn wait_for_decision(&self) -> Result<InviteDecision>;
    fn run_invite_ui(
        &self,
        username: &str,
        password: Option<&str>,
        friend_chat_open: bool,
    ) -> Result<bool>;
}

#[derive(Clone, Default)]
pub struct InviteService {
    executed_sequences: Arc<Mutex<HashSet<u32>>>,
}

impl InviteService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn should_accept(&self, sequence: Option<u32>) -> Result<bool> {
        let Some(sequence) = sequence else {
            return Ok(true);
        };
        Ok(!self.executed_sequences()?.contains(&sequence))
    }

    pub fn begin(&self, request: InviteRequest) -> Result<InviteStart> {
        if let Some(sequence) = request.sequence
            && !self.executed_sequences()?.insert(sequence)
        {
            return Ok(InviteStart::Duplicate { sequence });
        }
        Ok(InviteStart::Ready(InviteExecution {
            username: request.username,
            password: request.password,
        }))
    }

    fn executed_sequences(&self) -> Result<std::sync::MutexGuard<'_, HashSet<u32>>> {
        self.executed_sequences
            .lock()
            .map_err(|_| anyhow!("invite execution ledger mutex poisoned"))
    }
}

impl InviteExecution {
    pub fn run(self, port: &dyn InviteExecutionPort) -> Result<bool> {
        let username = self.username;
        let password = self.password;
        log::info!("邀请: 先检测是否公共大厅");
        if port.is_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            let friend_chat_open = port.notify_friend(
                &username,
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                true,
            );
            return port.run_invite_ui(&username, password.as_deref(), friend_chat_open);
        }
        let announce = format!(
            "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
            username
        );
        if let Err(error) = port.send_hall(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return port.run_invite_ui(&username, password.as_deref(), false);
        }
        match port.wait_for_decision()? {
            InviteDecision::Approve => {
                let friend_chat_open = port.notify_friend(
                    &username,
                    "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                port.run_invite_ui(&username, password.as_deref(), friend_chat_open)
            }
            InviteDecision::Timeout => {
                let friend_chat_open = port.notify_friend(
                    &username,
                    "已默认同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                port.run_invite_ui(&username, password.as_deref(), friend_chat_open)
            }
            InviteDecision::Reject => {
                log::info!("收到邀请拒绝，取消邀请");
                port.notify_friend(&username, "大厅成员已拒绝邀请", false);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Barrier, Mutex as StdMutex};
    use std::thread;

    use super::*;

    struct FakeInvitePort {
        public_hall: bool,
        decision: InviteDecision,
        notifications: StdMutex<Vec<(String, bool)>>,
        runs: StdMutex<Vec<bool>>,
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

        fn notify_friend(&self, _username: &str, message: &str, keep_chat_open: bool) -> bool {
            self.notifications
                .lock()
                .unwrap()
                .push((message.to_string(), keep_chat_open));
            true
        }

        fn send_hall(&self, _message: &str) -> Result<()> {
            Ok(())
        }

        fn wait_for_decision(&self) -> Result<InviteDecision> {
            Ok(self.decision)
        }

        fn run_invite_ui(
            &self,
            _username: &str,
            _password: Option<&str>,
            friend_chat_open: bool,
        ) -> Result<bool> {
            self.runs.lock().unwrap().push(friend_chat_open);
            Ok(true)
        }
    }

    fn begin_ready(
        service: &InviteService,
        username: &str,
        sequence: Option<u32>,
    ) -> InviteExecution {
        let InviteStart::Ready(execution) = service
            .begin(InviteRequest::new(username, sequence, None))
            .unwrap()
        else {
            panic!("invite should be ready");
        };
        execution
    }

    #[test]
    fn execution_ledger_accepts_each_sequence_once() {
        let service = InviteService::new();

        assert!(service.should_accept(Some(7)).unwrap());
        drop(begin_ready(&service, "甲", Some(7)));
        assert!(!service.should_accept(Some(7)).unwrap());
        assert!(matches!(
            service
                .begin(InviteRequest::new("乙", Some(7), None))
                .unwrap(),
            InviteStart::Duplicate { sequence: 7 }
        ));
    }

    #[test]
    fn concurrent_sequence_reservation_has_one_winner() {
        let service = InviteService::new();
        let barrier = Arc::new(Barrier::new(3));
        let workers = ["甲", "乙"].map(|username| {
            let service = service.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                matches!(
                    service
                        .begin(InviteRequest::new(username, Some(9), None))
                        .unwrap(),
                    InviteStart::Ready(_)
                )
            })
        });
        barrier.wait();

        let ready = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|ready| *ready)
            .count();

        assert_eq!(ready, 1);
        assert!(!service.should_accept(Some(9)).unwrap());
    }

    #[test]
    fn unsequenced_invites_can_start_repeatedly() {
        let service = InviteService::new();

        drop(begin_ready(&service, "甲", None));
        drop(begin_ready(&service, "甲", None));

        assert!(service.should_accept(None).unwrap());
    }

    #[test]
    fn public_hall_skips_vote_and_keeps_verified_friend_chat_open() {
        let service = InviteService::new();
        let port = FakeInvitePort::new(true, InviteDecision::Reject);

        assert!(begin_ready(&service, "甲", None).run(&port).unwrap());

        assert_eq!(*port.runs.lock().unwrap(), vec![true]);
        let notifications = port.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].0.starts_with("已同意"));
        assert!(notifications[0].1);
    }

    #[test]
    fn rejected_invite_notifies_friend_without_running_ui() {
        let service = InviteService::new();
        let port = FakeInvitePort::new(false, InviteDecision::Reject);

        assert!(!begin_ready(&service, "甲", None).run(&port).unwrap());

        assert!(port.runs.lock().unwrap().is_empty());
        assert_eq!(
            *port.notifications.lock().unwrap(),
            vec![("大厅成员已拒绝邀请".to_string(), false)]
        );
    }

    #[test]
    fn rejected_execution_does_not_release_its_sequence() {
        let service = InviteService::new();
        let port = FakeInvitePort::new(false, InviteDecision::Reject);

        assert!(!begin_ready(&service, "甲", Some(11)).run(&port).unwrap());

        assert!(!service.should_accept(Some(11)).unwrap());
        assert!(matches!(
            service
                .begin(InviteRequest::new("甲", Some(11), None))
                .unwrap(),
            InviteStart::Duplicate { sequence: 11 }
        ));
    }
}
