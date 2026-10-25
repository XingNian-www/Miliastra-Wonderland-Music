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
                let tools = if config.allow_commands {
                    ai_chat::command_tools(&allowed)
                } else {
                    Vec::new()
                };
                let result = (|| -> Result<String> {
                    let mut session = crate::config::ai::ToolSession::with_history_options(
                        &self.business.ai_connection,
                        &crate::config::ai::AiCallOptions {
                            model: config.model.clone(),
                            reasoning: config.reasoning,
                        },
                        &ai_chat::response_system_prompt(&config),
                        ai_chat::conversation_history(&context),
                        &prompt,
                        tools.clone(),
                    )?;
                    let deadline = Instant::now() + self.business.ai_request_timeout;
                    let mut executed = std::collections::HashSet::new();
                    for round in 0..=ai_chat::MAX_TOOL_ROUNDS {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        anyhow::ensure!(!remaining.is_zero(), "AI 工具调用总期限已到");
                        let allow_tools = !tools.is_empty() && round < ai_chat::MAX_TOOL_ROUNDS;
                        // 等待模型期间只放行共享扫描，UI 输入仍由当前正式任务独占。
                        let response = {
                            let _scan_permit =
                                self.ui.coordinator.permit_scans_for_current_thread();
                            session.request(
                                &self.business.ai_chat_openai,
                                config.max_tokens,
                                remaining,
                                allow_tools,
                            )?
                        };
                        if response.calls.is_empty() {
                            let text = ai_chat::sanitize_answer(&response.text);
                            anyhow::ensure!(!text.is_empty(), "AI 未返回最终回复");
                            return Ok(text);
                        }
                        anyhow::ensure!(allow_tools, "AI 在工具关闭后仍请求执行");
                        for call in response.calls {
                            let outcome = if call.name != ai_chat::COMMAND_TOOL {
                                serde_json::json!({"status":"rejected","reason":"未知工具"})
                            } else if let Ok(execution) = ai_chat::parse_execution(&call.arguments)
                            {
                                if !executed.insert(execution.command.clone()) {
                                    serde_json::json!({"status":"rejected","reason":"同一问答内禁止重复执行相同命令"})
                                } else if executed.len() > ai_chat::MAX_TOOL_ROUNDS {
                                    serde_json::json!({"status":"rejected","reason":"已达到命令执行次数上限"})
                                } else {
                                    self.run_ai_chat_command(parsed, &execution.command)
                                        .unwrap_or_else(|error| {
                                            log::warn!("AI 工具执行结果未确认: {error:#}");
                                            serde_json::json!({"status":"unknown","reason":"执行异常，不得重试或声称成功"})
                                        })
                                }
                            } else {
                                serde_json::json!({"status":"rejected","reason":"命令参数不符合工具 Schema"})
                            };
                            session.append_result(&call.id, &outcome.to_string())?;
                        }
                    }
                    anyhow::bail!("AI 工具循环未产生最终回复")
                })();
                let text = match result {
                    Ok(text) => text,
                    Err(error) => {
                        log::warn!("AI 聊天工具调用失败: {error:#}");
                        let text = self.render_reply(
                            crate::features::reply_template::AI_CHAT_UNAVAILABLE.key,
                            crate::features::reply_template::AI_CHAT_UNAVAILABLE.default,
                            &crate::features::reply_template::ReplyVars::new(),
                        );
                        return ApplicationRuntime::reply(self, &text);
                    }
                };
                let now_ms = command_now_ms();
                if !text.is_empty() {
                    let lines = crate::text::split_numbered_chat_message("", &text);
                    let refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
                    // 发送失败或仅部分发送时，不把完整回答写入下一轮会话。
                    self.reply_batch(&refs, 0)?;
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
                }
                self.log_executed_command(parsed, "ai_chat")?;
                if let Err(error) =
                    self.extract_ai_chat_facts(parsed, &actor, question, &text, &config)
                {
                    log::warn!("AI 回复已发送，长期记忆更新失败: {error:#}");
                }

                Ok(())
            }
        }
    }

    /// 当前提问者可以执行的命令名列表，注入提示词，避免模型给出越权命令。
    fn ai_chat_allowed_commands(&self, parsed: &RoutedCommand) -> Vec<String> {
        let catalog = crate::features::command_catalog::all_specs();
        let mut names = Vec::new();
        let snapshot = self.lifecycle.live_configs.snapshot();
        let actor = crate::features::command_access::actor_permission(
            self.lifecycle
                .live_configs
                .identity
                .role_of(&parsed.username),
            !parsed.username.trim().is_empty(),
        );
        for spec in catalog {
            let id = if spec.prefix == crate::features::command::CommandPrefix::At {
                crate::features::song_request::permission_id_for_command_name(spec.name)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("@{}", spec.name))
            } else {
                format!("{}{}", command_prefix_char(spec.prefix), spec.name)
            };
            let allowed =
                crate::features::command_access::catalog_policy(&snapshot.command_access, &id)
                    .is_some_and(|policy| policy.allows(actor));
            if allowed && !spec.hidden && spec.name != "忘记我" {
                names.push(id);
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
                &crate::config::ai::AiCallOptions {
                    model: config.model.clone(),
                    reasoning: config.reasoning,
                },
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
    fn run_ai_chat_command(
        &mut self,
        parsed: &RoutedCommand,
        command: &str,
    ) -> Result<serde_json::Value> {
        // 工具给的是一条**裸命令**（形如 `@点歌晴天`、`#提示`），不含发送者前缀。
        //
        // 不能走 `parse_command_envelope`：它第一步要把整行切成「发送者 + 正文」，
        // 而裸命令里没有 `：`/`:`/`]`/`】` 分隔符，会直接返回 None，
        // 于是工具命令 100% 被拒——表现为模型每次只拿到「命令无法解析」。
        // 身份不是从文本推出来的，而是提问者本人：直接用 `parsed` 里的发送者构造信封。
        let Some(request) = crate::features::command::CommandEnvelope::new(
            command.to_string(),
            parsed.username.clone(),
            parsed.message_type.clone(),
            command.trim().to_string(),
            crate::features::command::CommandObservation::default(),
        ) else {
            return Ok(serde_json::json!({"status":"rejected","reason":"命令无法解析"}));
        };
        let active = self.business.business.active_entertainment()?;
        let access_config = self.lifecycle.live_configs.snapshot();
        let router = crate::interfaces::chat::ChatCommandRouter::with_access(
            &self.business.custom_workflow,
            &self.lifecycle.live_configs.identity,
            &access_config.command_access,
        );
        let Some(routed) = router.route(&request, active) else {
            return Ok(
                serde_json::json!({"status":"rejected","reason":"当前会话无业务模块认领命令"}),
            );
        };
        if routed.permission_required.is_some() {
            return Ok(serde_json::json!({"status":"rejected","reason":"提问者权限不足"}));
        }
        // 防止模型进入 AI 自调用循环，或自行清除玩家记忆。
        if matches!(&routed.command, ModuleCommand::AiChat(_)) {
            return Ok(
                serde_json::json!({"status":"rejected","reason":"不能通过工具调用 AI 聊天或记忆命令"}),
            );
        }
        if self.command_is_disabled(&routed)? {
            return Ok(serde_json::json!({"status":"rejected","reason":"命令识别已禁用"}));
        }
        self.execute_command(&routed)?;
        Ok(
            serde_json::json!({"status":"handled","reason":"正常业务处理器已返回。业务可能已自行发送提示或启动异步流程；这不证明歌曲已入队、已播放等最终状态，不得据此声称最终成功。"}),
        )
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
