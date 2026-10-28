use super::*;

impl ApplicationRuntime {
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
            .lifecycle
            .live_configs
            .identity
            .replace_display_names(message);
        let prefixed;
        let message = if self
            .lifecycle
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
                self.lifecycle.running.clone(),
            );
        }
        match residency {
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
        let mapped = messages
            .iter()
            .map(|message| {
                self.lifecycle
                    .live_configs
                    .identity
                    .replace_display_names(message)
            })
            .collect::<Vec<_>>();
        let messages = mapped.iter().map(String::as_str).collect::<Vec<_>>();
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
                "回复模板 {key} 缺少变量 {:?}，已按空串渲染",
                rendered.missing
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
            log::warn!("回复模板 {} 缺少变量 {:?}", spec.key, rendered.missing);
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
                match self.lifecycle.live_configs.snapshot().ai_chat.ai_reply.mode {
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
                log::debug!("{} 的 AI 回复生成失败，留空继续: {error:#}", spec.key);
                out.text = out.text.replace(&tag, "");
            }
        }
        out
    }

    /// 让 AI 为一条回复生成内容。
    ///
    /// 失败只记 debug：回复本身不该因为 AI 挂掉而不发。
    fn generate_ai_reply(&self, spec_key: &str, template: &str) -> Result<String> {
        let snapshot = self.lifecycle.live_configs.snapshot();
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
        crate::config::ai::request_json(
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
        let store = self.lifecycle.config_store.as_ref()?.lock().ok()?;
        let replies = store.reply_templates().ok()?;
        crate::features::reply_template::stored_override(&replies, key)
    }

    pub(super) fn command_reply_is_muted(&self, key: &str) -> bool {
        let Some(entry) = crate::features::reply_template::command_reply_entry_for_key(key) else {
            return false;
        };
        let Some(store) = self.lifecycle.config_store.as_ref() else {
            return false;
        };
        let Ok(store) = store.lock() else {
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
        // 用户可以在一条回复里写多行：一行发一条消息，超宽的行按聊天宽度硬切。
        let messages = crate::text::split_configured_lines(&text);
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

    fn send_redacted_hall_batch(&self, messages: &[String]) -> Result<()> {
        let mapped = messages
            .iter()
            .map(|message| {
                self.lifecycle
                    .live_configs
                    .identity
                    .replace_display_names(message)
            })
            .collect::<Vec<_>>();
        let refs = mapped.iter().map(String::as_str).collect::<Vec<_>>();
        let delay = self
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

impl CardGameDeliveryPort for ApplicationRuntime {
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
        let rendered = deliveries
            .iter()
            .map(|delivery| {
                let message = match delivery.template.as_ref() {
                    Some(template) => self.render_card_game_template(template),
                    None => Some(delivery.message.clone()),
                };
                // 静音的条目整条剔除：不该出现在好友批次里。
                message.map(|message| (delivery.player.clone(), message))
            })
            .collect::<Vec<_>>();
        // 静音条目已被剔除；后续投递与重试只针对实际要发的那些。
        let rendered: Vec<_> = rendered.into_iter().flatten().collect();
        let messages = rendered
            .iter()
            .map(|(player, message)| FriendMessage::new(player, message))
            .collect::<Vec<_>>();
        match self.send_friend_delivery_batch(&messages)? {
            FriendBatchOutcome::Complete => {
                Ok(crate::features::card_games::CardGameFriendBatchOutcome::Complete)
            }
            FriendBatchOutcome::Failed { retryable, failure } => {
                let mut remaining = deliveries.iter().cloned().zip(rendered).collect::<Vec<_>>();
                let mut retryable_templates = Vec::with_capacity(retryable.len());
                for failed in retryable {
                    let position = remaining
                        .iter()
                        .position(|(delivery, message)| {
                            delivery.player == failed.recipient() && message.1 == failed.message()
                        })
                        .ok_or_else(|| {
                            anyhow!("card-game retry payload did not match its original delivery")
                        })?;
                    retryable_templates.push(remaining.remove(position).0);
                }
                Ok(
                    crate::features::card_games::CardGameFriendBatchOutcome::Failed {
                        retryable: retryable_templates,
                        failure,
                    },
                )
            }
        }
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

impl UndercoverDeliveryPort for ApplicationRuntime {
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
        let rendered = deliveries
            .iter()
            .filter_map(|delivery| {
                self.render_undercover_reply(&delivery.reply)
                    .map(|message| (delivery.recipient.clone(), message))
            })
            .collect::<Vec<_>>();
        let messages = rendered
            .iter()
            .map(|(recipient, message)| FriendMessage::new(recipient, message))
            .collect::<Vec<_>>();
        match self.send_friend_delivery_batch(&messages)? {
            FriendBatchOutcome::Complete => Ok(UndercoverFriendBatchOutcome::Complete),
            FriendBatchOutcome::Failed { retryable, failure } => {
                let mut remaining = deliveries.iter().cloned().zip(rendered).collect::<Vec<_>>();
                let mut keyed_retryable = Vec::with_capacity(retryable.len());
                for failed in retryable {
                    let position = remaining
                        .iter()
                        .position(|(delivery, (_, message))| {
                            delivery.recipient == failed.recipient() && message == failed.message()
                        })
                        .ok_or_else(|| {
                            anyhow!("keyed friend retry payload did not match its original message")
                        })?;
                    keyed_retryable.push(remaining.remove(position).0);
                }
                Ok(UndercoverFriendBatchOutcome::Failed {
                    retryable: keyed_retryable,
                    failure,
                })
            }
        }
    }
}
