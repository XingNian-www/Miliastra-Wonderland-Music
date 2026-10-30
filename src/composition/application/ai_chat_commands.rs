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
                // 在途的后台抽取/压缩是在这次删除之前入队的，照常提交会把刚忘掉的
                // 记忆再写回来；推进代际让它们提交前自己作废。
                self.business
                    .ai_chat_memory_generation
                    .fetch_add(1, AtomicOrdering::SeqCst);
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
                    log::debug!("AI 聊天冷却中：玩家「{actor}」还需等待 {seconds} 秒");
                    return Ok(());
                }

                let context = self
                    .business
                    .ai_chat_memory
                    // 用**读取**预算而不是落库预算：后者是 32000，当读取窗口用
                    // 等于每题注入 32K token 的跨玩家、无归属回答。
                    .context(&actor, crate::features::ai_chat::BOT_HISTORY_READ_BUDGET)?;
                // 记忆键是 UUID，不可读；给模型看的名字必须换回显示昵称。
                //
                // 曾经这里另外按 UID 反查一次名字，与 `actor` 各走一条链，
                // 结果记忆键是「甲」，称呼却用「乙」——同一个人分裂成两份。
                // 现在两者都从 `actor` 这一个键派生，不可能再对不上。
                let actor_display = self.actor_display_name(&actor);
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
                    // 一轮取一次：工具循环内不重算，避免同一次提问前后时间不一致。
                    now_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_secs())
                        .unwrap_or(0),
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
                // 事实抽取与记忆压缩**完全异步**：各自在后台线程里完成
                // 「读库 → 调模型 → 写库」，正式任务发完回复立刻结束，不再等
                // 这两个模型调用——线上实测串行等待让 BOT 发言后的下一条命令
                // 多等约 9 秒。
                //
                // 旧实现把提交留在正式任务线程，靠串行保证 forget/rebuild 的顺序；
                // 现在改用「记忆代际」：入队时快照 generation，提交前发现期间执行过
                // @忘记我 / 面板重建就丢弃结果。
                if !rendered.is_empty() {
                    self.spawn_fact_extraction(
                        actor.clone(),
                        actor_display.clone(),
                        question.to_string(),
                        rendered.to_string(),
                        config.clone(),
                        now_ms,
                    );
                    self.spawn_memory_compression(&actor, &config, now_ms);
                }
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
                        crate::features::ai_chat::HallSpeakerRole::Player,
                        config.hall_token_budget,
                        now_ms,
                    )?;
                    // BOT 的回答也进大厅记忆：
                    // 大厅是双向对话，只有问没有答，模型下一轮就不知道刚才承诺了什么。
                    self.business.ai_chat_memory.record_hall_line(
                        &self.bot_display_name(),
                        rendered,
                        crate::features::ai_chat::HallSpeakerRole::Bot,
                        config.hall_token_budget,
                        now_ms,
                    )?;
                    // 记住「这个 actor 还用过哪个名字」。
                    // actor 是按昵称存的，玩家改名后事实会落到新行、旧行成孤儿；
                    // 不记录曾用名，模型只能去大厅散文里猜，一猜就把别人也带进来。
                    let _ = self.business.ai_chat_memory.record_actor_name(
                        &actor,
                        &actor_display,
                        now_ms,
                    );
                }
                self.log_executed_command(parsed, "ai_chat")?;
                Ok(())
            }
        }
    }

    /// 当前提问者可以执行的命令名列表，注入提示词，避免模型给出越权命令。
    fn ai_chat_allowed_commands(&self, parsed: &RoutedCommand) -> Vec<String> {
        let catalog = crate::features::command_catalog::all_specs();
        let mut names = Vec::new();
        let snapshot = self.lifecycle.live_configs.snapshot();
        // 与路由期同一判定：有账号按 UID，否则按昵称；
        // 身份命中**覆盖**，未命中退回来源下界。
        let actor = crate::features::command_access::actor_permission_with_floor(
            self.lifecycle
                .live_configs
                .identity
                .role_of_sender(&parsed.username, parsed.account.as_ref()),
            parsed.authority,
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
                    .is_some_and(|policy| policy.allows(Some(actor)));
            if allowed && !spec.hidden && spec.name != "忘记我" {
                names.push(id);
            }
        }
        names
    }

    /// 抢占一个记忆维护单飞键；已被占用返回 None。
    ///
    /// 用普通集合而不是队列：记忆维护是 best-effort，被放弃的那一轮
    /// 原文仍在库里，下一轮会重新抽取/压缩，不需要补做。
    fn claim_maintenance_slot(&self, key: &str) -> Option<MaintenanceSlot> {
        let inflight = self.business.ai_chat_maintenance_inflight.clone();
        {
            let mut set = match inflight.lock() {
                Ok(set) => set,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !set.insert(key.to_string()) {
                return None;
            }
        }
        Some(MaintenanceSlot {
            inflight,
            key: key.to_string(),
        })
    }

    /// 后台抽取事实：读已有事实 → 调模型 → 写回，全部在线程里完成。
    ///
    /// 正式任务**不等待**它，回复发出后立刻可以执行下一条命令。
    /// 抽取是 read-modify-write，读和写必须在同一线程里完成，否则并发会让
    /// remove 失效、退化成「新旧事实并存」；同一 actor 靠单飞键互斥。
    ///
    /// 与 @忘记我 / 面板重建的先后顺序用「记忆代际」保证：入队时快照
    /// generation，提交前发现期间代际前进过就丢弃本次结果。
    fn spawn_fact_extraction(
        &self,
        actor: String,
        actor_display: String,
        question: String,
        answer: String,
        config: crate::features::ai_chat::AiChatConfig,
        now_ms: i64,
    ) {
        let Some(slot) = self.claim_maintenance_slot(&format!("extract:{actor}")) else {
            log::debug!("玩家「{actor_display}」已有抽取在进行，跳过本轮事实抽取");
            return;
        };
        let memory = self.business.ai_chat_memory.clone();
        let generation = self.business.ai_chat_memory_generation.clone();
        let generation_at_spawn = generation.load(AtomicOrdering::SeqCst);
        let openai = self.business.ai_chat_openai.clone();
        let connection = self.business.ai_connection.clone();
        let spawned = std::thread::Builder::new()
            .name("ai-memory-extract".to_string())
            .spawn(move || {
                // 保留 guard 到线程结束：线程 panic 时 Drop 仍会释放单飞键。
                let _slot = slot;
                // panic 隔离：线程内的 panic 会毒化记忆库连接锁，
                // 一旦中毒后续所有记忆读写都会失败，直到进程重启。
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    extract_and_commit_facts(
                        &memory,
                        &generation,
                        generation_at_spawn,
                        &actor,
                        &actor_display,
                        &question,
                        &answer,
                        &config,
                        now_ms,
                        |system_prompt: &str, user_prompt: &str| {
                            crate::config::ai::request_json(
                                &openai,
                                &connection,
                                &crate::config::ai::AiCallOptions {
                                    model: config.model.clone(),
                                    // Unset 时不发 reasoning 字段，但服务端的 thinking 仍会思考，
                                    // 所以输出预算要按「思考 + JSON」一起给（见常量注释）。
                                    reasoning: crate::config::ai::ReasoningEffort::Unset,
                                },
                                system_prompt,
                                user_prompt,
                                crate::features::ai_chat::FACT_EXTRACT_MAX_TOKENS,
                                crate::features::ai_chat::FACT_EXTRACT_TIMEOUT,
                            )
                        },
                    )
                }));
                match outcome {
                    Ok(Ok(())) => log::debug!("玩家「{actor_display}」的事实抽取已完成"),
                    Ok(Err(error)) => {
                        log::warn!(
                            "AI 回复已发出，但长期记忆更新失败（步骤：抽取并写入记忆事实），下一轮会重新尝试: {error:#}"
                        )
                    }
                    Err(_) => {
                        log::error!(
                            "事实抽取线程崩溃（步骤：为玩家「{actor_display}」抽取记忆事实），本轮记忆未更新，下一轮会重新尝试"
                        )
                    }
                }
            });
        if let Err(error) = spawned {
            log::warn!(
                "长期记忆的事实抽取线程没能启动（步骤：AI 回复发出后启动后台抽取），本轮记忆不更新，下一轮会重新尝试: {error}"
            );
        }
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
        // 展示名单独传：actor 是记忆键（UUID），提示词要的是人能认的名字。
        // 不分开就会出现「重建出来的事实里带 UUID」，与日常增量抽取口径不一致。
        actor_display: &str,
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
            // 重放同样要传 actor，否则重建出来的事实仍然没有名字，
            // 用户点一次「重建记忆」就把问题打回原形。
            let prompt = crate::features::ai_chat::fact_extraction_prompt(
                actor_display,
                &existing_text,
                question,
                answer,
            );
            let reply = crate::config::ai::request_json(
                openai,
                connection,
                &crate::config::ai::AiCallOptions {
                    model: config.model.clone(),
                    reasoning: config.reasoning,
                },
                &config.extract_prompt,
                &prompt,
                crate::features::ai_chat::FACT_EXTRACT_MAX_TOKENS,
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

    /// 超过预算时把最旧的记忆压成摘要（后台线程，完全不阻塞正式任务）。
    ///
    /// 分层（用户定的比例）：最远的 70% 压缩、20% 摘要、10% 最近原文完整保存。
    /// 远期事实单独存，不参与压缩。
    ///
    /// 压缩失败只记日志：回复已经发出，不该因为整理记忆而报错；
    /// 原文仍在库里，下一轮会再试。空摘要也不会覆盖旧摘要。
    ///
    /// 规划留在正式任务里做（读库，毫秒级），生成摘要与写回都在后台线程。
    /// 与 @忘记我 / 面板重建的先后顺序同样靠「记忆代际」：提交前发现代际前进过就丢弃。
    fn spawn_memory_compression(
        &self,
        actor: &str,
        config: &crate::features::ai_chat::AiChatConfig,
        now_ms: i64,
    ) {
        let memory = &self.business.ai_chat_memory;
        // 代际必须在**规划之前**取：规划读库是同步的，但面板可能在线程启动前
        // 完成一次 forget/重建；若先规划后取代际，这份过期计划会被当成新任务接受，
        // 把刚被删掉的原文重新压成摘要写回去。
        let generation = self.business.ai_chat_memory_generation.clone();
        let generation_at_spawn = generation.load(AtomicOrdering::SeqCst);
        // 个人记忆与大厅记忆各自独立压缩：两层预算不同，触发时机也不同。
        let mut plans = Vec::new();
        match memory.plan_actor_compression(actor, config.actor_token_budget) {
            Ok(Some(plan)) => {
                if let Some(slot) = self.claim_maintenance_slot(&format!("compress:{}", plan.scope))
                {
                    plans.push((plan, slot));
                }
            }
            Ok(None) => {}
            Err(error) => log::warn!("个人记忆压缩规划失败: {error:#}"),
        }
        match memory.plan_hall_compression(config.hall_token_budget) {
            Ok(Some(plan)) => {
                if let Some(slot) = self.claim_maintenance_slot(&format!("compress:{}", plan.scope))
                {
                    plans.push((plan, slot));
                }
            }
            Ok(None) => {}
            Err(error) => log::warn!("大厅记忆压缩规划失败: {error:#}"),
        }
        if plans.is_empty() {
            return;
        }
        let memory = self.business.ai_chat_memory.clone();
        let openai = self.business.ai_chat_openai.clone();
        let connection = self.business.ai_connection.clone();
        let config = config.clone();
        let timeout = self.business.ai_request_timeout;
        let spawned = std::thread::Builder::new()
            .name("ai-memory-compress".to_string())
            .spawn(move || {
                let _slots = plans.iter().map(|(_, slot)| slot).collect::<Vec<_>>();
                // 与抽取同样的 panic 隔离：不能让它毒化记忆库连接锁。
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for (plan, _) in &plans {
                        match summarize_memory_with(&openai, &connection, &config, plan, timeout) {
                            Ok(summary) => {
                                if generation.load(AtomicOrdering::SeqCst) != generation_at_spawn {
                                    log::debug!(
                                        "记忆压缩结果已过期（期间执行过忘记或重建），本次摘要丢弃，原文仍在库里"
                                    );
                                    continue;
                                }
                                let summary_budget = if plan.scope
                                    == crate::features::ai_chat::AiChatMemory::hall_scope()
                                {
                                    config.hall_summary_budget
                                } else {
                                    config.actor_summary_budget
                                };
                                if let Err(error) =
                                    memory.apply_compression(plan, &summary, summary_budget, now_ms)
                                {
                                    log::warn!(
                                        "记忆压缩结果写入失败（步骤：把新摘要写回记忆库），本轮摘要丢弃、原文仍保留，下一轮会重新压缩: {error:#}"
                                    );
                                }
                            }
                            Err(error) => log::warn!(
                                "记忆压缩失败（步骤：调用模型生成摘要），原文保留在库里，下一轮会重新尝试: {error:#}"
                            ),
                        }
                    }
                }));
                if outcome.is_err() {
                    log::error!("记忆压缩线程 panic，本轮摘要丢弃");
                }
            });
        if let Err(error) = spawned {
            log::warn!(
                "记忆压缩线程没能启动（步骤：超预算后整理旧记忆），原文仍保留在库里，下一轮会重新规划: {error}"
            );
        }
    }

    /// 执行 AI 给出的命令：复用正常路由与执行链路，身份仍是提问者本人。
    fn run_ai_chat_command(
        &mut self,
        parsed: &RoutedCommand,
        command: &str,
    ) -> Result<serde_json::Value> {
        let Some(request) = ai_chat_command_envelope(parsed, command) else {
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
        // 捕获执行期间 BOT 实际发出的回复。
        //
        // 回执原来只有「已执行完成」一句话：点歌成功和「没找到《歌名》」
        // 拿到的回执一模一样，模型因此无法判断自己做对没有，只能含糊其辞
        // 或补一句「能不能入队看平台反馈」。业务自己生成的那句回复才是最准的
        // 结果描述，直接透传给模型。
        let capture = self.ui.chat_output.capture_replies();
        let executed = self.execute_command(&routed);
        let replies = capture.finish();
        executed?;
        // 回执是模型决定「敢不敢肯定回答」的唯一依据。说成「已执行成功」仍会
        // 被解读成「只是提交了」，于是模型补一句「能不能成功看平台反馈」。
        // 明确写出业务结果已确认。
        let mut receipt = serde_json::json!({
            "status":"handled",
            "reason":"命令已执行完成，业务结果已确认。"
        });
        if !replies.is_empty() {
            // 这是玩家实际看到的文案，模型据此判断成功与否、并保持口径一致。
            receipt["replies"] = serde_json::json!(replies);
        }
        Ok(receipt)
    }
}

/// 用提问者本人身份重建一条工具命令的信封。
///
/// 工具给的是一条**裸命令**（形如 `@点歌晴天`、`#提示`），不含发送者前缀。
/// 不能走 `parse_command_envelope`：它第一步要把整行切成「发送者 + 正文」，
/// 而裸命令里没有 `：`/`:`/`]`/`】` 分隔符，会直接返回 None，
/// 于是工具命令 100% 被拒——表现为模型每次只拿到「命令无法解析」。
/// 身份不是从文本推出来的，而是提问者本人：直接用 `parsed` 里的发送者构造信封。
///
/// **必须补 `.with_account(...)`**：`CommandEnvelope::new` 把 account 固定成
/// `None`（那条路径服务 OCR，OCR 拿不到 UID），而外部聊天源的提问者完全可能
/// 只登记了 UID、昵称是随机匹配键。丢掉 account 后路由期只能按昵称解析身份，
/// 于是出现「listener 按 UID 放行、AI 代执行按昵称被拒」，模型只能回一句
/// 「提问者权限不足」。这与 secondary_chat.rs 里 `with_account` 的用法同源。
fn ai_chat_command_envelope(
    parsed: &RoutedCommand,
    command: &str,
) -> Option<crate::features::command::CommandEnvelope> {
    crate::features::command::CommandEnvelope::new(
        command.to_string(),
        parsed.username.clone(),
        parsed.message_type.clone(),
        command.trim().to_string(),
        crate::features::command::CommandObservation::default(),
    )
    .map(|envelope| envelope.with_account(parsed.account.clone()))
}

/// 调一次模型，把「旧摘要 + 新溢出的原文」压成新摘要。
///
/// 做成自由函数而不是方法：压缩要搬到后台线程，那里没有 &self，
/// 只能拿可克隆的运行时句柄。
fn summarize_memory_with(
    openai: &miliastra_kernel::ai::OpenAiRuntimeHandle,
    connection: &crate::config::ai::AiConfig,
    config: &crate::features::ai_chat::AiChatConfig,
    plan: &crate::features::ai_chat::CompressionPlan,
    timeout: std::time::Duration,
) -> Result<String> {
    let records = plan.records.join("\n");
    let prompt = crate::features::ai_chat::summary_prompt(&plan.previous_summary, &records);
    // **必须走纯文本通道**：摘要要的是一段自然语言，
    // 而 request_json 会强制 response_format=JsonObject。真机上表现为
    // 每次压缩都 400 invalid_request_error，原文因此永远压不掉、
    // 每轮重试一次注定失败的调用。
    //
    // reasoning **保留**：摘要要判断哪些信息值得留下，需要模型思考。
    // 之前失败的真正原因是预算太小——线上实测 reasoning_tokens 恰好吃满
    // 800，输出为空、status 变成 incomplete(reason=max_output_tokens)。
    // 现在输出预算放到 128k（模型实测支持），思考有多少空间都不会挤掉正文。
    let reply = crate::config::ai::request_plain_text(
        openai,
        connection,
        &crate::config::ai::AiCallOptions {
            model: config.model.clone(),
            reasoning: config.reasoning,
        },
        &prompt,
        &records,
        crate::features::ai_chat::SUMMARY_MAX_TOKENS,
        timeout,
    )?;
    Ok(crate::features::ai_chat::parse_summary(&reply))
}

/// 记忆维护单飞键的占用凭证；Drop 时自动释放。
///
/// 用 Drop 而不是在线程末尾手写释放：线程 panic 时也要把键还回去，
/// 否则那一路记忆维护会永久停摆（键永远「在跑」）。
struct MaintenanceSlot {
    inflight: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    key: String,
}

impl Drop for MaintenanceSlot {
    fn drop(&mut self) {
        let mut set = match self.inflight.lock() {
            Ok(set) => set,
            Err(poisoned) => poisoned.into_inner(),
        };
        set.remove(&self.key);
    }
}

/// 后台抽取事实：读已有事实 → 调模型 → 写回，全部在一个线程里完成。
///
/// 旧实现拆成「线程调模型 + 正式任务提交」；现在整体搬进后台线程，
/// 正式任务不再等待。读-改-写必须在同一线程内，否则并发会让 remove 失效。
/// 与 @忘记我 / 面板重建的先后顺序由记忆代际保证：期间代际前进过就丢弃结果。
fn extract_and_commit_facts(
    memory: &crate::features::ai_chat::AiChatMemory,
    generation: &std::sync::atomic::AtomicU64,
    generation_at_spawn: u64,
    actor: &str,
    actor_display: &str,
    question: &str,
    answer: &str,
    config: &crate::features::ai_chat::AiChatConfig,
    now_ms: i64,
    call_model: impl FnOnce(&str, &str) -> Result<String>,
) -> Result<()> {
    if generation.load(AtomicOrdering::SeqCst) != generation_at_spawn {
        log::debug!("记忆维护任务已过期（期间执行过忘记/重建），跳过本轮事实抽取");
        return Ok(());
    }
    let existing = memory.facts(actor)?;
    let existing_text = if existing.is_empty() {
        "（暂无）".to_string()
    } else {
        existing
            .iter()
            .enumerate()
            .map(|(index, fact)| format!("{}. {fact}", index + 1))
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    };
    let prompt = crate::features::ai_chat::fact_extraction_prompt(
        actor_display,
        &existing_text,
        question,
        answer,
    );
    let reply = call_model(&config.extract_prompt, &prompt)?;
    // 模型调用期间被忘记/重建：写回会复活旧记忆，丢弃。
    if generation.load(AtomicOrdering::SeqCst) != generation_at_spawn {
        log::debug!("事实抽取期间记忆被清除/重建，丢弃本次结果");
        return Ok(());
    }
    let (facts, remove) = crate::features::ai_chat::parse_fact_update(&reply);
    // 先删被推翻的旧事实，再写新的：顺序反了会在中间态里同时存在新旧两条，
    // 而模型下一轮读取正是看这个列表。
    for fact in &remove {
        if let Err(error) = memory.delete_fact(actor, fact) {
            log::debug!("淘汰旧事实失败（跳过）: {error:#}");
        }
    }
    for fact in facts {
        memory.add_fact(actor, &fact, now_ms)?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::ai_chat::{AiChatConfig, AiChatMemory};
    use std::sync::atomic::AtomicU64;

    fn memory() -> AiChatMemory {
        AiChatMemory::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn fact_extraction_commits_new_facts_and_remove_list() {
        let memory = memory();
        memory.add_fact("actor", "玩家自称：小A", 1).unwrap();
        let generation = AtomicU64::new(0);
        extract_and_commit_facts(
            &memory,
            &generation,
            0,
            "actor",
            "小明",
            "以后叫我小B",
            "好的，小B",
            &AiChatConfig::default(),
            2,
            |_, _| Ok(r#"{"facts":["玩家自称：小B"],"remove":["玩家自称：小A"]}"#.to_string()),
        )
        .unwrap();
        assert_eq!(memory.facts("actor").unwrap(), ["玩家自称：小B"]);
    }

    /// 提问者本人在路由层的形态：外部聊天源（蓝色大厅）消息，可带 UID。
    fn asker(
        username: &str,
        account: Option<crate::features::command::CommandAccount>,
    ) -> RoutedCommand {
        let mut parsed = RoutedCommand::console(
            "队列清空",
            "队列清空",
            ModuleCommand::Playback(PlaybackCommand::QueueClear),
        );
        parsed.username = username.to_string();
        parsed.message_type = "blue".to_string();
        parsed.authority = CommandAuthority::HallMember;
        parsed.account = account;
        parsed
    }

    fn workflow_service() -> CustomWorkflowService {
        CustomWorkflowService::new(
            crate::features::custom_workflow::CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1_000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1_000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        )
    }

    /// 只登记了 UID、昵称留空的管理员（外部聊天源的昵称是随机匹配键）。
    fn uid_only_admin_identity() -> crate::features::identity::IdentityAccess {
        use crate::features::identity::{GameAccount, IdentityConfig, IdentityMapping};
        crate::features::identity::IdentityAccess::new(IdentityConfig {
            mappings: vec![IdentityMapping {
                nickname: String::new(),
                id: uuid::Uuid::from_u128(1),
                role: IdentityRole::Admin,
                note: String::new(),
                remark_sourced: false,
                accounts: vec![GameAccount::new("100000001")],
            }],
        })
    }

    fn gamer_account(uid: &str) -> crate::features::command::CommandAccount {
        crate::features::command::CommandAccount::new(
            miliastra_kernel::chat_source::GAME_GENSHIN,
            uid,
        )
    }

    #[test]
    fn ai_tool_envelope_keeps_the_askers_account() {
        // 工具信封必须原样带上提问者的账号：丢掉它，路由期就只能按昵称查身份。
        let account = gamer_account("100000001");
        let envelope =
            ai_chat_command_envelope(&asker("abcd1234", Some(account.clone())), "@队列清空")
                .expect("工具命令应能构造信封");
        assert_eq!(envelope.account(), Some(&account));
        assert_eq!(envelope.username(), "abcd1234");
        assert_eq!(envelope.message_type(), "blue");
    }

    #[test]
    fn ai_tool_command_passes_permission_check_for_uid_only_admin() {
        // 回归：只登记 UID（昵称未登记）的管理员让 AI 代执行 @队列清空。
        // 修前信封 account 恒 None → 路由只能按昵称查 → permission_required 非空
        // → 模型拿到「提问者权限不足」，而同一人自己发命令是被 listener 按 UID 放行的。
        use crate::features::command_access::CommandAccessConfig;
        let identity = uid_only_admin_identity();
        let workflows = workflow_service();
        let access = CommandAccessConfig::default();
        let router = ChatCommandRouter::with_access(&workflows, &identity, &access);

        let with_account = ai_chat_command_envelope(
            &asker("abcd1234", Some(gamer_account("100000001"))),
            "@队列清空",
        )
        .expect("工具命令应能构造信封");
        let routed = router.route(&with_account, None).expect("命令应被认领");
        assert_eq!(routed.role, Some(IdentityRole::Admin));
        assert_eq!(routed.permission_required, None, "UID 管理员应通过权限判定");

        // 对照：同一条命令丢掉 account 后只能按昵称解析，必须被拒（不放松门槛）。
        let without_account =
            ai_chat_command_envelope(&asker("abcd1234", None), "@队列清空").expect("信封");
        let routed = router.route(&without_account, None).expect("命令应被认领");
        assert_eq!(routed.role, None);
        assert_eq!(
            routed.permission_required,
            Some(IdentityRole::Admin),
            "没有账号线索时不能凭空获得管理权限"
        );
    }

    #[test]
    fn stale_generation_discards_the_extracted_facts() {
        let memory = memory();
        // 入队后执行了 @忘记我：代际前进，本次结果不能再写回。
        let generation = AtomicU64::new(7);
        extract_and_commit_facts(
            &memory,
            &generation,
            3,
            "actor",
            "小明",
            "我叫什么",
            "你叫小明",
            &AiChatConfig::default(),
            2,
            |_, _| Ok(r#"{"facts":["玩家自称：小明"],"remove":[]}"#.to_string()),
        )
        .unwrap();
        assert!(memory.facts("actor").unwrap().is_empty());
    }
}
