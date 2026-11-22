use super::*;
use crate::features::friend_delivery::FriendBatchFailure;

/// 日志里的缺失变量名：用「」包住并按顿号串起，避免打印 Vec 的调试格式。
///
/// `pub(super)`：`commands.rs` 与 `workers.rs` 的同类告警也复用它，
/// 保证「模板缺少变量」这一类事件只有一种写法。
pub(super) fn missing_vars_label(missing: &[String]) -> String {
    missing
        .iter()
        .map(|name| format!("「{name}」"))
        .collect::<Vec<_>>()
        .join("、")
}

impl AppServices {
    /// 渲染一条歌单回复；带 key 的条目在此处才查用户覆盖与静音。
    ///
    /// 返回 Vec 是因为静音会让整条回复消失（零条消息）。
    pub(super) fn render_playlist_reply(
        &self,
        reply: &crate::features::playlist::PlaylistReply,
    ) -> Vec<String> {
        use crate::features::playlist::PlaylistReply;
        match reply {
            PlaylistReply::Raw(text) => vec![text.clone()],
            PlaylistReply::Keyed { spec, vars } => {
                self.render_template_text(spec, vars).into_iter().collect()
            }
        }
    }

    /// 渲染一条塔罗回复；带 key 的条目在此处才查用户覆盖与静音。
    pub(super) fn render_tarot_reply(
        &self,
        reply: &crate::features::tarot::TarotReply,
    ) -> Vec<String> {
        use crate::features::tarot::TarotReply;
        match reply {
            TarotReply::Raw(text) => vec![text.clone()],
            TarotReply::Keyed { spec, vars } => {
                self.render_template_text(spec, vars).into_iter().collect()
            }
            TarotReply::KeyedFitted { label, spec, vars } => {
                match self.render_template_text(spec, vars) {
                    Some(text) => crate::text::fit_chat_message(label, &text),
                    None => Vec::new(),
                }
            }
        }
    }

    pub(super) fn reply(&self, message: &str) -> Result<()> {
        self.reply_with_ready_gate(message, false)
    }

    pub(super) fn reply_invite_ready(&self, message: &str) -> Result<()> {
        self.reply_with_ready_gate(message, true)
    }

    fn reply_with_ready_gate(&self, message: &str, invite_ready: bool) -> Result<()> {
        let mapped = self
            .shared
            .live_configs
            .identity
            .replace_display_names(message);
        let prefixed;
        let message = if self
            .shared
            .console_reply_context
            .load(AtomicOrdering::SeqCst)
            && !mapped.starts_with("[控制台]:")
        {
            prefixed = format!("[控制台]: {}", mapped);
            prefixed.as_str()
        } else {
            mapped.as_str()
        };
        let residency = self.active_ui_residency()?;
        if invite_ready {
            let target = match residency {
                UiResidency::Primary => crate::ui::routines::UiResidencyTarget::Primary,
                UiResidency::SecondaryCurrentHall => {
                    crate::ui::routines::UiResidencyTarget::SecondaryCurrentHall
                }
            };
            return self.ui.chat_output.send_invite_ready(
                message,
                target,
                self.shared.running.clone(),
            );
        }
        // 表情标签先解析成独立消息，再进入按宽度切分/截断的路径。
        let emoji_parts = crate::features::emoji::expand_reply_for_sending(message);
        if emoji_parts.len() > 1 {
            let refs = emoji_parts.iter().map(String::as_str).collect::<Vec<_>>();
            return self.reply_batch(&refs, 0);
        }
        // 正文超宽时按显示宽度切段（复用 task-37 的 wrapped 入口）；
        // 必须单行的系统通告仍走原单条路径，保留 invite/microphone/colon 压缩语义。
        let delay_ms = self
            .shared
            .live_configs
            .snapshot()
            .timing
            .command
            .help_batch_ms;
        if crate::text::chat_layout::is_single_line_notice(message) {
            return match residency {
                UiResidency::Primary => self.ui.chat_output.send_for_command(message),
                UiResidency::SecondaryCurrentHall => self.ui.chat_output.send_current_chat(message),
            };
        }
        match residency {
            UiResidency::Primary => self.ui.chat_output.send_wrapped(message, delay_ms),
            UiResidency::SecondaryCurrentHall => self
                .ui
                .chat_output
                .send_current_chat_wrapped(message, delay_ms),
        }
    }

    pub(super) fn reply_batch(&self, messages: &[&str], _delay_ms: u64) -> Result<()> {
        let delay_ms = self
            .shared
            .live_configs
            .snapshot()
            .timing
            .command
            .help_batch_ms;
        // 正文先按显示宽度展开（每行标点优先、无编号、带段数上限）；
        // 必须单行的系统通告保持原样，继续走 fit_* 兜底。
        let expanded: Vec<String> = messages
            .iter()
            .flat_map(|message| {
                let mapped = self
                    .shared
                    .live_configs
                    .identity
                    .replace_display_names(message);
                // 先解析表情标签再换行：标签单独成条，永远不参与宽度切分/截断。
                crate::features::emoji::expand_reply_for_sending(&mapped)
                    .into_iter()
                    .flat_map(|piece| {
                        if crate::text::chat_layout::is_single_line_notice(&piece) {
                            vec![piece]
                        } else {
                            crate::text::chat_layout::expand_chat_segments(
                                &piece,
                                crate::text::chat_layout::MAX_SEGMENTS_PER_REPLY,
                            )
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let expanded = crate::text::chat_layout::cap_chat_segments(
            expanded,
            crate::text::chat_layout::MAX_SEGMENTS_PER_BATCH,
        );
        if expanded.is_empty() {
            return Ok(());
        }
        let messages = expanded.iter().map(String::as_str).collect::<Vec<_>>();
        match self.active_ui_residency()? {
            UiResidency::Primary => self
                .ui
                .chat_output
                .send_batch_for_command(&messages, delay_ms),
            UiResidency::SecondaryCurrentHall => self
                .ui
                .chat_output
                .send_current_chat_batch(&messages, delay_ms),
        }
    }

    /// 把一条已渲染的业务文案按模板键换成用户配置的版本。
    ///
    /// 调用点在回复收口处：默认配置为空表，此时直接返回原样文案，
    /// **不改变任何现有行为**；只有用户在配置中心为某个键写了覆盖才替换。
    ///
    /// 变量按"已知键"逐个注入：调用方给出的变量在渲染时才生效，
    /// 因此同一条默认文案在不同调用点可以有不同取值。
    pub(super) fn render_reply(
        &self,
        key: &'static str,
        // 必须是 'static：ReplyTemplate::default 要求它活到程序结束，
        // 因为它会被登记进目录、供面板展示，不能指向调用栈上的临时 String。
        default_text: &'static str,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> String {
        let Some(override_text) = self.command_reply_text(key) else {
            // 未配置覆盖：原样返回调用方已经组织好的文案。
            return default_text.to_string();
        };
        let spec = crate::features::reply_template::ReplyTemplate {
            key,
            default: default_text,
            summary: "",
            group: "",
        };
        let rendered = crate::features::reply_template::render(&spec, Some(&override_text), vars);
        if !rendered.missing.is_empty() {
            log::warn!(
                "回复模板「{key}」缺少变量 {}，这些位置已按空串渲染，玩家会看到内容缺一段",
                missing_vars_label(&rendered.missing)
            );
        }
        rendered.text
    }

    /// 按模板键渲染一条回复，并查配置中心的覆盖文案。
    ///
    /// 这是组装层唯一的模板渲染入口：`render_reply`（键+默认值）与各业务端口的
    /// `reply_template`/`render_template`（模板结构）都汇到这里，避免每个端口各抄一遍
    /// "查覆盖 → 渲染 → 记录缺变量" 的流程，也就不会再出现某条路径漏查覆盖。
    pub(super) fn render_reply_text(
        &self,
        reply: &crate::features::turtle_soup::TurtleSoupReplyText,
    ) -> Option<String> {
        use crate::features::turtle_soup::TurtleSoupReplyText;
        match reply {
            TurtleSoupReplyText::Raw(text) => Some(text.clone()),
            TurtleSoupReplyText::Keyed { spec, vars } => self.render_template_text(spec, vars),
            TurtleSoupReplyText::Numbered {
                label,
                index,
                total,
                content,
            } => {
                let label = self.render_template_text(
                    label,
                    &crate::features::reply_template::ReplyVars::new(),
                )?;
                Some(format!("{label}{index}/{total}：{content}"))
            }
            TurtleSoupReplyText::NumberedComposed {
                label,
                index,
                total,
                content,
            } => {
                let label = self.render_template_text(
                    label,
                    &crate::features::reply_template::ReplyVars::new(),
                )?;
                let content = self.render_reply_text(content)?;
                Some(format!("{label}{index}/{total}：{content}"))
            }
            TurtleSoupReplyText::Composed { spec, vars, nested } => {
                // 任一嵌套片段被静音，整条复合回复就不发：半个句子没有意义。
                let mut resolved = Vec::with_capacity(nested.len());
                for (name, reply) in nested {
                    resolved.push((*name, self.render_reply_text(reply)?));
                }
                let vars = vars.clone().with_nested(resolved);
                self.render_template_text(spec, &vars)
            }
        }
    }

    /// 按模板渲染一条文案；被静音时返回 None。
    ///
    /// 静音判定放在这里而不是各调用点：此前 21 个发送点里只有 5 个自己查了静音，
    /// 其余路径（牌类、卧底、海龟汤片段、塔罗、歌单）的静音根本不生效。
    /// 现在返回类型强制每个调用方处理"这条不该发"的情况，
    /// 新增发送路径时也不可能漏掉静音。
    pub(super) fn render_template_text(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Option<String> {
        if self.command_reply_is_muted(spec.key) {
            return None;
        }
        // 覆盖只来自命令面板保存的行；配置中心不再提供回复文案入口。
        let override_text = self.command_reply_text(spec.key);
        let rendered =
            crate::features::reply_template::render(spec, override_text.as_deref(), vars);
        // 先渲染出真实内容，再让 AI 看这段"已经发生的事实"——
        // 直接把未渲染的模板交给模型，它只能看到占位符，等于瞎编。
        let rendered = self.fill_ai_reply(spec, rendered);
        if !rendered.missing.is_empty() {
            log::warn!(
                "回复模板「{}」缺少变量 {}，这些位置已按空串渲染，玩家会看到内容缺一段",
                spec.key,
                missing_vars_label(&rendered.missing)
            );
        }
        Some(rendered.text)
    }

    /// 把渲染结果里的 `{AI回复}` 换成 AI 生成的内容。
    ///
    /// 顺序很关键：**先渲染再生成**。模板里 `{AI回复}` 之外的变量已经填好，
    /// 模型看到的是「小明点播的《晴天》已排上」而不是「{玩家}点播的{歌名}」。
    fn fill_ai_reply(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        rendered: crate::features::reply_template::RenderedReply,
    ) -> crate::features::reply_template::RenderedReply {
        let tag = format!("{{{}}}", crate::features::ai_tag::AI_TAG);
        if !rendered.text.contains(&tag) {
            return rendered;
        }
        let mut out = rendered;
        match self.generate_ai_reply(spec.key, &out.text) {
            Ok(text) => {
                let text = text.trim();
                match self.shared.live_configs.snapshot().ai_chat.ai_reply.mode {
                    // 补充：保留模板，只把标签换成那句话。
                    crate::features::ai_chat::AiReplyMode::Append => {
                        out.text = out.text.replace(&tag, text);
                    }
                    // 改写：整条换成 AI 的版本。模板只作为参考输入。
                    crate::features::ai_chat::AiReplyMode::Rewrite => {
                        out.text = text.to_string();
                    }
                }
            }
            Err(error) => {
                // 生成失败就删掉占位符，别让花括号发到游戏里。
                log::debug!(
                    "回复模板「{}」的 AI 回复生成失败，该位置留空继续发送，玩家看到的是模板其余部分：{error:#}",
                    spec.key
                );
                out.text = out.text.replace(&tag, "");
            }
        }
        out
    }

    /// 让 AI 为一条回复生成内容。
    ///
    /// 失败只记 debug：回复本身不该因为 AI 挂掉而不发。
    fn generate_ai_reply(&self, spec_key: &str, template: &str) -> Result<String> {
        let snapshot = self.shared.live_configs.snapshot();
        let config = snapshot.ai_chat.clone();
        if !config.enabled {
            anyhow::bail!("AI 聊天未启用");
        }
        let reply = &config.ai_reply;
        let command = crate::features::reply_template::command_reply_entry_for_key(spec_key)
            .map(|entry| entry.name)
            .unwrap_or_default();
        let user = reply.render_user_prompt(&command, template);
        let timeout = std::time::Duration::from_secs(reply.timeout_seconds.max(1));
        // 不能在持有 UI 租约时干等网络。
        let _scan_permit = self.ui.coordinator.permit_scans_for_current_thread();
        // **必须走纯文本通道**：模板要的是一句自然语言聊天文本，提示词里既没有
        // `json` 要求、返回值也不做 JSON 解析。`request_json` 会强制
        // response_format=JsonObject：严格接口下必然 400（表现为 `{AI回复}`
        // 位置永远空着），兼容接口下会把整段 JSON 原样发进游戏大厅。
        crate::config::ai::request_plain_text(
            &self.business.ai_chat_openai,
            &self.business.ai_connection,
            &crate::config::ai::AiCallOptions {
                model: config.model.clone(),
                reasoning: config.reasoning,
            },
            &reply.system_prompt,
            &user,
            reply.max_tokens,
            timeout,
        )
    }

    pub(super) fn command_reply_text(&self, key: &str) -> Option<String> {
        let store = self.shared.config_store.lock().ok()?;
        let replies = store.reply_templates().ok()?;
        crate::features::reply_template::stored_override(&replies, key)
    }

    pub(super) fn command_reply_is_muted(&self, key: &str) -> bool {
        let Some(entry) = crate::features::reply_template::command_reply_entry_for_key(key) else {
            return false;
        };
        let Ok(store) = self.shared.config_store.lock() else {
            return false;
        };
        let Ok(replies) = store.reply_templates() else {
            return false;
        };
        replies
            .get(entry.id)
            .is_some_and(|rows| rows.is_empty() || !rows.iter().any(|reply| reply.key == key))
    }

    /// 渲染并直接发送一条模板回复。
    ///
    /// 名字与各端口 trait 的 `reply_template` 不同，避免同类型上固有方法与 trait 方法同名。
    pub(super) fn reply_with_template(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        // 静音由 render_template_text 统一处理：None 表示这条不该发。
        let Some(text) = self.render_template_text(spec, vars) else {
            return Ok(());
        };
        // 用户可以在一条回复里写多行；正文按显示宽度标点优先切段（一行=一条、
        // 无编号、带段数上限）。必须单行的系统通告仍沿用原来的硬切/压缩路径。
        let messages = if crate::text::chat_layout::is_single_line_notice(&text) {
            crate::text::split_configured_lines(&text)
        } else {
            crate::text::chat_layout::expand_chat_segments(
                &text,
                crate::text::chat_layout::MAX_SEGMENTS_PER_REPLY,
            )
        };
        if messages.is_empty() {
            return Ok(());
        }
        if messages.len() == 1 {
            return self.reply(&messages[0]);
        }
        let refs: Vec<&str> = messages.iter().map(String::as_str).collect();
        self.reply_batch(&refs, 0)
    }

    pub(super) fn update_monitor_playback_controller(&self) {
        self.shared
            .monitor
            .publish(MonitorEvent::PlaybackController(
                self.playback.player.snapshot(),
            ));
    }

    pub(super) fn update_monitor_operational_state(&self) {
        self.shared.monitor.publish(MonitorEvent::ScannerPaused(
            self.shared.paused.load(AtomicOrdering::SeqCst),
        ));
    }

    fn send_redacted_hall_batch(&self, messages: &[String]) -> Result<()> {
        let mapped = messages
            .iter()
            .map(|message| {
                self.shared
                    .live_configs
                    .identity
                    .replace_display_names(message)
            })
            .collect::<Vec<_>>();
        let refs = mapped.iter().map(String::as_str).collect::<Vec<_>>();
        let delay = self
            .shared
            .live_configs
            .snapshot()
            .timing
            .command
            .help_batch_ms;
        match self.active_ui_residency()? {
            UiResidency::Primary => self
                .ui
                .chat_output
                .send_batch_for_command_redacted(&refs, delay),
            UiResidency::SecondaryCurrentHall => self
                .ui
                .chat_output
                .send_current_chat_batch_redacted(&refs, delay),
        }
    }

    /// 渲染卧底回复；静音时返回 None。
    /// 渲染牌类回复模板；内层模板同样解析覆盖与静音。
    fn render_card_game_template(
        &self,
        template: &crate::features::card_games::CardGameReplyTemplate,
    ) -> Option<String> {
        use crate::features::card_games::CardGameReplyTemplate;
        match template {
            CardGameReplyTemplate::Template { spec, vars } => self.render_template_text(spec, vars),
            CardGameReplyTemplate::Composed { spec, vars, nested } => {
                let mut resolved = Vec::with_capacity(nested.len());
                for (name, reply) in nested {
                    resolved.push((*name, self.render_card_game_template(reply)?));
                }
                self.render_template_text(spec, &vars.clone().with_nested(resolved))
            }
            CardGameReplyTemplate::Raw(text) => Some(text.clone()),
        }
    }

    fn render_undercover_reply(
        &self,
        reply: &crate::features::undercover::UndercoverReply,
    ) -> Option<String> {
        match reply {
            crate::features::undercover::UndercoverReply::Raw(text) => Some(text.clone()),
            crate::features::undercover::UndercoverReply::Keyed { spec, vars } => {
                self.render_template_text(spec, vars)
            }
            crate::features::undercover::UndercoverReply::Composed {
                spec,
                vars,
                fragments,
            } => {
                let mut resolved = Vec::with_capacity(fragments.len());
                for (name, reply) in fragments {
                    resolved.push((*name, self.render_undercover_reply(reply)?));
                }
                self.render_template_text(spec, &vars.clone().with_nested(resolved))
            }
            crate::features::undercover::UndercoverReply::Joined { fragments } => {
                // 任一细分片被静音，整条拼接就不发。
                let mut joined = String::new();
                for reply in fragments {
                    joined.push_str(&self.render_undercover_reply(reply)?);
                }
                Some(joined)
            }
        }
    }
}

/// 一次好友批量投递的骨架：渲染、剔除静音、发送，并把失败重试项对回原始条目。
///
/// **配对必须在这一个循环里完成**。先过滤出「要发的消息」，失败时再拿
/// **未过滤**的 `deliveries` 去 `zip` 那份缩减后的列表，会从第一条静音条目
/// 开始整体错位：轻则报「重试项匹配不到原始投递」，重则把另一个玩家的模板
/// 当成重试对象重新发一遍。
///
/// 返回值：`Ok(None)` 表示全部送达（或整批静音、无可发送）；`Ok(Some(..))`
/// 是仍需重试的原始条目与失败原因。
fn deliver_friend_batch<T: Clone>(
    deliveries: &[T],
    mut render: impl FnMut(&T) -> Option<String>,
    recipient_of: impl Fn(&T) -> &str,
    send: impl FnOnce(&[FriendMessage]) -> Result<FriendBatchOutcome>,
) -> Result<Option<(Vec<T>, FriendBatchFailure)>> {
    // 一条原始条目配一条渲染后的正文：静音条目（None）在这里被剔除，
    // 剩下每一项都还记得自己来自哪一条原始投递。
    let mut pairs: Vec<(T, String)> = Vec::with_capacity(deliveries.len());
    for delivery in deliveries {
        if let Some(message) = render(delivery) {
            pairs.push((delivery.clone(), message));
        }
    }
    if pairs.is_empty() {
        // 整批都被静音：没有要发的东西，对调用方算「已处理」。
        return Ok(None);
    }
    let messages = pairs
        .iter()
        .map(|(delivery, message)| FriendMessage::new(recipient_of(delivery), message.as_str()))
        .collect::<Vec<_>>();
    match send(&messages)? {
        FriendBatchOutcome::Complete => Ok(None),
        FriendBatchOutcome::Failed { retryable, failure } => {
            let mut retryable_deliveries = Vec::with_capacity(retryable.len());
            for failed in retryable {
                let position = pairs
                    .iter()
                    .position(|(delivery, message)| {
                        recipient_of(delivery) == failed.recipient() && message == failed.message()
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "好友批量投递的重试项匹配不到原始投递条目，本次重试已中止；重试项与静音过滤后的待发列表必须一一对应"
                        )
                    })?;
                let (delivery, _) = pairs.remove(position);
                retryable_deliveries.push(delivery);
            }
            Ok(Some((retryable_deliveries, failure)))
        }
    }
}

impl CardGameDeliveryPort for AppServicesPort<'_> {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_unique_friend_message(player, message)
    }

    fn send_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_friend_message(player, message)
    }

    fn send_friend_template(
        &self,
        player: &str,
        message: &str,
        template: &crate::features::card_games::CardGameReplyTemplate,
    ) -> Result<bool> {
        let rendered = self.render_card_game_template(template);
        let _ = message;
        // 静音：这条私聊不发送，但对调用方仍算"已处理"。
        let Some(rendered) = rendered else {
            return Ok(true);
        };
        self.send_friend_message(player, &rendered)
    }

    fn send_friend_batch_templates(
        &self,
        deliveries: &[LandlordPrivateDelivery],
    ) -> Result<crate::features::card_games::CardGameFriendBatchOutcome> {
        use crate::features::card_games::CardGameFriendBatchOutcome;
        // 静音条目在配对阶段整条剔除；重试项仍然对回它自己的原始投递。
        let outcome = deliver_friend_batch(
            deliveries,
            |delivery| match delivery.template.as_ref() {
                Some(template) => self.render_card_game_template(template),
                None => Some(delivery.message.clone()),
            },
            |delivery| delivery.player.as_str(),
            |messages| self.send_friend_delivery_batch(messages),
        )?;
        Ok(match outcome {
            None => CardGameFriendBatchOutcome::Complete,
            Some((retryable, failure)) => CardGameFriendBatchOutcome::Failed { retryable, failure },
        })
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

    fn send_hall_template(
        &self,
        message: &str,
        template: &crate::features::card_games::CardGameReplyTemplate,
    ) -> Result<()> {
        let rendered = self.render_card_game_template(template);
        let _ = message;
        let Some(rendered) = rendered else {
            return Ok(());
        };
        self.reply(&rendered)
    }
}

impl UndercoverDeliveryPort for AppServicesPort<'_> {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_stable_unique_friend_message(player, message)
    }

    fn verify_keyed_friend(
        &self,
        player: &str,
        reply: &crate::features::undercover::UndercoverReply,
    ) -> Result<bool> {
        // 静音：不发送，但对调用方仍算"已处理"。
        let Some(message) = self.render_undercover_reply(reply) else {
            return Ok(true);
        };
        self.send_stable_unique_friend_message(player, &message)
    }

    fn send_friend_batch(&self, deliveries: &[FriendMessage]) -> Result<FriendBatchOutcome> {
        self.send_friend_delivery_batch(deliveries)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn send_hall_batch(&self, messages: &[String]) -> Result<()> {
        self.send_redacted_hall_batch(messages)
    }

    fn send_keyed_hall(&self, reply: &crate::features::undercover::UndercoverReply) -> Result<()> {
        let Some(message) = self.render_undercover_reply(reply) else {
            return Ok(());
        };
        self.reply(&message)
    }

    fn send_keyed_hall_batch(
        &self,
        replies: &[crate::features::undercover::UndercoverReply],
    ) -> Result<()> {
        // 静音条目在批量里直接跳过。
        let messages = replies
            .iter()
            .filter_map(|reply| self.render_undercover_reply(reply))
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        self.send_redacted_hall_batch(&messages)
    }

    fn send_keyed_friend_batch(
        &self,
        deliveries: &[crate::features::undercover::UndercoverFriendDelivery],
    ) -> Result<crate::features::undercover::UndercoverFriendBatchOutcome> {
        use crate::features::undercover::UndercoverFriendBatchOutcome;
        // 与牌类批次同一条骨架：静音剔除与重试对齐必须在同一个循环里完成。
        let outcome = deliver_friend_batch(
            deliveries,
            |delivery| self.render_undercover_reply(&delivery.reply),
            |delivery| delivery.recipient.as_str(),
            |messages| self.send_friend_delivery_batch(messages),
        )?;
        Ok(match outcome {
            None => UndercoverFriendBatchOutcome::Complete,
            Some((retryable, failure)) => {
                UndercoverFriendBatchOutcome::Failed { retryable, failure }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::deliver_friend_batch;
    use crate::features::card_games::LandlordPrivateDelivery;
    use crate::features::friend_delivery::{
        FriendBatchFailure, FriendBatchFailureKind, FriendBatchOutcome, FriendMessage,
    };

    fn private_delivery(player: &str, message: &str) -> LandlordPrivateDelivery {
        LandlordPrivateDelivery {
            player: player.to_string(),
            message: message.to_string(),
            template: None,
        }
    }

    fn confirmed_unsent(failed: FriendMessage) -> FriendBatchOutcome {
        FriendBatchOutcome::Failed {
            retryable: vec![failed],
            failure: FriendBatchFailure::new(FriendBatchFailureKind::ConfirmedUnsent, "测试失败"),
        }
    }

    /// 回归：批次里有静音条目时，重试项必须仍指向它自己的原始投递。
    ///
    /// 旧实现先过滤出「要发的消息」，失败时再用**未过滤**的 `deliveries` 做
    /// `zip`。只要中间有一条静音，后面全体左移：这条用例里第三个玩家会被
    /// 错配成第二个（静音的）玩家的投递，重试就把别人的模板发给另一个人。
    #[test]
    fn muted_entries_do_not_shift_the_retry_alignment() {
        let deliveries = vec![
            private_delivery("甲", "第一条"),
            private_delivery("乙", "静音那条"),
            private_delivery("丙", "第三条"),
        ];
        let sent = std::cell::RefCell::new(Vec::<FriendMessage>::new());
        let outcome = deliver_friend_batch(
            &deliveries,
            // 乙被静音：整条剔除。
            |delivery| (delivery.player != "乙").then(|| delivery.message.clone()),
            |delivery| delivery.player.as_str(),
            |messages| {
                sent.borrow_mut().extend(messages.iter().cloned());
                Ok(confirmed_unsent(FriendMessage::new("丙", "第三条")))
            },
        )
        .expect("批量投递骨架不应报错");

        let sent = sent.borrow();
        let sent_pairs = sent
            .iter()
            .map(|message| (message.recipient(), message.message()))
            .collect::<Vec<_>>();
        assert_eq!(
            sent_pairs,
            vec![("甲", "第一条"), ("丙", "第三条")],
            "静音条目必须整条剔除，实际送达：{sent_pairs:?}"
        );

        let (retryable, _failure) = outcome.expect("有可重试失败时必须返回重试项");
        assert_eq!(
            retryable
                .iter()
                .map(|delivery| (delivery.player.as_str(), delivery.message.as_str()))
                .collect::<Vec<_>>(),
            vec![("丙", "第三条")],
            "重试项必须是「丙」自己的投递，不能被静音条目挤偏"
        );
    }

    /// 整批静音时不提交空批次：一次发送都不该发生。
    #[test]
    fn a_fully_muted_batch_sends_nothing() {
        let deliveries = vec![private_delivery("甲", "第一条")];
        let outcome = deliver_friend_batch(
            &deliveries,
            |_delivery| None,
            |delivery| delivery.player.as_str(),
            |_messages| panic!("整批静音时不得提交任何好友投递"),
        )
        .expect("整批静音不是错误");
        assert!(outcome.is_none(), "整批静音时对调用方算「已处理」");
    }

    /// 重试项必须能在待发列表里找到原条目，否则宁可报错也不能错配。
    #[test]
    fn a_retry_item_without_an_original_is_rejected() {
        let deliveries = vec![private_delivery("甲", "第一条")];
        let error = deliver_friend_batch(
            &deliveries,
            |delivery| Some(delivery.message.clone()),
            |delivery| delivery.player.as_str(),
            |_messages| Ok(confirmed_unsent(FriendMessage::new("甲", "另一条"))),
        )
        .expect_err("重试项对不上原始投递时必须报错");
        let text = format!("{error:#}");
        assert!(
            text.contains("匹配不到原始投递条目"),
            "错误必须说明重试项与原始投递对不上，实际：{text}"
        );
    }

    /// 接线锁定：`{AI回复}` 必须走纯文本通道。
    ///
    /// 这条路径的回归不会报错、不会 panic，只会让含 `{AI回复}` 的模板在严格
    /// 接口上 100% 400、在兼容接口上把整段 JSON 发进游戏大厅，因此只能盯源码：
    /// 生成函数里必须是 `request_plain_text`，不得出现 `request_json`。
    #[test]
    fn ai_reply_generation_uses_the_plain_text_channel() {
        const SOURCE: &str = include_str!("delivery.rs");
        let body = SOURCE
            .split("fn generate_ai_reply(")
            .nth(1)
            .expect("必须保留 generate_ai_reply");
        let body = &body[..body.find("\n    }").expect("函数体必须闭合")];
        // 注释里**必须**留着 `request_json` 这个词（那是这条缺陷的由来），
        // 所以只看代码行。
        let code = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("request_plain_text"),
            "生成 {{AI回复}} 内容必须走 request_plain_text，实际函数体：{code}"
        );
        assert!(
            !code.contains("request_json"),
            "生成 {{AI回复}} 内容不得走 request_json（会强制 JSON 输出），实际函数体：{code}"
        );
    }
}
