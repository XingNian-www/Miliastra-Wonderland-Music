//! Personal playlist decisions use the existing point-song hall subscription and UI lease.
use super::*;
use crate::features::command::{CommandEnvelope, CommandObservation};
use crate::features::playlist::{PlaylistApplication, PlaylistCommand, WINDOW_TTL};
use crate::observation::chat::ChatTimelineMessage;

/// Clear business window state on every exit, including failed baseline/output/observation.
struct PlaylistWindowCleanup {
    application: PlaylistApplication,
    owner: String,
}

struct PlaylistWindowSession {
    reader: ChatDecisionReader,
    _cleanup: PlaylistWindowCleanup,
}

impl PlaylistWindowSession {
    fn finish_observation(self) -> PlaylistWindowCleanup {
        let Self { reader, _cleanup } = self;
        drop(reader);
        _cleanup
    }
}

impl Drop for PlaylistWindowCleanup {
    fn drop(&mut self) {
        if let Err(error) = self.application.discard_window(&self.owner) {
            log::warn!("歌单窗口清理失败: {error:#}");
        }
    }
}

fn playlist_decision_envelope(
    message: &ChatTimelineMessage,
    captured_at: Instant,
) -> Option<CommandEnvelope> {
    // Point-song decisions read blue messages in CurrentHall even for a pink request.
    if message.message_type != "blue" {
        return None;
    }
    let observation = CommandObservation {
        captured_at: Some(captured_at),
        message_id: Some(message.id.clone()),
        message_text: Some(message.text.clone()),
        lock_anchor: Some(message.lock_anchor()),
        ..CommandObservation::default()
    };
    if let Some(sender) = message
        .sender
        .as_deref()
        .filter(|sender| !sender.trim().is_empty())
    {
        CommandEnvelope::new(
            &message.text,
            sender,
            &message.message_type,
            secondary_hall_command_text(&message.text)?,
            observation,
        )
    } else {
        // Primary OCR includes the sender in the same text, as in the ordinary listener.
        crate::interfaces::chat::parse_command_envelope(
            &message.text,
            &message.message_type,
            observation,
        )
    }
}

fn playlist_decision(envelope: &CommandEnvelope) -> Option<PlaylistCommand> {
    let command = PlaylistCommand::parse_window_chat(envelope)?.command;
    matches!(
        command,
        PlaylistCommand::Select(_) | PlaylistCommand::Reroll | PlaylistCommand::Close
    )
    .then_some(command)
}

fn owner_matches(owner: &str, sender: &str) -> bool {
    !sender.trim().is_empty()
        && crate::text::normalize_comparison_text(owner)
            == crate::text::normalize_comparison_text(sender)
}

/// 决策发送者是不是窗口属主（带身份解析）。
///
/// `owner` 是归属键（`id:<uuid>` 形态），`sender` 是 OCR 出来的展示名。
/// 用 [`owner_matches`] 直接比较这两者**永远不相等**，会让属主本人在窗口期
/// 发的 `@1`/`@换一批`/`@关闭` 全部被判"不是属主"，窗口只能超时。
///
/// 所以先把发送者解析成同一套候选归属键，再比对。`sender` 直比只作为
/// 身份未登记时的兜底，保留原有行为。
fn owner_matches_identity(owner: &str, sender: &str, candidates: &[String]) -> bool {
    candidates
        .iter()
        .any(|candidate| owner_matches(owner, candidate))
        || owner_matches(owner, sender)
}

fn acknowledge_playlist_decision(
    shared: &crate::observation::chat::ChatTimeline,
    envelope: &CommandEnvelope,
    changed: bool,
) -> Result<()> {
    let observation = envelope.observation();
    if let Some(id) = &observation.message_id {
        match (
            changed,
            observation.message_text.as_deref(),
            observation.lock_anchor,
        ) {
            (true, Some(text), Some(anchor)) if !text.trim().is_empty() => {
                shared.acknowledge_command_at(id, text, anchor)?;
            }
            _ => {
                shared.acknowledge_business(id)?;
            }
        }
    }
    Ok(())
}

impl ChatDecisionReader {
    fn accept_playlist_message(
        &mut self,
        shared: &crate::observation::chat::ChatTimeline,
        message: &ChatTimelineMessage,
    ) -> Result<bool> {
        let chat_message = ChatMessage {
            message_type: message.message_type.clone(),
            block: message.block,
            text: message.text.clone(),
        };
        if !self.accept_once(&chat_message) {
            return Ok(false);
        }
        self.seen.insert((message.id.clone(), message.text.clone()));
        // Consume before releasing the exclusive dispatch gate; success later adds a lock.
        shared.acknowledge_business(&message.id)?;
        Ok(true)
    }

    /// Keep the ordinary reader's baseline/cursor rule, but preserve shared sender/id evidence.
    /// An unconfirmed sender is deliberately not inserted into seen: a later shared revision
    /// may establish its identity without promoting a historical baseline message.
    fn read_playlist_messages(&mut self) -> Result<Vec<(ChatTimelineMessage, Instant)>> {
        let mut messages = Vec::new();
        for snapshot in self.subscriber.read()? {
            if !self.same_conversation(&snapshot) {
                continue;
            }
            // 与 read_messages 共用同一条首屏防护，避免某条路径静默绕开。
            if self.skips_first_frame_of_new_conversation(&snapshot) {
                continue;
            }
            let snapshot_at = snapshot.captured_at;
            for message in snapshot.messages {
                if !self.is_after_baseline_at(snapshot_at, &message) {
                    continue;
                }
                if self.baseline_cursor.is_some_and(|(session, sequence)| {
                    message.id.visual_session == session
                        && message.id.bubble_sequence.get() <= sequence
                }) || self
                    .seen
                    .contains(&(message.id.clone(), message.text.clone()))
                {
                    continue;
                }
                messages.push((message, snapshot.captured_at));
            }
        }
        Ok(messages)
    }
}

impl ApplicationRuntime {
    fn playlist_window_active(
        &self,
        reader: &ChatDecisionReader,
        deadline: Instant,
    ) -> Result<bool> {
        if !self.lifecycle.running.load(AtomicOrdering::SeqCst) || Instant::now() >= deadline {
            return Ok(false);
        }
        if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
            return Err(anyhow!("运行已暂停，已结束歌单窗口"));
        }
        if self.lifecycle.live_configs.has_pending_reload() {
            return Err(anyhow!("配置重载待处理，已结束歌单窗口"));
        }
        let mode = self.business.business.chat_listener_snapshot()?;
        if mode.mode != reader.listener_mode || mode.pending_mode.is_some() {
            return Err(anyhow!("值守模式正在切换，已结束歌单窗口"));
        }
        Ok(true)
    }

    fn apply_playlist_window_decision(
        &mut self,
        application: &PlaylistApplication,
        owner: &str,
        command: &PlaylistCommand,
        envelope: &CommandEnvelope,
        changed: bool,
    ) -> Result<()> {
        let mut port = PlaylistRuntimePlaybackPort { runtime: self };
        let messages =
            application.execute(owner, command, &mut port, Instant::now(), playlist_seed())?;
        // Successful choices use the existing text/position lock across visual rebuilds;
        // an invalid index is only consumed and must not evict a real success lock.
        acknowledge_playlist_decision(&self.ui.chat_timeline, envelope, changed)?;
        let matched =
            PlaylistCommand::parse_window_chat(envelope).expect("parsed playlist decision");
        let interaction =
            RoutedCommand::from_envelope(envelope, matched.map(ModuleCommand::Playlist));
        if changed {
            self.log_executed_command(&interaction, "playlist")?;
        } else if let PlaylistCommand::Select(index) = command {
            log::info!(
                "命令未执行：歌单序号越界 index={} window_len={} text={}",
                index,
                application.window_len(owner, Instant::now())?,
                crate::privacy::redacted_chat_text(envelope.user_command())
            );
        }
        let rendered: Vec<String> = messages
            .iter()
            .flat_map(|reply| self.render_playlist_reply(reply))
            .collect();
        if changed {
            crate::debug_read_only::record_command_immediate(
                &interaction,
                "playlist_window",
                rendered.join(" "),
            );
        }
        self.reply_batch(&rendered.iter().map(String::as_str).collect::<Vec<_>>(), 0)
    }

    pub(super) fn execute_playlist_window(&mut self, parsed: &RoutedCommand) -> Result<()> {
        // 歌单只属于已登记身份的玩家：没有 UUID 就没有歌单窗口。
        let Some(owner) = self.playlist_owner_key(&parsed.username) else {
            log::info!(
                "命令未执行：该玩家未在身份映射登记，无个人歌单 text={}",
                parsed.raw
            );
            let text = self.render_reply(
                crate::features::reply_template::REPLY_SONG[0].key,
                crate::features::reply_template::REPLY_SONG[0].default,
                &crate::features::reply_template::ReplyVars::new(),
            );
            return ApplicationRuntime::reply(self, &text);
        };
        let candidates = vec![owner.clone()];
        let application = self.business.playlist_application.clone();
        let _cleanup = PlaylistWindowCleanup {
            application: application.clone(),
            owner: owner.clone(),
        };
        if application.items_for(&candidates)?.is_empty() {
            let mut port = PlaylistRuntimePlaybackPort { runtime: self };
            let messages = application.execute(
                &owner,
                &PlaylistCommand::Open,
                &mut port,
                Instant::now(),
                playlist_seed(),
            )?;
            self.log_executed_command(parsed, "playlist")?;
            let rendered: Vec<String> = messages
                .iter()
                .flat_map(|reply| self.render_playlist_reply(reply))
                .collect();
            return self.reply_batch(&rendered.iter().map(String::as_str).collect::<Vec<_>>(), 0);
        }
        // Bound setup separately; the full selection period starts after the recommendations send.
        let setup_deadline = Instant::now() + WINDOW_TTL;
        let reader = self.begin_chat_decision_reader_until(
            ChatDecisionScope::CurrentHall,
            &|message_type| message_type == "blue",
            &|text| {
                secondary_hall_command_text(text)
                    .and_then(|text| {
                        CommandEnvelope::new(
                            text,
                            "决策基线",
                            "blue",
                            text,
                            CommandObservation::default(),
                        )
                    })
                    .as_ref()
                    .and_then(playlist_decision)
                    .is_some()
            },
            setup_deadline,
        )?;
        let mut session = PlaylistWindowSession { reader, _cleanup };
        let reader = &mut session.reader;
        if !self.playlist_window_active(reader, setup_deadline)? {
            return Ok(());
        }
        let mut port = PlaylistRuntimePlaybackPort { runtime: self };
        let messages = application.execute(
            &owner,
            &PlaylistCommand::Open,
            &mut port,
            Instant::now(),
            playlist_seed(),
        )?;
        self.log_executed_command(parsed, "playlist")?;
        if !self.playlist_window_active(reader, setup_deadline)? {
            return Ok(());
        }
        let rendered: Vec<String> = messages
            .iter()
            .flat_map(|reply| self.render_playlist_reply(reply))
            .collect();
        self.reply_batch(&rendered.iter().map(String::as_str).collect::<Vec<_>>(), 0)?;
        let mut deadline = Instant::now() + WINDOW_TTL;
        application.set_window_deadline(&owner, deadline)?;
        while self.playlist_window_active(reader, deadline)? {
            for (message, captured_at) in reader.read_playlist_messages()? {
                if !self.playlist_window_active(reader, deadline)? {
                    break;
                }
                let Some(envelope) = playlist_decision_envelope(&message, captured_at) else {
                    if crate::interfaces::chat::looks_like_command(&message.text) {
                        log::info!(
                            "命令未执行：歌单窗口决策信封不成立 text={}",
                            crate::privacy::redacted_chat_text(&message.text)
                        );
                    } else {
                        log::debug!(
                            "歌单窗口忽略非命令消息 text={}",
                            crate::privacy::redacted_chat_text(&message.text)
                        );
                    }
                    continue;
                };
                // 属主是 id:<uuid> 归属键、发送者是展示名：必须经身份解析比较，
                // 否则属主本人也永远对不上，整个决策窗口不可用。
                let sender_name = self.canonical_actor_name(envelope.username());
                let sender_candidates = self.playlist_owner_candidates(&sender_name);
                if !owner_matches_identity(&owner, &sender_name, &sender_candidates) {
                    log::info!(
                        "命令未执行：歌单窗口决策发送者不是当前窗口属主 sender={} owner={} text={}",
                        envelope.username(),
                        owner,
                        crate::privacy::redacted_chat_text(&message.text)
                    );
                    continue;
                }
                let Some(command) = playlist_decision(&envelope) else {
                    log::info!(
                        "命令未执行：歌单窗口收到非窗口决策命令 text={}",
                        crate::privacy::redacted_chat_text(&message.text)
                    );
                    continue;
                };
                if !reader.accept_playlist_message(&self.ui.chat_timeline, &message)? {
                    log::info!(
                        "命令未执行：歌单决策已被基线或已消费否决 text={} y={}",
                        crate::privacy::redacted_chat_text(&message.text),
                        message.block.y
                    );
                    continue;
                }
                let changed = match command {
                    PlaylistCommand::Select(index) => {
                        usize::from(index) <= application.window_len(&owner, Instant::now())?
                    }
                    _ => true,
                };
                if changed && matches!(command, PlaylistCommand::Select(_) | PlaylistCommand::Close)
                {
                    // As in point-song: finish observing before starting playback/output,
                    // while keeping the owner-state cleanup alive until the operation ends.
                    let _cleanup = session.finish_observation();
                    return self.apply_playlist_window_decision(
                        &application,
                        &owner,
                        &command,
                        &envelope,
                        changed,
                    );
                }
                self.apply_playlist_window_decision(
                    &application,
                    &owner,
                    &command,
                    &envelope,
                    changed,
                )?;
                if changed && matches!(command, PlaylistCommand::Reroll) {
                    deadline = Instant::now() + WINDOW_TTL;
                    application.set_window_deadline(&owner, deadline)?;
                }
                if application.window_len(&owner, Instant::now())? == 0 {
                    return Ok(());
                }
            }
            reader.wait_for_input(
                self.lifecycle.config.timing.decision.poll_ms,
                deadline.saturating_duration_since(Instant::now()),
            )?;
        }
        drop(session);
        if self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            ApplicationRuntime::reply(self, "推荐已超时关闭,重新发送@歌单可再次推荐.")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::chat::{ChatIdentity, ChatTimeline, RecognizedChatMessage};
    use miliastra_playback::{PlayableTrack, ProviderId, TrackKey, TrackMetadata, TrackRef};

    struct Playback {
        played: usize,
    }
    impl crate::features::playlist::PlaylistPlaybackPort for Playback {
        fn current_track(&self) -> Option<PlayableTrack> {
            None
        }
        fn play(&mut self, _: &PlayableTrack) -> Result<()> {
            anyhow::bail!("recommendations should enqueue instead of playing immediately")
        }
        fn enqueue_next(&mut self, _: &PlayableTrack) -> Result<bool> {
            self.played += 1;
            Ok(true)
        }
        fn import_playlist(
            &mut self,
            _: &str,
            _: &str,
        ) -> Result<miliastra_playback::PlaylistImportOperation> {
            anyhow::bail!("playlist import is unavailable in this test port")
        }
    }

    fn input(text: &str, sender: Option<&str>, color: &str, y: i32) -> RecognizedChatMessage {
        RecognizedChatMessage {
            text: text.into(),
            sender: sender.map(str::to_owned),
            raw_sender: sender.map(str::to_owned),
            message_type: color.into(),
            block: Rect::new(0, y, 100, 10),
        }
    }

    fn session(
        shared: &ChatTimeline,
        observations: &ChatObservationShared,
        application: &PlaylistApplication,
        chat: ChatIdentity,
    ) -> PlaylistWindowSession {
        let guard = observations.begin_exclusive().unwrap();
        let subscriber = shared.subscribe().unwrap();
        shared
            .publish(
                Instant::now(),
                chat.clone(),
                vec![input("旧历史:@1", None, "blue", 0)],
            )
            .unwrap();
        let snapshot = subscriber.read().unwrap().pop().unwrap();
        subscriber
            .bind_chat(chat.clone(), snapshot.visual_session)
            .unwrap();
        PlaylistWindowSession {
            reader: ChatDecisionReader {
                listener_mode: ChatListenerMode::Primary,
                subscriber,
                chat,
                baseline_at: snapshot.captured_at,
                baseline_cursor: Some((
                    snapshot.visual_session,
                    snapshot.messages[0].id.bubble_sequence.get(),
                )),
                seen: snapshot
                    .messages
                    .into_iter()
                    .map(|message| (message.id, message.text))
                    .collect(),
                scope: snapshot.scope.clone(),
                any_conversation: false,
                settled_conversations: std::collections::HashSet::new(),
                sequence_floor: None,
                screen_lock: DecisionScreenLock::default(),
                _observation_session: guard,
            },
            _cleanup: PlaylistWindowCleanup {
                application: application.clone(),
                owner: "甲".into(),
            },
        }
    }

    fn application() -> PlaylistApplication {
        let application =
            PlaylistApplication::from_connection(rusqlite::Connection::open_in_memory().unwrap())
                .unwrap();
        application
            .add(
                "甲",
                &PlayableTrack {
                    track_ref: TrackRef {
                        key: TrackKey::new(ProviderId::QqMusic, "a").unwrap(),
                        resolver_locator: None,
                    },
                    metadata: TrackMetadata {
                        title: "歌".into(),
                        artists: vec![],
                        album: None,
                        duration_ms: None,
                    },
                },
                0,
            )
            .unwrap();
        application
    }

    #[test]
    fn selection_cancel_timeout_and_failure_release_exclusive_and_business_window() {
        for chat in [ChatIdentity::PrimaryHall, ChatIdentity::SecondaryHall] {
            for ending in ["select", "close", "timeout", "failure"] {
                let shared = ChatTimeline::new();
                let observations = ChatObservationShared::new();
                let app = application();
                let mut session = session(&shared, &observations, &app, chat.clone());
                let mut playback = Playback { played: 0 };
                let now = Instant::now();
                app.execute("甲", &PlaylistCommand::Open, &mut playback, now, 1)
                    .unwrap();
                assert!(observations.begin_exclusive().is_err());
                match ending {
                    "select" | "close" => {
                        let text = if ending == "select" { "@1" } else { "@关闭" };
                        shared
                            .publish(
                                Instant::now(),
                                chat.clone(),
                                vec![
                                    input("旧历史:@1", None, "blue", 0),
                                    input(text, Some("甲"), "blue", 20),
                                ],
                            )
                            .unwrap();
                        let messages = session.reader.read_playlist_messages().unwrap();
                        assert_eq!(messages.len(), 1);
                        let envelope =
                            playlist_decision_envelope(&messages[0].0, messages[0].1).unwrap();
                        let command = playlist_decision(&envelope).unwrap();
                        app.execute("甲", &command, &mut playback, now, 1).unwrap();
                        assert_eq!(playback.played, usize::from(ending == "select"));
                    }
                    "timeout" => assert_eq!(app.window_len("甲", now + WINDOW_TTL).unwrap(), 0),
                    "failure" => {
                        shared.report_failure("真实读取失败").unwrap();
                        assert!(session.reader.read_playlist_messages().is_err());
                    }
                    _ => unreachable!(),
                }
                drop(session);
                assert!(observations.begin_exclusive().is_ok(), "{ending}");
                assert!(
                    !app.close_active_window("甲", now + WINDOW_TTL).unwrap(),
                    "{ending}: stale owner window"
                );
            }
        }
    }

    #[test]
    fn terminal_observation_finishes_before_playback_without_removing_selection() {
        let shared = ChatTimeline::new();
        let observations = ChatObservationShared::new();
        let app = application();
        let mut session = session(&shared, &observations, &app, ChatIdentity::PrimaryHall);
        let mut playback = Playback { played: 0 };
        let now = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut playback, now, 1)
            .unwrap();
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    input("旧历史:@1", None, "blue", 0),
                    input("甲:@1", None, "blue", 20),
                ],
            )
            .unwrap();
        let fresh = session.reader.read_playlist_messages().unwrap();
        let message = &fresh[0].0;
        assert!(shared.is_business_pending(&message.id).unwrap());
        assert!(
            session
                .reader
                .accept_playlist_message(&shared, message)
                .unwrap()
        );
        assert!(
            !session
                .reader
                .accept_playlist_message(&shared, message)
                .unwrap()
        );
        let cleanup = session.finish_observation();
        assert!(
            !shared.is_business_pending(&message.id).unwrap(),
            "backlog control consumed before release"
        );
        assert!(
            observations.begin_exclusive().is_ok(),
            "observation ends before playback"
        );
        assert_eq!(
            app.window_len("甲", now).unwrap(),
            1,
            "selection remains until consumed"
        );
        app.execute("甲", &PlaylistCommand::Select(1), &mut playback, now, 1)
            .unwrap();
        assert_eq!(playback.played, 1);
        drop(cleanup);
        assert!(!app.close_active_window("甲", now + WINDOW_TTL).unwrap());
    }

    #[test]
    fn decisions_preserve_blue_color_and_only_owner_can_use_primary_or_secondary_sender() {
        let shared = ChatTimeline::new();
        let observations = ChatObservationShared::new();
        let app = application();
        let mut session = session(&shared, &observations, &app, ChatIdentity::PrimaryHall);
        shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![
                    input("旧历史:@1", None, "blue", 0),
                    input("甲:@1", None, "blue", 20),
                    input("@关闭", Some("乙"), "blue", 40),
                    input("[甲]:@换一批", Some("甲"), "pink", 60),
                    input("@换一批", None, "blue", 80),
                ],
            )
            .unwrap();
        let messages = session.reader.read_playlist_messages().unwrap();
        assert_eq!(messages.len(), 4);
        let primary = playlist_decision_envelope(&messages[0].0, messages[0].1).unwrap();
        assert!(owner_matches("甲", primary.username()));
        assert_eq!(primary.message_type(), "blue");
        assert_eq!(
            playlist_decision(&primary),
            Some(PlaylistCommand::Select(1))
        );
        let other = playlist_decision_envelope(&messages[1].0, messages[1].1).unwrap();
        assert!(!owner_matches("甲", other.username()));
        assert!(playlist_decision_envelope(&messages[2].0, messages[2].1).is_none());
        assert!(playlist_decision_envelope(&messages[3].0, messages[3].1).is_none());
        assert!(owner_matches("甲", " 甲 "));
        assert!(!owner_matches("甲", ""));
    }

    #[test]
    fn successful_decisions_survive_visual_rebuild_but_invalid_index_writes_no_success_lock() {
        for (text, changed) in [
            ("甲:@1", true),
            ("甲:@换一批", true),
            ("甲:@关闭", true),
            ("甲:@5", false),
        ] {
            let shared = ChatTimeline::new();
            let observations = ChatObservationShared::new();
            let app = application();
            let mut session = session(&shared, &observations, &app, ChatIdentity::PrimaryHall);
            let mut playback = Playback { played: 0 };
            let now = Instant::now();
            app.execute("甲", &PlaylistCommand::Open, &mut playback, now, 1)
                .unwrap();
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![
                        input("旧历史:@1", None, "blue", 0),
                        input(text, None, "blue", 20),
                    ],
                )
                .unwrap();
            let fresh = session.reader.read_playlist_messages().unwrap();
            let envelope = playlist_decision_envelope(&fresh[0].0, fresh[0].1).unwrap();
            assert_eq!(envelope.observation().captured_at, Some(fresh[0].1));
            let command = playlist_decision(&envelope).unwrap();
            app.execute("甲", &command, &mut playback, now, 1).unwrap();
            acknowledge_playlist_decision(&shared, &envelope, changed).unwrap();
            let previous_id = fresh[0].0.id.clone();
            shared
                .publish_no_chat(
                    Instant::now(),
                    "真实会话切换的状态事件",
                    crate::observation::chat::ViewAbsence::Transient,
                )
                .unwrap();
            let rebuilt = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![input(text, None, "blue", 20)],
                )
                .unwrap();
            assert_ne!(rebuilt.messages[0].id, previous_id);
            if changed {
                let lock_texts = shared.command_lock_texts_for_test(&ChatIdentity::PrimaryHall);
                assert!(
                    lock_texts.iter().any(|locked| locked == text),
                    "successful decision must write a lock: {text}"
                );
            } else {
                assert!(
                    shared
                        .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
                        .is_empty(),
                    "invalid selection must not write a success lock: {text}"
                );
            }
        }
    }

    #[test]
    fn numeric_selection_is_not_routed_outside_the_playlist_window() {
        let router = crate::interfaces::chat::ChatCommandRouter::without_custom_workflow();
        for (username, color) in [("大厅成员", "blue"), ("好友", "pink")] {
            for text in ["@1", "@2", "@3", "@4", "@5", "@换一批", "@关闭"] {
                let envelope = CommandEnvelope::new(
                    text,
                    username,
                    color,
                    text,
                    CommandObservation::default(),
                )
                .unwrap();
                assert!(
                    router.select_module(&envelope, None).is_none(),
                    "{text} must not be claimed by modules for {color} chat"
                );
                assert!(router.route(&envelope, None).is_none(), "{text}");
            }
        }
    }

    #[test]
    fn unknown_sender_can_be_confirmed_later_but_baseline_never_reappears() {
        let shared = ChatTimeline::new();
        let observations = ChatObservationShared::new();
        let app = application();
        let mut session = session(&shared, &observations, &app, ChatIdentity::SecondaryHall);
        for sender in [None, Some("甲")] {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::SecondaryHall,
                    vec![
                        input("旧历史:@1", Some("甲"), "blue", 0),
                        input("@1", sender, "blue", 20),
                    ],
                )
                .unwrap();
            let messages = session.reader.read_playlist_messages().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(
                playlist_decision_envelope(&messages[0].0, messages[0].1).is_some(),
                sender.is_some()
            );
        }
    }
}

fn playlist_seed() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
