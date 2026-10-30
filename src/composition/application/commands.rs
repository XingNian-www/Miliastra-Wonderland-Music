use super::*;

#[path = "playlist_window.rs"]
mod playlist_window;

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

/// 日志里的聊天原文：先脱敏、再折叠空白并截断。
///
/// 与 `listener::compact_for_log` 同义。放在这里而不是复用 `listener` 的私有函数，
/// 是因为 `listener` 是同级模块、其辅助函数未对外暴露；两处逻辑保持一致：
/// 先 `redacted_chat_text`（歌单导入链接等按整串匹配，截断后再脱敏会漏）。
fn compact_chat_for_log(text: &str) -> String {
    let compact = crate::privacy::redacted_chat_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() > 120 {
        let mut truncated: String = compact.chars().take(120).collect();
        truncated.push('…');
        truncated
    } else {
        compact
    }
}

/// 日志里的身份角色名：`IdentityRole` 不能直接 `{:?}` 打给维护者看。
fn identity_role_label(role: crate::features::identity::IdentityRole) -> &'static str {
    use crate::features::identity::IdentityRole;
    match role {
        IdentityRole::Friend => "好友",
        IdentityRole::Admin => "管理员",
        IdentityRole::Owner => "主人",
    }
}

/// `execute_command` 那条「排队期 permission_required 过期」路径的文案。
///
/// **不另写文案表**：把 `IdentityRole` 映射成对应的 `CommandPermission` 档位，
/// 再复用拦截点的单一来源 [`CommandGate::message`]。
fn required_message(required: IdentityRole) -> &'static str {
    use crate::features::command_access::CommandPermission;
    let permission = match required {
        IdentityRole::Friend => CommandPermission::Friend,
        // Admin 与 Owner 面向玩家是同一句「仅限管理员」；Owner 档位更严，
        // 单独给 CommandPermission::Owner 会让文案随内部档位变化而漂移。
        IdentityRole::Admin | IdentityRole::Owner => CommandPermission::Admin,
    };
    crate::composition::application::listener::CommandGate::BelowMinimum(permission)
        .message()
        .expect("BelowMinimum 恒有文案")
}

/// 歌单与收藏是否放行。
///
/// **唯一判据是 `@歌单` 这条命令的 `minimum_role`**，与其它命令同一套判定
/// （`catalog_policy` + `actor_permission_with_floor`）。
///
/// 旧实现是 `authority == Friend || role.is_some()`：粉色来源无条件放行，
/// 等于绕开面板上「@歌单 最低权限」的配置，也和「来源只给下界、身份命中即覆盖」
/// 的定稿口径不一致。现在粉色未登记的人靠**来源下界**拿到 Friend 照样能进，
/// 大厅里已映射 Friend/Admin/Owner 的人靠**身份覆盖**能进，蓝字普通成员被拒。
///
/// 注意用 `role_of_sender` 现场解析，不用 `parsed.role` 快照：队列等待期间
/// 身份映射可能已经变化，执行期必须读当前值（与 `command_is_disabled` 同口径）。
fn playlist_command_allows_friend(
    runtime: &ApplicationRuntime,
    parsed: &RoutedCommand,
) -> bool {
    let snapshot = runtime.lifecycle.live_configs.snapshot();
    // 现场解析，不用 parsed.role 快照：队列等待期间身份映射可能已经变化。
    let role = runtime
        .lifecycle
        .live_configs
        .identity
        .role_of_sender(&parsed.username, parsed.account.as_ref());
    playlist_policy_allows(&snapshot.command_access, parsed.authority, role)
}

/// [`playlist_command_allows_friend`] 的纯判定部分，便于脱离运行时单测。
fn playlist_policy_allows(
    command_access: &crate::features::command_access::CommandAccessConfig,
    authority: CommandAuthority,
    role: Option<IdentityRole>,
) -> bool {
    use crate::features::command_access::{actor_permission_with_floor, catalog_policy};
    let actor = actor_permission_with_floor(role, authority);
    catalog_policy(command_access, "@歌单").is_some_and(|policy| policy.allows(Some(actor)))
}

pub(super) struct ImmediateAdministrationPort {
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
    monitor: MonitorShared,
    executed_commands_log_path: std::path::PathBuf,
}

pub(super) struct DeferredIdiomChainPort {
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
    live_configs: LiveConfigs,
    /// 延迟发送也要读命令面板的覆盖，否则同一条接龙文案在"立即发送"和
    /// "延迟发送"两条路径上行为不同，用户会以为改了没反应。
    config_store: Option<crate::config::SharedConfigStore>,
}

/// 个人歌单的播放侧适配：读取当前播放曲目、直接播放收藏曲目。
struct PlaylistRuntimePlaybackPort<'a> {
    runtime: &'a mut ApplicationRuntime,
}

impl ApplicationRuntime {
    /// 上下文丢失（换大厅/进千星/进入游戏）：结束个人歌单模式并清理残留自动项。
    pub(super) fn end_personal_playlist_for_context_loss(&self, reason: &str) -> Result<()> {
        let application = self.playback.playback_application.clone();
        let Some(owner) = application.take_personal_playlist_mode() else {
            return Ok(());
        };
        log::info!("{reason}，已结束「{owner}」的个人歌单模式");
        // 原子清理自动项，避免与并发点歌入队产生索引错位而误删用户点歌。
        self.business
            .business
            .remove_playlist_auto_items()
            .map_err(anyhow::Error::from)?;
        Ok(())
    }
}

impl crate::features::playlist::PlaylistPlaybackPort for PlaylistRuntimePlaybackPort<'_> {
    fn current_track(&self) -> Option<miliastra_playback::PlayableTrack> {
        self.runtime
            .business
            .business
            .playback_state_snapshot()
            .ok()
            .and_then(|snapshot| snapshot.last_observation)
            .and_then(|observation| observation.track)
    }

    fn play(&mut self, track: &miliastra_playback::PlayableTrack) -> Result<()> {
        let application = self.runtime.playback.playback_application.clone();
        application.play_structured_track(track, self.runtime)
    }

    fn enqueue_next(&mut self, track: &miliastra_playback::PlayableTrack) -> Result<bool> {
        let outcome = self
            .runtime
            .business
            .business
            .push_playback_queue_next(QueueItem {
                keyword: track.metadata.title.clone(),
                source: track.track_ref.key.provider.as_str().to_owned(),
                track: Some(track.clone()),
                ..QueueItem::default()
            })?;
        Ok(outcome.accepted)
    }

    fn start_playlist(
        &mut self,
        owner: &str,
        tracks: Vec<miliastra_playback::PlayableTrack>,
    ) -> Result<crate::features::playback::PersonalPlaylistStart> {
        let application = self.runtime.playback.playback_application.clone();
        application.start_personal_playlist(owner, tracks, self.runtime)
    }

    fn stop_playlist(&mut self) -> Result<bool> {
        let application = self.runtime.playback.playback_application.clone();
        application.stop_personal_playlist(self.runtime)
    }

    fn playlist_mode_owner(&self) -> Option<String> {
        self.runtime
            .playback
            .playback_application
            .playlist_mode_owner()
    }

    fn extend_playlist_tracks(
        &mut self,
        owner: &str,
        tracks: &[miliastra_playback::PlayableTrack],
    ) -> Result<bool> {
        Ok(self
            .runtime
            .playback
            .playback_application
            .extend_personal_playlist_tracks(owner, tracks))
    }

    fn import_playlist(
        &mut self,
        provider: &str,
        input: &str,
    ) -> Result<miliastra_playback::PlaylistImportOperation> {
        self.runtime
            .playback
            .native_playback
            .import_playlist(provider, input)
            .map_err(anyhow::Error::from)
    }
}

struct DeferredCardGamePort {
    task_engine: TaskEngineHandle,
}

struct TurtleSoupCommandPort<'a> {
    runtime: &'a ApplicationRuntime,
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
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

impl DeferredIdiomChainPort {
    fn enqueue_message(&self, message: String) -> Result<IdiomDeliveryOutcome> {
        let snapshot = self.business.chat_listener_snapshot()?;
        let target = match listener_residency(snapshot.mode, snapshot.temporary_primary) {
            UiResidency::Primary => DeferredChatTarget::Primary,
            UiResidency::SecondaryCurrentHall => DeferredChatTarget::SecondaryCurrentHall,
        };
        Ok(
            match self.task_engine.enqueue_deferred(DeferredChatMessage {
                text: message,
                target,
                background_key: None,
                formal_epoch: None,
            })? {
                EnqueueOutcome::Added => IdiomDeliveryOutcome::Added,
                EnqueueOutcome::DroppedMessage => IdiomDeliveryOutcome::DroppedEarlierMessage,
                EnqueueOutcome::Rejected => IdiomDeliveryOutcome::Rejected,
            },
        )
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
        self.enqueue_message(message)
    }

    fn send_deferred_template(
        &mut self,
        _message: &str,
        template: &idiom_chain::IdiomReplyTemplate,
    ) -> Result<IdiomDeliveryOutcome> {
        let stored = self
            .config_store
            .as_ref()
            .and_then(|store| store.lock().ok())
            .and_then(|store| store.reply_templates().ok());
        if let Some(stored) = &stored {
            if crate::features::reply_template::is_muted(stored, template.spec.key) {
                return Ok(IdiomDeliveryOutcome::Added);
            }
        }
        let command_override = stored.as_ref().and_then(|stored| {
            crate::features::reply_template::stored_override(stored, template.spec.key)
        });
        // 覆盖只来自命令面板保存的行；配置中心不再提供回复文案入口。
        let rendered = crate::features::reply_template::render(
            template.spec,
            command_override.as_deref(),
            &template.vars,
        );
        if !rendered.missing.is_empty() {
            // 与 delivery.rs / workers.rs 同一口径：用 missing_vars_label 渲染变量名，
            // 并说明玩家可见后果（空串渲染会让这条回复缺一段）。
            log::warn!(
                "回复模板「{}」缺少变量 {}，这些位置已按空串渲染，玩家会看到内容缺一段",
                template.spec.key,
                super::delivery::missing_vars_label(&rendered.missing)
            );
        }
        self.enqueue_message(rendered.text)
    }
}

impl CardGameDeliveryPort for DeferredCardGamePort {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能执行好友验证"))
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能发送好友消息"))
    }

    fn send_friend_template(
        &self,
        _player: &str,
        _message: &str,
        _template: &crate::features::card_games::CardGameReplyTemplate,
    ) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能发送好友模板消息"))
    }

    fn send_friend_batch(
        &self,
        _deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        Err(anyhow!("延迟牌类端口不能发送好友批次"))
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.task_engine, message)
    }
}

impl TurtleSoupApplicationPort for TurtleSoupCommandPort<'_> {
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
        enqueue_current_hall_reply(&self.task_engine, message)
    }

    fn send_turtle_soup_reply(&mut self, reply: turtle_soup::TurtleSoupReplyText) -> Result<()> {
        // 静音由 render_reply_text 统一处理：None 表示这条不该发。
        // 这里不再手动判断 key，复合回复的静音才能沿嵌套链传播。
        let Some(text) = self.runtime.render_reply_text(&reply) else {
            return Ok(());
        };
        enqueue_current_hall_reply(&self.task_engine, &text)
    }
}

impl AdministrationImmediatePort for ImmediateAdministrationPort {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()> {
        self.business
            .set_commands_enabled(enabled)
            .map_err(anyhow::Error::from)
    }

    fn enqueue_hall_reply(&mut self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.task_engine, message)
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
            business: self.business.business.clone(),
            task_engine: self.business.task_engine.clone(),
            live_configs: self.lifecycle.live_configs.clone(),
            config_store: self.lifecycle.config_store.clone(),
        }
    }

    fn turtle_soup_command_port(&self) -> TurtleSoupCommandPort<'_> {
        TurtleSoupCommandPort {
            runtime: self,
            business: self.business.business.clone(),
            task_engine: self.business.task_engine.clone(),
        }
    }

    pub(super) fn immediate_administration_port(&self) -> ImmediateAdministrationPort {
        let live_config = self.lifecycle.live_configs.snapshot();
        ImmediateAdministrationPort {
            business: self.business.business.clone(),
            task_engine: self.business.task_engine.clone(),
            monitor: self.lifecycle.monitor.clone(),
            executed_commands_log_path: live_config.state.executed_commands_log_path.clone(),
        }
    }

    pub(super) fn maybe_warn_hall_expiring(&mut self) -> Result<bool> {
        let application = self.business.hall_application;
        application.maybe_warn_expiring(self)
    }

    pub(super) fn execute_command(&mut self, parsed: &RoutedCommand) -> Result<()> {
        // 队列等待期间权限/开关可能变化，执行时再次读取当前策略。
        if self.command_is_disabled(parsed)? {
            log::info!(
                "命令未执行：当前权限或启用策略拒绝（排队期间策略或全局开关发生变化）「{}」",
                compact_chat_for_log(&parsed.raw)
            );
            return Ok(());
        }
        if let Some(required) = parsed.permission_required {
            log::info!(
                "命令未执行：权限不足，需要{}；该发送者身份为{}「{}」",
                identity_role_label(required),
                parsed
                    .role
                    .map(identity_role_label)
                    .unwrap_or("未登记的普通成员"),
                compact_chat_for_log(&parsed.raw)
            );
            // 走到这里只有一个原因：**路由期**记下的 permission_required 仍是旧值，
            // 而拦截期（listener 的 CommandGate）已按当前配置放行——典型场景是
            // 命令排队期间管理员把该命令的档位调宽了。此时不再重复拦截，
            // 只把这句过期的记录如实回给玩家（常见来源是 route_as_permission_denied）。
            //
            // 文案**不另写一份**：直接复用拦截点的单一来源 `CommandGate::message()`，
            // 否则同一个原因会有两张表，改一处就会漂移。
            let message = required_message(required);
            return ApplicationRuntime::reply(self, message);
        }
        match &parsed.command {
            ModuleCommand::SongRequest(command) => {
                let context = SongRequestContext {
                    message_type: parsed.message_type.clone(),
                    raw: parsed.raw.clone(),
                    username: command_username(parsed).to_string(),
                    user_command: parsed.user_command.clone(),
                    account: parsed.account.clone(),
                    authority: parsed.authority,
                };
                self.business
                    .song_requests
                    .clone()
                    .execute(&context, command, self)
            }
            ModuleCommand::Playback(command) => self.execute_playback_intent(parsed, command),
            ModuleCommand::Hall(command) => self.execute_hall_intent(parsed, command),
            ModuleCommand::Administration(command) => {
                self.execute_administration_intent(parsed, command)
            }
            ModuleCommand::IdiomChain(command) => self.execute_idiom_chain_intent(parsed, command),
            ModuleCommand::CardGame(command) => self.execute_card_game_intent(parsed, command),
            ModuleCommand::TurtleSoup(command) => self.execute_turtle_soup_intent(parsed, command),
            ModuleCommand::Tarot(command) => self.execute_tarot_intent(parsed, command),
            ModuleCommand::Playlist(command) => self.execute_playlist_intent(parsed, command),
            ModuleCommand::Undercover(command) => self.execute_undercover_intent(parsed, command),
            ModuleCommand::Invite(command) => self.execute_invite_intent(parsed, command),
            ModuleCommand::Moderation(command) => self.execute_moderation_intent(parsed, command),
            ModuleCommand::CustomWorkflow(command) => {
                self.execute_custom_workflow_intent(parsed, command)
            }
            ModuleCommand::AiChat(command) => self.execute_ai_chat_intent(parsed, command),
        }
    }

    fn execute_tarot_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &crate::features::tarot::TarotCommand,
    ) -> Result<()> {
        if matches!(command, crate::features::tarot::TarotCommand::Help) {
            self.log_executed_command(parsed, "tarot")?;
            return self.send_command_reply_batch(
                "#塔罗牌帮助",
                &crate::features::reply_template::tarot::HELP,
                &crate::features::reply_template::ReplyVars::new(),
                0,
            );
        }
        let actor = self.canonical_actor_name(&parsed.username);
        let messages = self.business.tarot_application.execute(
            &actor,
            command,
            Instant::now(),
            &self.business.ai,
        )?;
        self.log_executed_command(parsed, "tarot")?;
        let mut rendered = Vec::new();
        for message in &messages {
            rendered.extend(self.render_tarot_reply(message));
        }
        let messages: Vec<_> = rendered.iter().map(String::as_str).collect();
        self.reply_batch(&messages, 0)
    }

    fn execute_playlist_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &crate::features::playlist::PlaylistCommand,
    ) -> Result<()> {
        if !playlist_command_allows_friend(self, parsed) {
            log::info!(
                "命令未执行：个人歌单仅对好友及以上开放，该发送者身份为{}「{}」",
                parsed
                    .role
                    .map(identity_role_label)
                    .unwrap_or("未登记的普通成员"),
                compact_chat_for_log(&parsed.raw)
            );
            // 文案可被配置中心覆盖；变量留空，渲染时按内置默认文案输出。
            let text = self.render_reply(
                crate::features::reply_template::REPLY_SONG[0].key,
                crate::features::reply_template::REPLY_SONG[0].default,
                &crate::features::reply_template::ReplyVars::new(),
            );
            return ApplicationRuntime::reply(self, &text);
        }
        if matches!(command, crate::features::playlist::PlaylistCommand::Open) {
            return self.execute_playlist_window(parsed);
        }
        if matches!(command, crate::features::playlist::PlaylistCommand::Help) {
            self.log_executed_command(parsed, "playlist help")?;
            return self.send_command_reply_batch(
                "@歌单帮助",
                &crate::features::reply_template::HELP_PLAYLIST,
                &crate::features::reply_template::ReplyVars::new(),
                0,
            );
        }
        // 歌单只属于已登记身份的玩家：没有 UUID 就没有歌单。
        let Some(actor) = self.playlist_owner_key(&parsed.username, parsed.account.as_ref()) else {
            log::info!(
                "命令未执行：该玩家未在身份映射登记，因此没有个人歌单「{}」",
                compact_chat_for_log(&parsed.raw)
            );
            let text = self.render_reply(
                crate::features::reply_template::REPLY_SONG[0].key,
                crate::features::reply_template::REPLY_SONG[0].default,
                &crate::features::reply_template::ReplyVars::new(),
            );
            return ApplicationRuntime::reply(self, &text);
        };
        let now = Instant::now();
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or_default();
        // 先克隆 Arc 应用，避免与播放端口同时借用 self。
        let application = self.business.playlist_application.clone();
        let mut port = PlaylistRuntimePlaybackPort { runtime: self };
        let messages = application.execute(&actor, command, &mut port, now, now_epoch)?;
        if matches!(
            command,
            crate::features::playlist::PlaylistCommand::Import { .. }
        ) {
            self.log_executed_command_fields(
                &parsed.message_type,
                command_username(parsed),
                "@导入歌单<链接已隐藏>",
                "playlist",
            )?;
        } else {
            self.log_executed_command(parsed, "playlist")?;
        }
        let rendered: Vec<String> = messages
            .iter()
            .flat_map(|reply| self.render_playlist_reply(reply))
            .collect();
        let messages: Vec<_> = rendered.iter().map(String::as_str).collect();
        self.reply_batch(&messages, 0)
    }

    fn execute_hall_intent(&mut self, parsed: &RoutedCommand, command: &HallCommand) -> Result<()> {
        let context = HallCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let application = self.business.hall_application;
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
        let application = self.business.administration_application;
        application.execute(&context, command, self)
    }

    fn execute_idiom_chain_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &idiom_chain::IdiomChainCommand,
    ) -> Result<()> {
        let observed_at = command_observed_at(parsed);
        let player = self.canonical_actor_name(&parsed.username);
        if command.requires_executor() {
            let application = self.business.idiom_chain_application;
            application.execute_explanation(&player, command, observed_at, self)
        } else {
            log::debug!("成语接龙命令已按正式队列顺序处理，回复进入延迟聊天队列");
            let mut port = self.deferred_idiom_chain_port();
            self.business.idiom_chain_application.execute_deferred(
                &parsed.raw,
                &player,
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
        let player = self.canonical_actor_name(&parsed.username);
        match card_game_effect_lane(command) {
            CardGameEffectLane::Formal => self.business.card_games.execute_command(
                &player,
                command,
                observed_at,
                CardGameEffectLane::Formal,
                self,
            ),
            CardGameEffectLane::Deferred => {
                let port = DeferredCardGamePort {
                    task_engine: self.business.task_engine.clone(),
                };
                self.business.card_games.execute_command(
                    &player,
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
        let player = self.canonical_actor_name(&parsed.username);
        self.business.turtle_soup_application.execute_command(
            &parsed.raw,
            &player,
            parsed.authority == CommandAuthority::Friend,
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
        let mut request = InviteRequest::new(
            invite.username.clone(),
            invite.display_name.clone(),
            invite.seq,
            invite.password.clone(),
        );
        // 邀请要去好友列表里找**发起人**。外部聊天源的信封主语是纯 UID，
        // 而好友列表上显示的是昵称/备注，直接拿 UID 搜必然落空
        // （实机表现：locate_friend / no unique stable friend row）。
        //
        // 这里用 UID 反查出可以匹配的名字。之所以是多个：
        // 自动建的身份把 API 昵称放在 note、随机串放在 nickname，
        // 手工映射则真名在 nickname——两种来源都得试。
        if let Some(account) = parsed.account.as_ref() {
            let names = self
                .lifecycle
                .live_configs
                .identity
                .friend_row_names_for_account(&account.game, &account.uid);
            if !names.is_empty() {
                log::info!(
                    "邀请流程：UID「{}」反查到 {} 个候选名，将依次尝试搜索好友",
                    account.uid,
                    names.len()
                );
                request.search_names = names;
            } else {
                // 不打印真实 UID 与信封主语：两者都是玩家标识（MUST_READ §9）。
                log::warn!(
                    "邀请流程：该发起者未在身份映射登记任何昵称（步骤：按 UID 反查好友列表候选名），本次只能退回用信封主语搜索，很可能在好友列表里定位不到人"
                );
            }
        }
        let execution = match self.business.business.begin_invite(request)? {
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
        self.execute_moderation_with_vote(parsed, command)
            .map(|_| ())
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
        configure_idle_exit(&self.business.business, minutes)
    }

    pub(super) fn commands_enabled(&self) -> Result<bool> {
        Ok(self
            .business
            .business
            .operational_snapshot(Instant::now())?
            .commands_enabled())
    }

    pub(super) fn check_public_hall(&self) -> Result<bool> {
        let mut port = PublicHallDetectionPort {
            hall_ui: self.ui.hall_ui.clone(),
            business: self.business.business.clone(),
        };
        self.business.hall_application.check_public_hall(&mut port)
    }

    fn execute_undercover_command(
        &self,
        parsed: &RoutedCommand,
        command: &UndercoverCommand,
    ) -> Result<()> {
        let source = match parsed.authority {
            CommandAuthority::Friend => UndercoverCommandSource::Friend,
            CommandAuthority::HallMember => UndercoverCommandSource::Hall,
        };
        let player = self.canonical_actor_name(&parsed.username);
        self.business.undercover_game.execute_command(
            &player,
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
        self.business
            .turtle_soup_application
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
            .business
            .explain_idiom_chain_at(player, command, observed_at)
            .map_err(anyhow::Error::from)
    }

    fn send_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()> {
        let message_refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&message_refs, delay_ms)
    }

    fn send_explanation_batch(
        &mut self,
        messages: &[idiom_chain::IdiomExplanationMessage],
        delay_ms: u64,
    ) -> Result<()> {
        let rendered = messages
            .iter()
            .filter_map(|message| match message {
                idiom_chain::IdiomExplanationMessage::Template { spec, vars } => {
                    self.render_template_text(spec, vars)
                }
                idiom_chain::IdiomExplanationMessage::Raw(text) => Some(text.clone()),
            })
            .collect::<Vec<_>>();
        let refs = rendered.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&refs, delay_ms)
    }
}

impl AdministrationImmediatePort for ApplicationRuntime {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()> {
        self.business
            .business
            .set_commands_enabled(enabled)
            .map_err(anyhow::Error::from)
    }

    fn enqueue_hall_reply(&mut self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.business.task_engine, message)
    }

    fn enqueue_hall_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        // 静音由 render_template_text 统一处理：None 表示这条不该发。
        let Some(text) = self.render_template_text(spec, vars) else {
            return Ok(());
        };
        // 多行文案逐条入队，与立即发送路径保持同一拆条规则。
        for message in crate::text::split_configured_lines(&text) {
            self.enqueue_hall_reply(&message)?;
        }
        Ok(())
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
    fn send_command_reply_batch(
        &mut self,
        command_id: &str,
        template: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
        delay_ms: u64,
    ) -> Result<()> {
        // 帮助是单条模板、多行内容。命令面板的覆盖按该模板的 key 查，
        // 多行由发送路径拆成多条消息——不再有"逐行模板数组"这一层。
        let stored = if let Some(store) = self.lifecycle.config_store.as_ref() {
            let store = store.lock().map_err(|_| anyhow!("命令回复存储锁已损坏"))?;
            store.reply_templates()?.get(command_id).cloned()
        } else {
            None
        };
        let override_text = stored
            .as_ref()
            .and_then(|rows| rows.iter().find(|row| row.key == template.key))
            .map(|row| row.text.clone())
            .or_else(|| self.command_reply_text(template.key));
        let rendered =
            crate::features::reply_template::render(template, override_text.as_deref(), vars);
        if !rendered.missing.is_empty() {
            log::warn!(
                "帮助模板「{}」缺少变量，这些位置已按空串渲染，玩家会看到帮助少一段：{}",
                template.key,
                rendered
                    .missing
                    .iter()
                    .map(|name| format!("「{name}」"))
                    .collect::<Vec<_>>()
                    .join("、")
            );
        }
        let messages = crate::text::split_configured_lines(&rendered.text);
        let refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&refs, delay_ms)
    }
    fn send_hall(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn send_hall_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        self.reply_with_template(spec, vars)
    }

    fn send_hall_batch(&mut self, messages: &[&str], delay_ms: u64) -> Result<()> {
        self.reply_batch(messages, delay_ms)
    }

    fn render_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> String {
        // 端口契约是"只渲染不发送"，静音时给空串，由调用方决定是否成段。
        self.render_template_text(spec, vars).unwrap_or_default()
    }
}

impl HallDetectionPort for ApplicationRuntime {
    fn detect_public_hall(&mut self) -> Result<Option<HallObservation>> {
        detect_public_hall(&self.ui.hall_ui)
    }

    fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        self.business
            .business
            .update_hall_remaining_minutes(minutes)
            .map_err(anyhow::Error::from)
    }

    fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
        self.business
            .business
            .clear_hall_remaining_minutes()
            .map_err(anyhow::Error::from)
    }
}

impl HallApplicationPort for ApplicationRuntime {
    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn reply_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        self.reply_with_template(spec, vars)
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
            .ui
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
            .ui
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
        Ok(self
            .business
            .business
            .hall_state_snapshot()?
            .remaining_minutes_now())
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
        Ok(self
            .business
            .business
            .hall_state_snapshot()?
            .expiring_warning_sent)
    }

    fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
        Ok(self
            .business
            .business
            .hall_state_snapshot()?
            .remaining_minutes_now())
    }

    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn reply_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        self.reply_with_template(spec, vars)
    }

    fn mark_hall_expiring_warning_sent(&mut self) -> Result<()> {
        self.business
            .business
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
    use crate::composition::application::listener::CommandGate;
    use crate::features::command::CommandAuthority;
    use crate::features::command_access::{catalog_policy, CommandPermission};

    /// 构造一个走真实 CommandEnvelope::new 的视频信封（与路由测试同形）。
    fn access_envelope(message_type: &str, command: &str) -> CommandEnvelope {
        CommandEnvelope::new(
            format!("玩家甲：{command}"),
            "玩家甲",
            message_type,
            command,
            crate::features::command::CommandObservation::default(),
        )
        .expect("test command envelope")
    }

    /// 空的自定义流程服务：这两条测试只关心权限策略层，不涉及流程。
    fn empty_workflows() -> CustomWorkflowService {
        CustomWorkflowService::new(
            crate::features::custom_workflow::CustomWorkflowConfig::default(),
            crate::features::custom_workflow::WorkflowDefaults {
                default_timeout_ms: 1000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        )
    }

    /// 面板关掉命令时，玩家看到的必须是「已禁用」，而不是「仅限好友」。
    ///
    /// 回归：旧实现把 `User | Friend` 一起映射成 `IdentityRole::Friend`，
    /// 面板 `enabled=false` 关掉命令后玩家收到「该命令仅限好友使用」，
    /// 排查方向直接被带偏到身份映射上。
    #[test]
    fn disabled_command_reports_disabled_not_friend_only() {
        use crate::features::command::DeniedReason;
        use crate::features::command_access::{
            CommandAccessConfig, CommandPermission, CommandPolicy,
        };
        let mut access = CommandAccessConfig::default();
        access.overrides.insert(
            "@删除".into(),
            CommandPolicy {
                enabled: false,
                // 即使档位是 Friend，关闭也要优先表达为 Disabled。
                minimum_role: CommandPermission::Friend,
            },
        );
        let identity = crate::features::identity::IdentityAccess::default();
        let workflows = empty_workflows();
        let routed =
            crate::interfaces::chat::ChatCommandRouter::with_access(&workflows, &identity, &access)
                .route(&access_envelope("pink", "@删除"), None)
                .expect("@删除 应被路由（拒绝发生在策略层）");

        // 诊断字段仍在（route 期记录），但**它不是玩家可见文案的来源**。
        assert_eq!(routed.denied_reason, Some(DeniedReason::Disabled));
        // permission_required 的**既有语义不变**（Friend 仍是一票否决的摘要）。
        assert_eq!(routed.permission_required, Some(IdentityRole::Friend));
        // 真正的门在这里：入队前按 policy.enabled 判为 Disabled。
        // 这是玩家唯一会经过的拦截点——上一版只在 execute_command 出文案，
        // 而命令根本走不到那里，所以玩家什么都收不到。
        let policy = catalog_policy(&access, "@删除").expect("@删除 有策略");
        let gate = CommandGate::from_policy(
            &policy,
            CommandPermission::Friend,
            CommandAuthority::Friend,
            true,
            true,
        );
        assert_eq!(
            gate,
            CommandGate::Disabled,
            "面板关掉命令时 gate 必须报 Disabled",
        );
        assert_eq!(gate.message(), Some("该命令已在面板禁用"));
    }

    /// 权限不够（命令本身是开着的）时要说出真正需要的档位。
    #[test]
    fn below_minimum_reports_the_actual_required_role() {
        use crate::features::command::DeniedReason;
        use crate::features::command_access::{
            CommandAccessConfig, CommandPermission, CommandPolicy,
        };
        let mut access = CommandAccessConfig::default();
        access.overrides.insert(
            "@暂停".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::Admin,
            },
        );
        let identity = crate::features::identity::IdentityAccess::default();
        let workflows = empty_workflows();
        let routed =
            crate::interfaces::chat::ChatCommandRouter::with_access(&workflows, &identity, &access)
                .route(&access_envelope("blue", "@暂停"), None)
                .expect("@暂停 应被路由");

        assert_eq!(
            routed.denied_reason,
            Some(DeniedReason::BelowMinimum(CommandPermission::Admin))
        );
        assert_eq!(routed.permission_required, Some(IdentityRole::Admin));
        // 拦截点的判定：命令开着但档位不够 → BelowMinimum(Admin)。
        let policy = catalog_policy(&access, "@暂停").expect("@暂停 有策略");
        let gate = CommandGate::from_policy(
            &policy,
            CommandPermission::User,
            CommandAuthority::HallMember,
            false,
            true,
        );
        assert_eq!(gate, CommandGate::BelowMinimum(CommandPermission::Admin));
        assert_eq!(gate.message(), Some("权限不足：该命令仅限管理员使用"));
    }

    /// `execute_command` 的过期 permission_required 文案，必须与拦截点逐字相同。
    ///
    /// 复核要求：窗口两侧（`execute_command` 与 `CommandGate::message`）
    /// 都各写死 `&'static str` 时，必须有一条测试把它们钉在一起，
    /// 否则改一处就会漂移，玩家会看到两套说法。
    ///
    /// 当前实现已把 `execute_command` 侧收敛到复用 `CommandGate::message()`
    /// （见 `required_message`），本条测试既验证收敛生效，也把玩家可见的
    /// 字面量钉死——将来任一侧被改回硬编码，这里就会红。
    #[test]
    fn execute_command_reuses_the_gate_message_table() {
        use crate::features::command_access::CommandPermission;
        for (role, gate_permission, expected) in [
            (
                IdentityRole::Friend,
                CommandPermission::Friend,
                "权限不足：该命令仅限好友使用",
            ),
            (
                IdentityRole::Admin,
                CommandPermission::Admin,
                "权限不足：该命令仅限管理员使用",
            ),
            (
                IdentityRole::Owner,
                CommandPermission::Admin,
                "权限不足：该命令仅限管理员使用",
            ),
        ] {
            // ① execute_command 实际用的文案
            let from_execute = required_message(role);
            // ② 拦截点 gate 实际用的文案
            let from_gate = CommandGate::BelowMinimum(gate_permission)
                .message()
                .expect("BelowMinimum 恒有文案");
            // 两者必须逐字相同
            assert_eq!(
                from_execute, from_gate,
                "execute_command 与拦截点文案漂移：{role:?}",
            );
            // 且等于玩家应当看到的字面量
            assert_eq!(
                from_execute, expected,
                "玩家可见文案被意外改写：{role:?}",
            );
        }
    }

    /// 歌单的放行必须走 `@歌单` 的 minimum_role，而不是"粉色来源无条件放行"。
    #[test]
    fn playlist_commands_require_friend_or_above_via_the_catalog_policy() {
        use crate::features::command_access::{
            CommandAccessConfig, CommandPermission, CommandPolicy,
        };
        let default = CommandAccessConfig::default();

        // 蓝字普通大厅成员（无映射）：来源下界 User < Friend → 拒绝。
        assert!(!playlist_policy_allows(
            &default,
            CommandAuthority::HallMember,
            None
        ));
        // 粉色好友消息（未映射）：来源下界 Friend → 放行。
        assert!(playlist_policy_allows(
            &default,
            CommandAuthority::Friend,
            None
        ));
        // 大厅里已映射 Friend/Admin/Owner：身份覆盖 → 放行。
        for role in [
            IdentityRole::Friend,
            IdentityRole::Admin,
            IdentityRole::Owner,
        ] {
            assert!(
                playlist_policy_allows(&default, CommandAuthority::HallMember, Some(role)),
                "mapped role {role:?}"
            );
        }

        // 面板把 @歌单 降到 User：蓝字普通成员也能进（证明判据真的是面板配置）。
        let mut opened = CommandAccessConfig::default();
        opened.overrides.insert(
            "@歌单".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(playlist_policy_allows(
            &opened,
            CommandAuthority::HallMember,
            None
        ));

        // 面板把 @歌单 关掉：连粉色好友也进不去（enabled 优先）。
        let mut closed = CommandAccessConfig::default();
        closed.overrides.insert(
            "@歌单".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(!playlist_policy_allows(
            &closed,
            CommandAuthority::Friend,
            None
        ));
        assert!(!playlist_policy_allows(
            &closed,
            CommandAuthority::HallMember,
            Some(IdentityRole::Owner)
        ));
    }

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
            account: None,
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            denied_reason: None,
            command: ModuleCommand::CardGame(LandlordCommand::Status),
            observation: CommandObservation {
                captured_at: Some(captured_at),
                ..CommandObservation::default()
            },
        };

        assert_eq!(command_observed_at(&parsed), captured_at);
    }
}
