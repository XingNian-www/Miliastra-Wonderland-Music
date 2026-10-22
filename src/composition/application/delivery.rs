use super::*;

impl ApplicationRuntime {
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
        let overrides = self
            .lifecycle
            .live_configs
            .replies
            .read()
            .map(|config| config.get(key).map(str::to_string))
            .unwrap_or(None);
        let Some(override_text) = overrides else {
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
    ) -> String {
        use crate::features::turtle_soup::TurtleSoupReplyText;
        match reply {
            TurtleSoupReplyText::Raw(text) => text.clone(),
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
                );
                format!("{label}{index}/{total}：{content}")
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
                );
                let content = self.render_reply_text(content);
                format!("{label}{index}/{total}：{content}")
            }
            TurtleSoupReplyText::Composed { spec, vars, nested } => {
                let nested = nested
                    .iter()
                    .map(|(name, reply)| (*name, self.render_reply_text(reply)))
                    .collect();
                let vars = vars.clone().with_nested(nested);
                self.render_template_text(spec, &vars)
            }
        }
    }

    pub(super) fn render_template_text(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> String {
        let command_override = self.command_reply_text(spec.key);
        let legacy_override = self
            .lifecycle
            .live_configs
            .replies
            .read()
            .ok()
            .and_then(|config| config.get(spec.key).map(str::to_string));
        let override_text = command_override.or(legacy_override);
        let rendered =
            crate::features::reply_template::render(spec, override_text.as_deref(), vars);
        if !rendered.missing.is_empty() {
            log::warn!("回复模板 {} 缺少变量 {:?}", spec.key, rendered.missing);
        }
        rendered.text
    }

    fn command_reply_text(&self, key: &str) -> Option<String> {
        let entry = crate::features::reply_template::command_reply_entry_for_key(key)?;
        if entry.ordered_batch {
            return None;
        }
        let store = self.lifecycle.config_store.as_ref()?.lock().ok()?;
        let replies = store.reply_templates().ok()?;
        replies
            .get(entry.id)?
            .iter()
            .find(|reply| reply.key == key)
            .map(|reply| reply.text.clone())
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
        replies.get(entry.id).is_some_and(|rows| {
            rows.is_empty() || (!entry.ordered_batch && !rows.iter().any(|reply| reply.key == key))
        })
    }

    /// 渲染并直接发送一条模板回复。
    ///
    /// 名字与各端口 trait 的 `reply_template` 不同，避免同类型上固有方法与 trait 方法同名。
    pub(super) fn reply_with_template(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        if self.command_reply_is_muted(spec.key) {
            return Ok(());
        }
        let text = self.render_template_text(spec, vars);
        self.reply(&text)
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

    fn render_undercover_reply(
        &self,
        reply: &crate::features::undercover::UndercoverReply,
    ) -> String {
        match reply {
            crate::features::undercover::UndercoverReply::Raw(text) => text.clone(),
            crate::features::undercover::UndercoverReply::Keyed { spec, vars } => {
                self.render_template_text(spec, vars)
            }
            crate::features::undercover::UndercoverReply::Composed {
                spec,
                vars,
                fragments,
            } => {
                let nested = fragments
                    .iter()
                    .map(|(name, reply)| (*name, self.render_undercover_reply(reply)))
                    .collect();
                let vars = vars.clone().with_nested(nested);
                self.render_template_text(spec, &vars)
            }
            crate::features::undercover::UndercoverReply::Joined { fragments } => fragments
                .iter()
                .map(|reply| self.render_undercover_reply(reply))
                .collect::<Vec<_>>()
                .join(""),
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
        let rendered = match template {
            crate::features::card_games::CardGameReplyTemplate::Template { spec, vars } => {
                self.render_template_text(spec, vars)
            }
            crate::features::card_games::CardGameReplyTemplate::Raw(text) => text.clone(),
        };
        let _ = message;
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
                    Some(crate::features::card_games::CardGameReplyTemplate::Template {
                        spec,
                        vars,
                    }) => self.render_template_text(spec, vars),
                    Some(crate::features::card_games::CardGameReplyTemplate::Raw(text)) => {
                        text.clone()
                    }
                    None => delivery.message.clone(),
                };
                (delivery.player.clone(), message)
            })
            .collect::<Vec<_>>();
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
        let rendered = match template {
            crate::features::card_games::CardGameReplyTemplate::Template { spec, vars } => {
                self.render_template_text(spec, vars)
            }
            crate::features::card_games::CardGameReplyTemplate::Raw(text) => text.clone(),
        };
        let _ = message;
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
        let message = self.render_undercover_reply(reply);
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
        let message = self.render_undercover_reply(reply);
        self.reply(&message)
    }

    fn send_keyed_hall_batch(
        &self,
        replies: &[crate::features::undercover::UndercoverReply],
    ) -> Result<()> {
        let messages = replies
            .iter()
            .map(|reply| self.render_undercover_reply(reply))
            .collect::<Vec<_>>();
        self.send_redacted_hall_batch(&messages)
    }

    fn send_keyed_friend_batch(
        &self,
        deliveries: &[crate::features::undercover::UndercoverFriendDelivery],
    ) -> Result<crate::features::undercover::UndercoverFriendBatchOutcome> {
        use crate::features::undercover::UndercoverFriendBatchOutcome;
        let rendered = deliveries
            .iter()
            .map(|delivery| {
                (
                    delivery.recipient.clone(),
                    self.render_undercover_reply(&delivery.reply),
                )
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
