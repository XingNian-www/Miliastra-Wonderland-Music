use std::fmt::{Display, Formatter};

use anyhow::Result;

const START_GAME_CONTEXT_LOSS_REASON: &str = "启动游戏任务将重建聊天上下文";
const START_GAME_RESCAN_REASON: &str = "启动游戏任务开始";
const ENTER_WONDERLAND_CONTEXT_LOSS_REASON: &str = "进入千星任务将切换大厅";
const ENTER_WONDERLAND_RESCAN_REASON: &str = "进入千星任务开始";
const ENTER_WONDERLAND_COMPLETE_RESCAN_REASON: &str = "进入千星任务完成";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTaskKind {
    StartGame,
    EnterWonderland,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartupSource(&'static str);

impl StartupSource {
    pub const STARTUP_CONFIG: Self = Self("启动配置");
    pub const REMOTE_CONSOLE: Self = Self("远程指挥台");

    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }
}

impl From<&'static str> for StartupSource {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

impl Display for StartupSource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartupTask {
    kind: StartupTaskKind,
    source: StartupSource,
}

impl StartupTask {
    pub const fn new(kind: StartupTaskKind, source: StartupSource) -> Self {
        Self { kind, source }
    }

    pub fn start_game(source: impl Into<StartupSource>) -> Self {
        Self::new(StartupTaskKind::StartGame, source.into())
    }

    pub fn enter_wonderland(source: impl Into<StartupSource>) -> Self {
        Self::new(StartupTaskKind::EnterWonderland, source.into())
    }

    pub fn label(self) -> String {
        match self.kind {
            StartupTaskKind::StartGame => format!("启动游戏({})", self.source),
            StartupTaskKind::EnterWonderland => format!("进入千星({})", self.source),
        }
    }
}

pub trait StartupExecutionPort {
    fn invalidate_chat_context(&self, reason: &'static str);

    fn request_window_rescan(&self, reason: &'static str) -> Result<()>;

    fn run_start_game(&self, on_window_detection_reset: &mut dyn FnMut(&'static str))
    -> Result<()>;

    fn run_enter_wonderland(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StartupService;

impl StartupService {
    pub const fn new() -> Self {
        Self
    }

    pub fn execute(&self, task: StartupTask, port: &dyn StartupExecutionPort) -> Result<()> {
        match task.kind {
            StartupTaskKind::StartGame => self.start_game(task.source, port),
            StartupTaskKind::EnterWonderland => self.enter_wonderland(task.source, port),
        }
    }

    fn start_game(&self, source: StartupSource, port: &dyn StartupExecutionPort) -> Result<()> {
        log::info!("执行启动游戏任务: {}", source);
        port.invalidate_chat_context(START_GAME_CONTEXT_LOSS_REASON);
        port.request_window_rescan(START_GAME_RESCAN_REASON)?;

        let mut reset_window_detection = |reason| {
            if let Err(error) = port.request_window_rescan(reason) {
                log::error!("请求重置窗口检测退避失败: {error:#}");
            }
        };
        port.run_start_game(&mut reset_window_detection)
    }

    fn enter_wonderland(
        &self,
        source: StartupSource,
        port: &dyn StartupExecutionPort,
    ) -> Result<()> {
        log::info!("执行进入千星任务: {}", source);
        port.invalidate_chat_context(ENTER_WONDERLAND_CONTEXT_LOSS_REASON);
        port.request_window_rescan(ENTER_WONDERLAND_RESCAN_REASON)?;
        port.run_enter_wonderland()?;

        port.request_window_rescan(ENTER_WONDERLAND_COMPLETE_RESCAN_REASON)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::{Result, bail};

    use super::*;

    struct RecordingPort {
        events: RefCell<Vec<String>>,
        fail_rescan: Option<&'static str>,
        start_progress: Vec<&'static str>,
        fail_start: bool,
        fail_enter: bool,
    }

    impl Default for RecordingPort {
        fn default() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                fail_rescan: None,
                start_progress: Vec::new(),
                fail_start: false,
                fail_enter: false,
            }
        }
    }

    impl RecordingPort {
        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }

        fn record(&self, event: impl Into<String>) {
            self.events.borrow_mut().push(event.into());
        }
    }

    impl StartupExecutionPort for RecordingPort {
        fn invalidate_chat_context(&self, reason: &'static str) {
            self.record(format!("invalidate:{reason}"));
        }

        fn request_window_rescan(&self, reason: &'static str) -> Result<()> {
            self.record(format!("rescan:{reason}"));
            if self.fail_rescan == Some(reason) {
                bail!("rescan failed: {reason}");
            }
            Ok(())
        }

        fn run_start_game(
            &self,
            on_window_detection_reset: &mut dyn FnMut(&'static str),
        ) -> Result<()> {
            self.record("run:start-game");
            for reason in &self.start_progress {
                on_window_detection_reset(reason);
            }
            if self.fail_start {
                bail!("start game failed");
            }
            Ok(())
        }

        fn run_enter_wonderland(&self) -> Result<()> {
            self.record("run:enter-wonderland");
            if self.fail_enter {
                bail!("enter wonderland failed");
            }
            Ok(())
        }
    }

    #[test]
    fn tasks_keep_existing_labels_and_sources() {
        let start = StartupTask::start_game(StartupSource::STARTUP_CONFIG);
        let enter = StartupTask::enter_wonderland("远程指挥台");

        assert_eq!(start.label(), "启动游戏(启动配置)");
        assert_eq!(enter.label(), "进入千星(远程指挥台)");
    }

    #[test]
    fn start_game_preserves_context_rescan_and_progress_order() {
        let port = RecordingPort {
            start_progress: vec!["已创建游戏进程", "已检测到游戏窗口"],
            ..RecordingPort::default()
        };

        StartupService::new()
            .execute(
                StartupTask::start_game(StartupSource::REMOTE_CONSOLE),
                &port,
            )
            .unwrap();

        assert_eq!(
            port.events(),
            [
                "invalidate:启动游戏任务将重建聊天上下文",
                "rescan:启动游戏任务开始",
                "run:start-game",
                "rescan:已创建游戏进程",
                "rescan:已检测到游戏窗口",
            ]
        );
    }

    #[test]
    fn start_game_does_not_run_when_initial_rescan_fails() {
        let port = RecordingPort {
            fail_rescan: Some(START_GAME_RESCAN_REASON),
            ..RecordingPort::default()
        };

        let error = StartupService::new()
            .execute(StartupTask::start_game("测试"), &port)
            .unwrap_err();

        assert!(error.to_string().contains("rescan failed"));
        assert_eq!(
            port.events(),
            [
                "invalidate:启动游戏任务将重建聊天上下文",
                "rescan:启动游戏任务开始",
            ]
        );
    }

    #[test]
    fn start_game_ignores_progress_rescan_failures() {
        const PROGRESS: &str = "启动游戏流程已检测到一级界面";
        let port = RecordingPort {
            fail_rescan: Some(PROGRESS),
            start_progress: vec![PROGRESS],
            ..RecordingPort::default()
        };

        StartupService::new()
            .execute(StartupTask::start_game("测试"), &port)
            .unwrap();

        assert_eq!(
            port.events(),
            [
                "invalidate:启动游戏任务将重建聊天上下文",
                "rescan:启动游戏任务开始",
                "run:start-game",
                "rescan:启动游戏流程已检测到一级界面",
            ]
        );
    }

    #[test]
    fn start_game_propagates_ui_failure() {
        let port = RecordingPort {
            fail_start: true,
            ..RecordingPort::default()
        };

        let error = StartupService::new()
            .execute(StartupTask::start_game("测试"), &port)
            .unwrap_err();

        assert_eq!(error.to_string(), "start game failed");
        assert_eq!(
            port.events(),
            [
                "invalidate:启动游戏任务将重建聊天上下文",
                "rescan:启动游戏任务开始",
                "run:start-game",
            ]
        );
    }

    #[test]
    fn enter_wonderland_preserves_success_order() {
        let port = RecordingPort::default();

        StartupService::new()
            .execute(StartupTask::enter_wonderland("测试"), &port)
            .unwrap();

        assert_eq!(
            port.events(),
            [
                "invalidate:进入千星任务将切换大厅",
                "rescan:进入千星任务开始",
                "run:enter-wonderland",
                "rescan:进入千星任务完成",
            ]
        );
    }

    #[test]
    fn enter_wonderland_failure_skips_primary_return_and_completion_rescan() {
        let port = RecordingPort {
            fail_enter: true,
            ..RecordingPort::default()
        };

        let error = StartupService::new()
            .execute(StartupTask::enter_wonderland("测试"), &port)
            .unwrap_err();

        assert_eq!(error.to_string(), "enter wonderland failed");
        assert_eq!(
            port.events(),
            [
                "invalidate:进入千星任务将切换大厅",
                "rescan:进入千星任务开始",
                "run:enter-wonderland",
            ]
        );
    }

    #[test]
    fn enter_wonderland_propagates_completion_rescan_failure_after_return() {
        let port = RecordingPort {
            fail_rescan: Some(ENTER_WONDERLAND_COMPLETE_RESCAN_REASON),
            ..RecordingPort::default()
        };

        let error = StartupService::new()
            .execute(StartupTask::enter_wonderland("测试"), &port)
            .unwrap_err();

        assert!(error.to_string().contains("rescan failed"));
        assert_eq!(
            port.events(),
            [
                "invalidate:进入千星任务将切换大厅",
                "rescan:进入千星任务开始",
                "run:enter-wonderland",
                "rescan:进入千星任务完成",
            ]
        );
    }
}
