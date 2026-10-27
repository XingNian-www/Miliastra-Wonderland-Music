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
                // 给模型看的名字：先按 UID 反查（外部源的信封主语可能是 UID），
                // 查不到再用 canonical 名。`actor` 仍是记忆归属键，两者分开。
                let actor_display = match parsed.account.as_ref() {
                    Some(account) => self
                        .lifecycle
                        .live_configs
                        .identity
                        .friend_row_names_for_account(&account.game, &account.uid)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| actor.clone()),
                    None => actor.clone(),
                };
                // 大厅共享记忆：所有人最近说的 + 滚动摘要。
                // 个人记忆只属于提问者，大厅上下文是所有人在同一空间里共享的。
                let hall = self.business.ai_chat_memory.hall_context()?;
                let allowed = self.ai_chat_allowed_commands(parsed);
                let prompt = ai_chat::build_prompt(&ai_chat::PromptInput {
                    config: &config,
                    question,
                    context: &context,
                    allowed_commands: &allowed,
                    asker: actor_display.as_str(),
                    hall: &hall,
                });
                let tools = if config.allow_commands {
                    ai_chat::command_tools(&allowed)
                } else {
                    Vec::new()
                };
                let result = (|| -> Result<ai_chat::AiChatAnswer> {
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
                            let reply = ai_chat::parse_reply(&response.text);
                            let rendered = ai_chat::render_reply(&reply);
                            anyhow::ensure!(!rendered.is_empty(), "AI 未返回最终回复");
                            // 三个消费者要的不是同一份东西：
                            // - 发送给玩家：渲染文本（`*动作* 台词`）
                            // - conversation_history：原始 JSON（few-shot 一致，
                            //   否则模型在历史里看到散文，就不会再按格式输出）
                            // - 事实抽取：渲染文本（不能把 JSON 语法当玩家信息）
                            let raw = if reply.action.is_empty() && reply.text.is_empty() {
                                String::new()
                            } else {
                                serde_json::to_string(&serde_json::json!({
                                    "action": reply.action,
                                    "text": reply.text,
                                }))
                                .unwrap_or_else(|_| rendered.clone())
                            };
                            return Ok(ai_chat::AiChatAnswer { raw, rendered });
                        }
                        anyhow::ensure!(allow_tools, "AI 在工具关闭后仍请求执行");
                        for call in response.calls {
                            let outcome = if call.name != ai_chat::COMMAND_TOOL {
                                serde_json::json!({"status":"rejected","reason":"未知工具"})
                            } else if let Ok(execution) = ai_chat::parse_execution(&call.arguments)
                            {
                                // 去重键要规范化：`@点歌 晴天` 与 `@点歌晴天` 是同一命令
                                // （全项目都不依赖空格分隔），原样比字符串会让模型绕过去重。
                                let key = normalize_tool_command(&execution.command);
                                if executed.contains(&key) {
                                    serde_json::json!({"status":"rejected","reason":"同一问答内禁止重复执行相同命令"})
                                } else if executed.len() >= ai_chat::MAX_TOOL_ROUNDS {
                                    serde_json::json!({"status":"rejected","reason":"已达到命令执行次数上限"})
                                } else {
                                    // 只有真正执行成功才占用配额：原来 insert 在判定之前，
                                    // 三条被拒的命令就会耗光次数，模型连自我纠错的机会都没有。
                                    let outcome = self
                                        .run_ai_chat_command(parsed, &execution.command)
                                        .unwrap_or_else(|error| {
                                            log::warn!("AI 工具执行结果未确认: {error:#}");
                                            serde_json::json!({"status":"unknown","reason":"执行异常，不得重试或声称成功"})
                                        });
                                    if outcome.get("status").and_then(|v| v.as_str())
                                        == Some("handled")
                                    {
                                        executed.insert(key);
                                    }
                                    outcome
                                }
                            } else {
                                serde_json::json!({"status":"rejected","reason":"命令参数不符合工具 Schema"})
                            };
                            session.append_result(&call.id, &outcome.to_string())?;
                        }
                    }
                    anyhow::bail!("AI 工具循环未产生最终回复")
                })();
                let answer = match result {
                    Ok(answer) => answer,
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
                let rendered = &answer.rendered;
                let now_ms = command_now_ms();
                if !rendered.is_empty() {
                    let lines = crate::text::split_numbered_chat_message("", rendered);
                    let refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
                    // 发送失败或仅部分发送时，不把完整回答写入下一轮会话。
                    self.reply_batch(&refs, 0)?;
                    // 历史存原始 JSON：模型在 few-shot 里看到「上一轮就是这种格式」，
                    // 才会继续按格式输出；存渲染后的散文会让它以为自由文本也可以。
                    self.business.ai_chat_memory.record_turn(
                        &actor,
                        question,
                        &answer.raw,
                        config.actor_token_budget,
                        now_ms,
                    )?;
                    self.business.ai_chat_memory.record_bot_line(
                        rendered,
                        config.bot_token_budget,
                        now_ms,
                    )?;
                    // 大厅共享记忆：写入的是「谁说了什么」。
                    // 只有以关键词开头、真正触发 AI 的内容才进来，普通闲聊不入记忆。
                    self.business.ai_chat_memory.record_hall_line(
                        &actor_display,
                        question,
                        now_ms,
                    )?;
                    // BOT 的回答也进大厅记忆：
                    // 大厅是双向对话，只有问没有答，模型下一轮就不知道刚才承诺了什么。
                    self.business.ai_chat_memory.record_hall_line(
                        &self.bot_display_name(),
                        rendered,
                        now_ms,
                    )?;
                    // 超预算时压缩。失败不影响已发出的回复。
                    self.compress_ai_chat_memory(&actor, &config, now_ms);
                }
                self.log_executed_command(parsed, "ai_chat")?;
                if let Err(error) =
                    self.extract_ai_chat_facts(parsed, &actor, question, rendered, &config)
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
        // 与路由期同一判定：有账号按 UID，否则按昵称。
        let actor = crate::features::command_access::actor_permission(
            self.lifecycle
                .live_configs
                .identity
                .role_of_sender(&parsed.username, parsed.account.as_ref()),
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
        // 已有事实交给模型，它才能判断「这条推翻旧的」而不是「再补一条」。
        // 放在用户消息里而不是 extract_prompt 模板里——模板存在配置库，
        // 用户沿用的旧值不会有这个占位符，改代码默认值对已部署实例无效。
        let existing = self.business.ai_chat_memory.facts(actor)?;
        let existing_text = if existing.is_empty() {
            "（暂无）".to_string()
        } else {
            existing
                .iter()
                .enumerate()
                .map(|(index, fact)| format!("{}. {fact}", index + 1))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // 抽取规则收在 features::ai_chat::fact_extraction_prompt，与历史重放清洗共用，
        // 避免两处口径不一致——否则清洗出来的结果和日常增量抽取会对不上。
        let prompt =
            crate::features::ai_chat::fact_extraction_prompt(&existing_text, question, answer);
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
        let (facts, remove) = crate::features::ai_chat::parse_fact_update(&reply);
        // 先删被推翻的旧事实，再写新的：顺序反了会在中间态里同时存在新旧两条，
        // 而模型下一轮读取正是看这个列表。
        for fact in &remove {
            if let Err(error) = self.business.ai_chat_memory.delete_fact(actor, fact) {
                log::debug!("淘汰旧事实失败（跳过）: {error:#}");
            }
        }
        for fact in facts {
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

    /// 用新规则重放某个玩家的全部历史问答，重建事实表。
    ///
    /// 为什么需要重放而不是「等玩家下次提到再修正」：错误的旧事实会持续污染
    /// BOT 的回答，而 BOT 的回答又会被下一轮抽取当上下文——形成自我强化闭环，
    /// 只靠后续对话很难自行洗掉。历史原文都在库里，直接重跑最干净。
    ///
    /// 做成不依赖 `self` 的关联函数：HTTP 面板要用它，而面板拿不到 `&mut self`。
    /// 所需句柄都可克隆，由调用方捕获进闭包。
    pub(crate) fn rebuild_facts_with(
        memory: &crate::features::ai_chat::AiChatMemory,
        openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
        connection: &crate::config::ai::AiConfig,
        timeout: std::time::Duration,
        config: &crate::features::ai_chat::AiChatConfig,
        actor: &str,
    ) -> Result<usize> {
        let turns = memory.all_turns(actor)?;
        if turns.is_empty() {
            return Ok(0);
        }
        let mut facts: Vec<String> = Vec::new();
        for (question, answer) in &turns {
            if answer.trim().is_empty() {
                continue;
            }
            let existing_text = if facts.is_empty() {
                "（暂无）".to_string()
            } else {
                facts
                    .iter()
                    .enumerate()
                    .map(|(index, fact)| format!("{}. {fact}", index + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let prompt =
                crate::features::ai_chat::fact_extraction_prompt(&existing_text, question, answer);
            let reply = crate::config::ai::request_json(
                openai,
                connection,
                &crate::config::ai::AiCallOptions {
                    model: config.model.clone(),
                    reasoning: config.reasoning,
                },
                &config.extract_prompt,
                &prompt,
                256,
                timeout,
            );
            let Ok(reply) = reply else {
                log::debug!("重放抽取失败，跳过该轮");
                continue;
            };
            let (new_facts, remove) = crate::features::ai_chat::parse_fact_update(&reply);
            facts.retain(|fact| !remove.iter().any(|item| item.trim() == fact.trim()));
            for fact in new_facts {
                if !facts.iter().any(|item| item.trim() == fact.trim()) {
                    facts.push(fact);
                }
            }
        }
        memory.replace_facts(actor, &facts, command_now_ms())?;
        Ok(facts.len())
    }

    /// 大厅记忆里代表 BOT 自己的名字。
    ///
    /// 优先用配置的 `chat_source.self_uid` 反查（玩家给 BOT 的备注名最自然），
    /// 查不到才退回固定称呼。避免写成 UID —— 那对模型毫无意义。
    fn bot_display_name(&self) -> String {
        let self_uid = self.lifecycle.config.chat_source.self_uid;
        if self_uid != 0
            && let Some(name) = self
                .lifecycle
                .live_configs
                .identity
                .friend_row_names_for_account(
                    miliastra_kernel::chat_source::GAME_GENSHIN,
                    &self_uid.to_string(),
                )
                .into_iter()
                .next()
        {
            return name;
        }
        "BOT".to_string()
    }

    /// 超过预算时把最旧的记忆压成摘要。
    ///
    /// 分层（用户定的比例）：最远的 70% 压缩、20% 摘要、10% 最近原文完整保存。
    /// 远期事实单独存，不参与压缩。
    ///
    /// 压缩失败只记日志：回复已经发出，不该因为整理记忆而报错；
    /// 原文仍在库里，下一轮会再试。空摘要也不会覆盖旧摘要。
    fn compress_ai_chat_memory(
        &self,
        actor: &str,
        config: &crate::features::ai_chat::AiChatConfig,
        now_ms: i64,
    ) {
        let memory = &self.business.ai_chat_memory;
        // 个人记忆与大厅记忆各自独立压缩：两层预算不同，触发时机也不同。
        let mut plans = Vec::new();
        match memory.plan_actor_compression(actor, config.actor_token_budget) {
            Ok(Some(plan)) => plans.push(plan),
            Ok(None) => {}
            Err(error) => log::warn!("个人记忆压缩规划失败: {error:#}"),
        }
        match memory.plan_hall_compression(config.hall_token_budget) {
            Ok(Some(plan)) => plans.push(plan),
            Ok(None) => {}
            Err(error) => log::warn!("大厅记忆压缩规划失败: {error:#}"),
        }
        for plan in plans {
            let summary = match self.summarize_memory(&plan, config) {
                Ok(summary) => summary,
                Err(error) => {
                    log::warn!(
                        "记忆压缩失败，原文保留待下轮重试 scope={}: {error:#}",
                        plan.scope
                    );
                    continue;
                }
            };
            if let Err(error) = memory.apply_compression(&plan, &summary, now_ms) {
                log::warn!("记忆压缩结果写入失败 scope={}: {error:#}", plan.scope);
            }
        }
    }

    /// 调一次模型，把「旧摘要 + 新溢出的原文」压成新摘要。
    fn summarize_memory(
        &self,
        plan: &crate::features::ai_chat::CompressionPlan,
        config: &crate::features::ai_chat::AiChatConfig,
    ) -> Result<String> {
        let records = plan.records.join("\n");
        let prompt = crate::features::ai_chat::summary_prompt(&plan.previous_summary, &records);
        // 与事实抽取同样处理：不持 UI 租约干等模型。
        let _scan_permit = self.ui.coordinator.permit_scans_for_current_thread();
        let reply = crate::config::ai::request_json(
            &self.business.ai_chat_openai,
            &self.business.ai_connection,
            &crate::config::ai::AiCallOptions {
                model: config.model.clone(),
                reasoning: config.reasoning,
            },
            &prompt,
            &records,
            crate::features::ai_chat::SUMMARY_MAX_TOKENS,
            self.business.ai_request_timeout,
        )?;
        Ok(crate::features::ai_chat::parse_summary(&reply))
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
        // 回执是模型决定「敢不敢肯定回答」的唯一依据。说成「已执行成功」仍会
        // 被解读成「只是提交了」，于是模型补一句「能不能成功看平台反馈」。
        // 明确写出业务结果已确认。
        Ok(serde_json::json!({"status":"handled","reason":"命令已执行完成，业务结果已确认。"}))
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

/// 工具命令的去重键：去掉所有空白与全角空格，避免同一条命令换个空格就绕过。
///
/// 项目约定命令不依赖空格分隔（`@点歌晴天` 与 `@点歌 晴天` 等价），
/// 这里按同一语义归一化，只用于去重，不改变真正下发的文本。
fn normalize_tool_command(command: &str) -> String {
    command
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '\u{3000}')
        .collect()
}
