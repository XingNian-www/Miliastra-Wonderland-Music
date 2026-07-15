use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;

use anyhow::Result;

use super::{AutomationApp, game_startup, startup_flow};
use crate::features::startup::StartupExecutionPort;

impl StartupExecutionPort for AutomationApp {
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
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        game_startup::start_game(
            &config,
            &self.game_ui,
            &self.ocr,
            || running.load(AtomicOrdering::SeqCst),
            on_window_detection_reset,
        )
    }

    fn run_enter_wonderland(&self) -> Result<()> {
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        startup_flow::enter_wonderland(&config, &self.game_ui, || {
            running.load(AtomicOrdering::SeqCst)
        })
    }

    fn return_to_primary(&self) -> bool {
        self.return_to_primary_fixed()
    }
}
