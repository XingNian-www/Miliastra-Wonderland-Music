use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteDecision {
    Approve,
    Reject,
    Timeout,
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

    pub fn was_executed(&self, sequence: u32) -> Result<bool> {
        Ok(self
            .executed_sequences
            .lock()
            .map_err(|_| anyhow!("invite execution ledger mutex poisoned"))?
            .contains(&sequence))
    }

    pub fn reserve_execution(&self, sequence: u32) -> Result<bool> {
        Ok(self
            .executed_sequences
            .lock()
            .map_err(|_| anyhow!("invite execution ledger mutex poisoned"))?
            .insert(sequence))
    }

    pub fn execute(
        &self,
        username: &str,
        password: Option<&str>,
        port: &dyn InviteExecutionPort,
    ) -> Result<bool> {
        log::info!("邀请: 先检测是否公共大厅");
        if port.is_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            let friend_chat_open = port.notify_friend(
                username,
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                true,
            );
            return port.run_invite_ui(username, password, friend_chat_open);
        }
        let announce = format!(
            "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
            username
        );
        if let Err(error) = port.send_hall(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return port.run_invite_ui(username, password, false);
        }
        match port.wait_for_decision()? {
            InviteDecision::Approve => {
                let friend_chat_open = port.notify_friend(
                    username,
                    "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                port.run_invite_ui(username, password, friend_chat_open)
            }
            InviteDecision::Timeout => {
                let friend_chat_open = port.notify_friend(
                    username,
                    "已默认同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                port.run_invite_ui(username, password, friend_chat_open)
            }
            InviteDecision::Reject => {
                log::info!("收到邀请拒绝，取消邀请");
                port.notify_friend(username, "大厅成员已拒绝邀请", false);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

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

    #[test]
    fn execution_ledger_accepts_each_sequence_once() {
        let service = InviteService::new();

        assert!(!service.was_executed(7).unwrap());
        assert!(service.reserve_execution(7).unwrap());
        assert!(service.was_executed(7).unwrap());
        assert!(!service.reserve_execution(7).unwrap());
    }

    #[test]
    fn public_hall_skips_vote_and_keeps_verified_friend_chat_open() {
        let service = InviteService::new();
        let port = FakeInvitePort::new(true, InviteDecision::Reject);

        assert!(service.execute("甲", None, &port).unwrap());

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

        assert!(!service.execute("甲", None, &port).unwrap());

        assert!(port.runs.lock().unwrap().is_empty());
        assert_eq!(
            *port.notifications.lock().unwrap(),
            vec![("大厅成员已拒绝邀请".to_string(), false)]
        );
    }
}
