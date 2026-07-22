use super::*;

use crate::features::administration::{
    AdministrationApplicationPort, AdministrationCommandContext, AdministrationImmediatePort,
};
use crate::features::hall::{
    HallApplicationPort, HallCommandContext, HallDetectionPort, HallMaintenancePort,
    HallObservation, HallStatePatch,
};
use crate::features::idiom_chain::{
    IdiomChainDeferredPort, IdiomChainExplanationPort, IdiomChainOutcome, IdiomDeliveryOutcome,
};
use crate::features::turtle_soup::{
    QuestionSubmitOutcome, TurtleSoupApplicationPort, TurtleSoupCommandOutcome,
};

pub(super) struct ImmediateAdministrationPort {
    business: BusinessRuntimeHandle,
    monitor: MonitorShared,
    executed_commands_log_path: std::path::PathBuf,
}

pub(super) struct DeferredIdiomChainPort {
    business: BusinessRuntimeHandle,
}

struct DeferredCardGamePort {
    business: BusinessRuntimeHandle,
}

struct TurtleSoupCommandPort {
    business: BusinessRuntimeHandle,
}

struct PublicHallDetectionPort {
    hall_ui: HallUi,
    business: BusinessRuntimeHandle,
}

impl HallDetectionPort for PublicHallDetectionPort {
    fn detect_public_hall(&mut self) -> Result<Option<HallObservation>> {
        detect_public_hall(&self.hall_ui)
    }

    fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        self.business
            .update_hall_remaining_minutes(minutes)
            .map_err(anyhow::Error::from)
    }

    fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
        self.business
            .clear_hall_remaining_minutes()
            .map_err(anyhow::Error::from)
    }
}

impl IdiomChainDeferredPort for DeferredIdiomChainPort {
    fn handle_command(
        &mut self,
        player: &str,
        command: &idiom_chain::IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome> {
        self.business
            .handle_idiom_chain_at(player, command, observed_at)
            .map_err(anyhow::Error::from)
    }

    fn send_deferred(&mut self, message: String) -> Result<IdiomDeliveryOutcome> {
        let snapshot = self.business.chat_listener_snapshot()?;
        let target = match listener_residency(snapshot.mode, snapshot.temporary_primary) {
            UiResidency::Primary => DeferredChatTarget::Primary,
            UiResidency::SecondaryCurrentHall => DeferredChatTarget::SecondaryCurrentHall,
        };
        Ok(
            match self.business.enqueue_deferred_chat(DeferredChatMessage {
                text: message,
                target,
            })? {
                EnqueueOutcome::Added => IdiomDeliveryOutcome::Added,
                EnqueueOutcome::DroppedMessage => IdiomDeliveryOutcome::DroppedEarlierMessage,
                EnqueueOutcome::Rejected => IdiomDeliveryOutcome::Rejected,
            },
        )
    }
}

impl CardGameDeliveryPort for DeferredCardGamePort {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能执行好友验证"))
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能发送好友消息"))
    }

    fn send_friend_batch(
        &self,
        _deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        Err(anyhow!("延迟牌类端口不能发送好友批次"))
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.business, message)
    }
}

impl TurtleSoupApplicationPort for TurtleSoupCommandPort {
    fn handle_hall_command(
        &mut self,
        player: &str,
        command: &turtle_soup::TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome> {
        self.business
            .handle_turtle_soup_hall_command(player, command)
            .map_err(anyhow::Error::from)
    }

    fn handle_friend_command(
        &mut self,
        player: &str,
        command: &turtle_soup::TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome> {
        self.business
            .handle_turtle_soup_friend_command(player, command)
            .map_err(anyhow::Error::from)
    }

    fn submit_question(
        &mut self,
        question: turtle_soup::TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<QuestionSubmitOutcome> {
        self.business
            .submit_turtle_soup_question_at(question, observed_at)
            .map_err(anyhow::Error::from)
    }

    fn send_current_hall(&mut self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.business, message)
    }
}

impl AdministrationImmediatePort for ImmediateAdministrationPort {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()> {
        self.business
            .set_commands_enabled(enabled)
            .map_err(anyhow::Error::from)
    }

    fn configure_idle_exit(&mut self, minutes: u32) -> Result<()> {
        configure_idle_exit(&self.business, minutes)
    }

    fn record_command_activity(&mut self) -> Result<()> {
        self.business
            .record_command_activity(Instant::now())
            .map_err(anyhow::Error::from)
    }

    fn log_executed(
        &mut self,
        context: &AdministrationCommandContext,
        final_command: &str,
    ) -> Result<()> {
        super::tasks::write_executed_command_fields(
            &self.monitor,
            &self.executed_commands_log_path,
            &context.message_type,
            &context.username,
            &context.user_command,
            final_command,
        )
    }
}

impl ApplicationRuntime {
    pub(super) fn deferred_idiom_chain_port(&self) -> DeferredIdiomChainPort {
        DeferredIdiomChainPort {
            business: self.business.clone(),
        }
    }

    fn turtle_soup_command_port(&self) -> TurtleSoupCommandPort {
        TurtleSoupCommandPort {
            business: self.business.clone(),
        }
    }

    pub(super) fn immediate_administration_port(&self) -> ImmediateAdministrationPort {
        ImmediateAdministrationPort {
            business: self.business.clone(),
            monitor: self.monitor.clone(),
            executed_commands_log_path: self.config.state.executed_commands_log_path.clone(),
        }
    }

    pub(super) fn maybe_warn_hall_expiring(&mut self) -> Result<bool> {
        let application = self.hall_application;
        application.maybe_warn_expiring(self)
    }

    pub(super) fn execute_command(&mut self, parsed: &RoutedCommand) -> Result<()> {
        match &parsed.command {
            ModuleCommand::SongRequest(command) => {
                let context = SongRequestContext {
                    message_type: parsed.message_type.clone(),
                    raw: parsed.raw.clone(),
                    username: command_username(parsed).to_string(),
                    user_command: parsed.user_command.clone(),
                };
                self.song_requests.clone().execute(&context, command, self)
            }
            ModuleCommand::Playback(command) => self.execute_playback_intent(parsed, command),
            ModuleCommand::Hall(command) => self.execute_hall_intent(parsed, command),
            ModuleCommand::Administration(command) => {
                self.execute_administration_intent(parsed, command)
            }
            ModuleCommand::IdiomChain(command) => self.execute_idiom_chain_intent(parsed, command),
            ModuleCommand::CardGame(command) => self.execute_card_game_intent(parsed, command),
            ModuleCommand::TurtleSoup(command) => self.execute_turtle_soup_intent(parsed, command),
            ModuleCommand::Undercover(command) => self.execute_undercover_intent(parsed, command),
            ModuleCommand::Invite(command) => self.execute_invite_intent(parsed, command),
            ModuleCommand::Moderation(command) => self.execute_moderation_intent(parsed, command),
            ModuleCommand::CustomWorkflow(command) => {
                self.execute_custom_workflow_intent(parsed, command)
            }
        }
    }

    fn execute_hall_intent(&mut self, parsed: &RoutedCommand, command: &HallCommand) -> Result<()> {
        let context = HallCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let application = self.hall_application;
        application.execute(&context, command, self)
    }

    fn execute_administration_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &AdministrationCommand,
    ) -> Result<()> {
        let context = AdministrationCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let application = self.administration_application;
        application.execute(&context, command, self)
    }

    fn execute_idiom_chain_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &idiom_chain::IdiomChainCommand,
    ) -> Result<()> {
        let observed_at = command_observed_at(parsed);
        if command.requires_executor() {
            let application = self.idiom_chain_application;
            application.execute_explanation(&parsed.username, command, observed_at, self)
        } else {
            log::debug!("成语接龙命令已按正式队列顺序处理，回复进入延迟聊天队列");
            let mut port = self.deferred_idiom_chain_port();
            self.idiom_chain_application.execute_deferred(
                &parsed.raw,
                &parsed.username,
                command,
                observed_at,
                &mut port,
            )
        }
    }

    fn execute_card_game_intent(
        &self,
        parsed: &RoutedCommand,
        command: &LandlordCommand,
    ) -> Result<()> {
        let observed_at = command_observed_at(parsed);
        match card_game_effect_lane(command) {
            CardGameEffectLane::Formal => self.card_games.execute_command(
                &parsed.username,
                command,
                observed_at,
                CardGameEffectLane::Formal,
                self,
            ),
            CardGameEffectLane::Deferred => {
                let port = DeferredCardGamePort {
                    business: self.business.clone(),
                };
                self.card_games.execute_command(
                    &parsed.username,
                    command,
                    observed_at,
                    CardGameEffectLane::Deferred,
                    &port,
                )
            }
        }
    }

    fn execute_turtle_soup_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &turtle_soup::TurtleSoupCommand,
    ) -> Result<()> {
        log::debug!("海龟汤控制命令已按正式队列顺序处理，回复进入延迟聊天队列");
        let mut port = self.turtle_soup_command_port();
        self.turtle_soup_application.execute_command(
            &parsed.raw,
            &parsed.username,
            parsed.message_type == "pink",
            command,
            &mut port,
        )
    }

    fn execute_undercover_intent(
        &self,
        parsed: &RoutedCommand,
        command: &UndercoverCommand,
    ) -> Result<()> {
        self.execute_undercover_command(parsed, command)
    }

    fn execute_invite_intent(
        &mut self,
        parsed: &RoutedCommand,
        invite: &crate::features::invite::InviteCommand,
    ) -> Result<()> {
        let request =
            InviteRequest::new(invite.username.clone(), invite.seq, invite.password.clone());
        let execution = match self.business.begin_invite(request)? {
            InviteStart::Duplicate { sequence } => {
                log::info!("邀请参数 {} 已执行过，跳过", sequence);
                return Ok(());
            }
            InviteStart::Ready(execution) => execution,
        };
        self.log_executed_command(parsed, &format!("invite {}", invite.username))?;
        execution.run(self).map(|_| ())
    }

    fn execute_moderation_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &crate::features::moderation::ModerationCommand,
    ) -> Result<()> {
        self.log_executed_command(
            parsed,
            &format!("{} uid {}", command.action.label(), command.uid),
        )?;
        self.execute_moderation_with_vote(command).map(|_| ())
    }

    fn execute_custom_workflow_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &crate::features::custom_workflow::CustomWorkflowCommand,
    ) -> Result<()> {
        self.log_executed_command(parsed, &format!("custom workflow {}", command.workflow))?;
        self.execute_custom_workflow(command, parsed)
    }

    pub(super) fn configure_idle_exit(&self, minutes: u32) -> Result<()> {
        configure_idle_exit(&self.business, minutes)
    }

    pub(super) fn commands_enabled(&self) -> Result<bool> {
        Ok(self
            .business
            .operational_snapshot(Instant::now())?
            .commands_enabled())
    }

    pub(super) fn check_public_hall(&self) -> Result<bool> {
        let mut port = PublicHallDetectionPort {
            hall_ui: self.hall_ui.clone(),
            business: self.business.clone(),
        };
        self.hall_application.check_public_hall(&mut port)
    }

    fn execute_undercover_command(
        &self,
        parsed: &RoutedCommand,
        command: &UndercoverCommand,
    ) -> Result<()> {
        let source = match parsed.message_type.as_str() {
            "pink" => UndercoverCommandSource::Friend,
            "控制台" => UndercoverCommandSource::Console,
            _ => UndercoverCommandSource::Hall,
        };
        self.undercover_game.execute_command(
            &parsed.username,
            source,
            command,
            command_observed_at(parsed),
            self,
        )
    }

    pub(super) fn execute_turtle_soup_question(
        &self,
        question: turtle_soup::TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<bool> {
        let mut port = self.turtle_soup_command_port();
        self.turtle_soup_application
            .submit_question(question, observed_at, &mut port)
    }
}

fn card_game_effect_lane(command: &LandlordCommand) -> CardGameEffectLane {
    if command.requires_executor() {
        CardGameEffectLane::Formal
    } else {
        CardGameEffectLane::Deferred
    }
}

fn configure_idle_exit(business: &BusinessRuntimeHandle, minutes: u32) -> Result<()> {
    business.configure_idle_exit(Duration::from_secs(u64::from(minutes) * 60), Instant::now())?;
    log::info!(
        "已设置闲置退出: {}分钟无新命令后暂停播放器、关闭目标游戏进程，软件主进程和播放器运行时继续运行",
        minutes
    );
    Ok(())
}

impl IdiomChainExplanationPort for ApplicationRuntime {
    fn explain(
        &mut self,
        player: &str,
        command: &idiom_chain::IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome> {
        self.business
            .explain_idiom_chain_at(player, command, observed_at)
            .map_err(anyhow::Error::from)
    }

    fn send_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()> {
        let message_refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&message_refs, delay_ms)
    }
}

impl AdministrationImmediatePort for ApplicationRuntime {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()> {
        self.business
            .set_commands_enabled(enabled)
            .map_err(anyhow::Error::from)
    }

    fn configure_idle_exit(&mut self, minutes: u32) -> Result<()> {
        ApplicationRuntime::configure_idle_exit(self, minutes)
    }

    fn record_command_activity(&mut self) -> Result<()> {
        ApplicationRuntime::record_command_activity(self, Instant::now())
    }

    fn log_executed(
        &mut self,
        context: &AdministrationCommandContext,
        final_command: &str,
    ) -> Result<()> {
        self.log_executed_command_fields(
            &context.message_type,
            &context.username,
            &context.user_command,
            final_command,
        )
    }
}

impl AdministrationApplicationPort for ApplicationRuntime {
    fn send_hall(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn send_hall_batch(&mut self, messages: &[&str], delay_ms: u64) -> Result<()> {
        self.reply_batch(messages, delay_ms)
    }
}

impl HallDetectionPort for ApplicationRuntime {
    fn detect_public_hall(&mut self) -> Result<Option<HallObservation>> {
        detect_public_hall(&self.hall_ui)
    }

    fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        self.business
            .update_hall_remaining_minutes(minutes)
            .map_err(anyhow::Error::from)
    }

    fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
        self.business
            .clear_hall_remaining_minutes()
            .map_err(anyhow::Error::from)
    }
}

impl HallApplicationPort for ApplicationRuntime {
    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn log_executed(&mut self, context: &HallCommandContext, final_command: &str) -> Result<()> {
        self.log_executed_command_fields(
            &context.message_type,
            &context.username,
            &context.user_command,
            final_command,
        )
    }

    fn read_hall_info(&mut self) -> Result<HallObservation> {
        let outcome = self
            .hall_ui
            .submit_read(ReadHallInfo)
            .context("提交大厅信息读取 UI 事务")?
            .wait()
            .context("等待大厅信息读取 UI 事务")?;
        let info = match outcome.effect() {
            ReadHallInfoEffect::Read(info) => info.clone(),
            ReadHallInfoEffect::Failed(failure) => {
                return Err(anyhow!("大厅信息读取失败：{failure}"));
            }
        };
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            return Err(anyhow!("大厅信息已读取，但一级驻留恢复失败：{failure}"));
        }
        Ok(HallObservation {
            name: info.name,
            remaining_minutes: info.remaining_minutes,
        })
    }

    fn toggle_microphone(&mut self) -> Result<()> {
        let outcome = self
            .hall_ui
            .submit_microphone(ToggleMicrophone)
            .context("提交麦克风切换 UI 事务")?
            .wait()
            .context("等待麦克风切换 UI 事务")?;
        if let ToggleMicrophoneEffect::Failed(failure) = outcome.effect() {
            return Err(anyhow!("麦克风切换失败：{failure}"));
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            return Err(anyhow!("麦克风已切换，但一级驻留恢复失败：{failure}"));
        }
        Ok(())
    }

    fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
        Ok(self.business.hall_state_snapshot()?.remaining_minutes_now())
    }
}

fn detect_public_hall(hall_ui: &HallUi) -> Result<Option<HallObservation>> {
    let outcome = hall_ui
        .submit_detect(DetectPublicHall)
        .context("提交公共大厅检测 UI 事务")?
        .wait()
        .context("等待公共大厅检测 UI 事务")?;
    let info = match outcome.effect() {
        DetectPublicHallEffect::Detected { is_public, info } => {
            log::debug!("公共大厅 UI 判断: {}", is_public);
            info.clone()
        }
        DetectPublicHallEffect::Failed(failure) => {
            log::error!("大厅检测 OCR 失败，按非公共大厅处理: {failure}");
            return Ok(None);
        }
    };
    if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
        return Err(anyhow!("大厅检测已完成，但一级驻留恢复失败：{failure}"));
    }
    Ok(Some(HallObservation {
        name: info.name,
        remaining_minutes: info.remaining_minutes,
    }))
}

impl HallMaintenancePort for ApplicationRuntime {
    fn executor_is_idle(&mut self) -> Result<bool> {
        ApplicationRuntime::executor_is_idle(self)
    }

    fn hall_expiring_warning_sent(&mut self) -> Result<bool> {
        Ok(self.business.hall_state_snapshot()?.expiring_warning_sent)
    }

    fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
        Ok(self.business.hall_state_snapshot()?.remaining_minutes_now())
    }

    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn mark_hall_expiring_warning_sent(&mut self) -> Result<()> {
        self.business
            .patch_hall_state(HallStatePatch {
                expiring_warning_sent: Some(true),
                ..HallStatePatch::default()
            })
            .map_err(anyhow::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_game_lane_matches_the_effects_required_by_each_command() {
        for command in [
            LandlordCommand::Start,
            LandlordCommand::RunFastStart,
            LandlordCommand::Join,
            LandlordCommand::Rob,
            LandlordCommand::Decline,
            LandlordCommand::Hand,
            LandlordCommand::Retry,
        ] {
            assert_eq!(
                card_game_effect_lane(&command),
                CardGameEffectLane::Formal,
                "command={command:?}"
            );
        }

        for command in [
            LandlordCommand::Status,
            LandlordCommand::Play("3".to_string()),
            LandlordCommand::Pass,
            LandlordCommand::Exit,
        ] {
            assert_eq!(
                card_game_effect_lane(&command),
                CardGameEffectLane::Deferred,
                "command={command:?}"
            );
        }
    }

    #[test]
    fn queued_command_uses_its_frame_capture_time() {
        let captured_at = Instant::now() - Duration::from_secs(30);
        let parsed = RoutedCommand {
            matched: "#".to_string(),
            raw: "状态".to_string(),
            user_command: "#状态".to_string(),
            message_type: "blue".to_string(),
            username: "测试玩家".to_string(),
            command: ModuleCommand::CardGame(LandlordCommand::Status),
            observation: CommandObservation {
                captured_at: Some(captured_at),
                ..CommandObservation::default()
            },
        };

        assert_eq!(command_observed_at(&parsed), captured_at);
    }
}
