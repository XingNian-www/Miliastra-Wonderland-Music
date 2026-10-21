//! AI 聊天命令的执行侧。
//!
//! 从 `commands.rs` 拆出：AI 聊天横跨"解析提问 → 调模型 → 按提问者权限复核并执行模型给出的命令"，
//! 与其余业务模块的分派逻辑没有共用状态，单独成文件后 `commands.rs` 只剩"一条命令分派到哪个模块"。
//!
//! 安全边界（不要在这里放宽）：模型输出是**不受信输入**。回放命令前必须用提问者本人身份
//! 重建信封并走正常路由，`permission_required` 非空即拒绝——提示词里的可用命令列表只是提示，
//! 不是权限边界。

use super::*;

impl ApplicationRuntime {
    /// AI 聊天：关键词触发已在上游转成 `AiChatCommand::Ask`；
    /// `Forget` 来自 @忘记我。AI 产出的命令按**提问者本人权限**复核后执行。
    pub(super) fn execute_ai_chat_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &crate::features::ai_chat::AiChatCommand,
    ) -> Result<()> {
        use crate::features::ai_chat;

        let actor = self.canonical_actor_name(&parsed.username);
        match command {
            ai_chat::AiChatCommand::Forget => {
                let removed = self.business.ai_chat_memory.forget(&actor)?;
                self.log_executed_command(parsed, "ai_chat_forget")?;
                let text = self.render_reply(
                    crate::features::reply_template::AI_CHAT_FORGET.key,
                    crate::features::reply_template::AI_CHAT_FORGET.default,
                    &crate::features::reply_template::ReplyVars::new(),
                );
                let _ = removed;
                return ApplicationRuntime::reply(self, &text);
            }
            ai_chat::AiChatCommand::Ask { question } => {
                let question = question.trim();
                if question.is_empty() {
                    return Ok(());
                }
                if question.chars().count() > ai_chat::MAX_QUESTION_CHARS {
                    let text = self.render_reply(
                        crate::features::reply_template::AI_CHAT_TOO_LONG.key,
                        crate::features::reply_template::AI_CHAT_TOO_LONG.default,
                        &crate::features::reply_template::ReplyVars::new(),
                    );
                    return ApplicationRuntime::reply(self, &text);
                }
                if !self.business.ai.enabled() {
                    return Ok(());
                }
                let config = self.lifecycle.live_configs.snapshot().ai_chat.clone();
                if !config.enabled {
                    return Ok(());
                }
                if let Err(seconds) = self.business.ai_chat_gate.check(&actor, Instant::now()) {
                    log::debug!("AI 聊天冷却中: actor={actor} 剩余 {seconds}s");
                    return Ok(());
                }

                let context = self
                    .business
                    .ai_chat_memory
                    .context(&actor, config.bot_token_budget)?;
                let allowed = self.ai_chat_allowed_commands(parsed);
                let prompt = ai_chat::build_prompt(&ai_chat::PromptInput {
                    config: &config,
                    question,
                    context: &context,
                    allowed_commands: &allowed,
                });
                // 模型请求最长可等 ai_request_timeout（默认 35s）。正式任务全程持有 UI 租约，
                // 而持租约期间共享观察拿不到帧——不临时放行的话，一次问答就能让聊天扫描停 35 秒。
                // 这里只放行「扫描」这一项能力，UI 输入仍归本线程独占，符合租约语义。
                let reply = {
                    let _scan_permit = self.ui.coordinator.permit_scans_for_current_thread();
                    crate::config::ai::request_json(
                        &self.business.ai_chat_openai,
                        &self.business.ai_connection,
                        &config.model,
                        &config.system_prompt,
                        &prompt,
                        config.max_tokens,
                        self.business.ai_request_timeout,
                    )
                };
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        log::warn!("AI 聊天请求失败: {error:#}");
                        let text = self.render_reply(
                            crate::features::reply_template::AI_CHAT_UNAVAILABLE.key,
                            crate::features::reply_template::AI_CHAT_UNAVAILABLE.default,
                            &crate::features::reply_template::ReplyVars::new(),
                        );
                        return ApplicationRuntime::reply(self, &text);
                    }
                };
                let answer = ai_chat::sanitize_answer(&reply);
                if answer.is_empty() {
                    return Ok(());
                }
                let (text, commands) = ai_chat::split_commands(&answer);
                let now_ms = command_now_ms();
                if !text.is_empty() {
                    self.business.ai_chat_memory.record_turn(
                        &actor,
                        question,
                        &text,
                        config.actor_token_budget,
                        now_ms,
                    )?;
                    self.business.ai_chat_memory.record_bot_line(
                        &text,
                        config.bot_token_budget,
                        now_ms,
                    )?;
                    let lines = crate::text::split_numbered_chat_message("", &text);
                    let refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
                    self.reply_batch(&refs, 0)?;
                }
                self.log_executed_command(parsed, "ai_chat")?;
                self.extract_ai_chat_facts(parsed, &actor, question, &text, &config)?;
                if config.allow_commands {
                    self.run_ai_chat_commands(parsed, &commands)?;
                }
                Ok(())
            }
        }
    }

    /// 当前提问者可以执行的命令名列表，注入提示词，避免模型给出越权命令。
    fn ai_chat_allowed_commands(&self, parsed: &RoutedCommand) -> Vec<String> {
        let catalog = crate::features::command_catalog::all_specs();
        let mut names = Vec::new();
        for spec in catalog {
            let allowed = spec.authorities.is_empty()
                || spec.authorities.iter().any(|authority| {
                    *authority == parsed.authority
                        || (parsed.role.is_some() && *authority == CommandAuthority::Friend)
                });
            if allowed && !spec.hidden {
                names.push(format!("{}{}", command_prefix_char(spec.prefix), spec.name));
            }
        }
        names
    }

    /// 回答后单独调用一次抽取事实；失败只记日志，不影响已经发出的回复。
    fn extract_ai_chat_facts(
        &self,
        parsed: &RoutedCommand,
        actor: &str,
        question: &str,
        answer: &str,
        config: &crate::features::ai_chat::AiChatConfig,
    ) -> Result<()> {
        if answer.trim().is_empty() {
            return Ok(());
        }
        let prompt = format!("玩家说：{question}\n你回答：{answer}");
        // 同一次问答的第二次模型调用，同样不能在持有 UI 租约时干等。
        let reply = match {
            let _scan_permit = self.ui.coordinator.permit_scans_for_current_thread();
            crate::config::ai::request_json(
                &self.business.ai_chat_openai,
                &self.business.ai_connection,
                &config.model,
                &config.extract_prompt,
                &prompt,
                256,
                self.business.ai_request_timeout,
            )
        } {
            Ok(reply) => reply,
            Err(error) => {
                log::debug!("AI 聊天事实抽取失败: {error:#}");
                return Ok(());
            }
        };
        for fact in crate::features::ai_chat::parse_facts(&reply) {
            self.business.ai_chat_memory.add_fact(
                actor,
                &fact,
                config.actor_token_budget,
                command_now_ms(),
            )?;
        }
        let _ = parsed;
        Ok(())
    }

    /// 执行 AI 给出的命令：复用正常路由与执行链路，身份仍是提问者本人。
    fn run_ai_chat_commands(&mut self, parsed: &RoutedCommand, commands: &[String]) -> Result<()> {
        for command in commands {
            // 只接受单个命令；日志里隐藏具体内容以外的东西。
            let envelope = crate::interfaces::chat::parse_command_envelope(
                command,
                &parsed.message_type,
                crate::features::command::CommandObservation::default(),
            );
            let Some(envelope) = envelope else {
                log::info!("AI 聊天给出的命令无法解析，已忽略");
                continue;
            };
            // 用提问者身份替换信封发送者，确保权限、锁与归属都指向真人。
            let spoofed = crate::features::command::CommandEnvelope::new(
                envelope.original_text().to_string(),
                parsed.username.clone(),
                parsed.message_type.clone(),
                envelope.user_command().to_string(),
                envelope.observation().clone(),
            );
            let Some(spoofed) = spoofed else {
                continue;
            };
            let active = self.business.business.active_entertainment()?;
            let router = crate::interfaces::chat::ChatCommandRouter::with_identity(
                &self.business.custom_workflow,
                &self.lifecycle.live_configs.identity,
            );
            let Some(routed) = router.route(&spoofed, active) else {
                log::info!("AI 聊天给出的命令无模块认领，已忽略");
                continue;
            };
            if routed.permission_required.is_some() {
                log::info!("AI 聊天给出的命令超出提问者权限，已拒绝");
                continue;
            }
            // 与真人链路同一道闸门：管理员关掉命令后，AI 转述也不能绕过。
            if self.command_is_disabled(&routed)? {
                log::info!("命令识别已禁用，跳过 AI 转述的命令: {}", routed.raw);
                continue;
            }
            if let Err(error) = self.execute_command(&routed) {
                log::warn!("AI 聊天给出的命令执行失败: {error:#}");
            }
        }
        Ok(())
    }
}

/// 命令前缀字符（用于提示词里展示可用命令）。
fn command_prefix_char(prefix: crate::features::command::CommandPrefix) -> char {
    match prefix {
        crate::features::command::CommandPrefix::At => '@',
        crate::features::command::CommandPrefix::Hash => '#',
        crate::features::command::CommandPrefix::Amp => '&',
    }
}

/// 当前墙钟毫秒，用于记忆表的时间戳。
fn command_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
