use anyhow::{Context, Result, anyhow};

use super::ApplicationRuntime;
use crate::features::startup::StartupExecutionPort;
use crate::ui::routines::{
    EnterGame, EnterGameEffect, EnterWonderland, EnterWonderlandEffect, UiResidencyOutcome,
};

impl StartupExecutionPort for ApplicationRuntime {
    fn invalidate_chat_context(&self, reason: &'static str) {
        self.abort_entertainment_for_context_loss(reason);
    }

    fn request_window_rescan(&self, reason: &'static str) -> Result<()> {
        self.window_detection_signal.request(reason)
    }

    fn run_start_game(
        &self,
        on_window_detection_reset: &mut dyn FnMut(&'static str),
    ) -> Result<()> {
        let outcome = self
            .startup_ui
            .submit_enter_game(EnterGame)
            .context("提交进入游戏 UI 事务")?
            .wait()
            .context("等待进入游戏 UI 事务")?;
        match outcome.effect() {
            EnterGameEffect::WindowReady | EnterGameEffect::Entered => {}
            EnterGameEffect::Failed(failure) => return Err(anyhow!(failure.to_string())),
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            log::warn!("启动游戏目标已完成，但一级驻留未确认: {failure}");
        }
        on_window_detection_reset("启动游戏 UI 事务已完成");
        Ok(())
    }

    fn run_enter_wonderland(&self) -> Result<()> {
        let outcome = self
            .startup_ui
            .submit_enter_wonderland(EnterWonderland)
            .context("提交进入千星 UI 事务")?
            .wait()
            .context("等待进入千星 UI 事务")?;
        match outcome.effect() {
            EnterWonderlandEffect::Entered => {}
            EnterWonderlandEffect::Failed(failure) => return Err(anyhow!(failure.to_string())),
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            return Err(anyhow!("进入千星已确认，但一级驻留恢复失败：{failure}"));
        }
        Ok(())
    }
}
