use super::*;

impl ApplicationRuntime {
    pub(super) fn reply(&self, message: &str) -> Result<()> {
        let prefixed;
        let message = if self
            .lifecycle
            .console_reply_context
            .load(AtomicOrdering::SeqCst)
            && !message.starts_with("[控制台]:")
        {
            prefixed = format!("[控制台]: {}", message);
            prefixed.as_str()
        } else {
            message
        };
        match self.active_ui_residency()? {
            UiResidency::Primary => self.ui.chat_output.send_for_command(message),
            UiResidency::SecondaryCurrentHall => self.ui.chat_output.send_current_chat(message),
        }
    }

    pub(super) fn reply_batch(&self, messages: &[&str], _delay_ms: u64) -> Result<()> {
        let delay_ms = self
            .lifecycle
            .live_configs
            .snapshot()
            .timing
            .command
            .help_batch_ms;
        match self.active_ui_residency()? {
            UiResidency::Primary => self
                .ui
                .chat_output
                .send_batch_for_command(messages, delay_ms),
            UiResidency::SecondaryCurrentHall => self
                .ui
                .chat_output
                .send_current_chat_batch(messages, delay_ms),
        }
    }

    pub(super) fn update_monitor_playback_controller(&self) {
        self.lifecycle
            .monitor
            .publish(MonitorEvent::PlaybackController(
                self.playback.player.snapshot(),
            ));
    }

    pub(super) fn update_monitor_operational_state(&self) {
        self.lifecycle.monitor.publish(MonitorEvent::ScannerPaused(
            self.lifecycle.paused.load(AtomicOrdering::SeqCst),
        ));
    }
}

impl CardGameDeliveryPort for ApplicationRuntime {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_unique_friend_message(player, message)
    }

    fn send_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_friend_message(player, message)
    }

    fn send_friend_batch(
        &self,
        deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        let messages = deliveries
            .iter()
            .map(|delivery| FriendMessage::new(&delivery.player, &delivery.message))
            .collect::<Vec<_>>();
        self.send_friend_delivery_batch(&messages)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }
}

impl UndercoverDeliveryPort for ApplicationRuntime {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_stable_unique_friend_message(player, message)
    }

    fn send_friend_batch(&self, deliveries: &[FriendMessage]) -> Result<FriendBatchOutcome> {
        self.send_friend_delivery_batch(deliveries)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn send_hall_batch(&self, messages: &[String]) -> Result<()> {
        let refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        let help_batch_ms = self
            .lifecycle
            .live_configs
            .snapshot()
            .timing
            .command
            .help_batch_ms;
        match self.active_ui_residency()? {
            UiResidency::Primary => self
                .ui
                .chat_output
                .send_batch_for_command_redacted(&refs, help_batch_ms),
            UiResidency::SecondaryCurrentHall => self
                .ui
                .chat_output
                .send_current_chat_batch_redacted(&refs, help_batch_ms),
        }
    }
}
