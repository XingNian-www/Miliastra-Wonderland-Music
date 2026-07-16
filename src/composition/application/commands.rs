use super::*;

impl ApplicationRuntime {
    pub(super) fn execute_command(&mut self, parsed: &ParsedCommand) -> Result<()> {
        match &parsed.command {
            BusinessIntent::SongRequest(song) => {
                let Some(mut request) = self.resolve_and_confirm_song(song)? else {
                    return Ok(());
                };
                request.console_bypass_dedup = parsed.message_type == "控制台";
                if !self.review_song_candidate(parsed, &request)? {
                    return Ok(());
                }
                if self.queue_contains_request(&request)? {
                    log::info!("队列已有: {}", request.keyword);
                    self.log_executed_command(
                        parsed,
                        &final_song_command_text(&request, "duplicate"),
                    )?;
                    self.reply(&format!("队列已有: {}", request.keyword))?;
                    return Ok(());
                }
                if !self.playback_queue()?.is_empty() {
                    let outcome = self.push_queue_request(&request)?;
                    self.handle_queue_push_outcome(parsed, &request, outcome, QUEUE_PUSH_FEEDBACK)?;
                    return Ok(());
                }

                let status = self.player.status();
                match status {
                    Ok(status) if is_playing(&status) => {
                        if !request.uri.trim().is_empty()
                            && status.current_uri.trim() == request.uri.trim()
                        {
                            self.log_executed_command(
                                parsed,
                                &final_song_command_text(&request, "already-playing"),
                            )?;
                            self.reply(&format!("当前正在播放: {}", request.keyword))?;
                            return Ok(());
                        }
                        if self
                            .player
                            .should_queue_until_current_song_finished(&status)?
                        {
                            let outcome = self.push_queue_request(&request)?;
                            self.handle_queue_push_outcome(
                                parsed,
                                &request,
                                outcome,
                                QUEUE_PUSH_FEEDBACK,
                            )?;
                            return Ok(());
                        }
                        if !self.player.current_status_matches_request(&status)? {
                            let outcome = self.play_request_confirmed(&request, true)?;
                            self.log_play_request_outcome(parsed, &request, outcome)?;
                            return Ok(());
                        }
                        let outcome = self.push_queue_request(&request)?;
                        self.handle_queue_push_outcome(
                            parsed,
                            &request,
                            outcome,
                            QUEUE_PUSH_FEEDBACK,
                        )?;
                        return Ok(());
                    }
                    Ok(status) => {
                        if self
                            .player
                            .should_queue_until_current_song_finished(&status)?
                        {
                            let outcome = self.push_queue_request(&request)?;
                            self.handle_queue_push_outcome(
                                parsed,
                                &request,
                                outcome,
                                QUEUE_PUSH_FEEDBACK,
                            )?;
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        log::error!("获取播放状态失败: {error:#}");
                        let outcome = self.push_queue_request(&request)?;
                        self.handle_queue_push_outcome(
                            parsed,
                            &request,
                            outcome,
                            UNKNOWN_STATUS_QUEUE_PUSH_FEEDBACK,
                        )?;
                        return Ok(());
                    }
                }

                let outcome = self.play_request_confirmed(&request, true)?;
                self.log_play_request_outcome(parsed, &request, outcome)?;
            }
            BusinessIntent::Playback(PlaybackCommand::Pause) => {
                let message = self.player.pause_by_user()?;
                self.log_executed_command(parsed, "pause")?;
                self.update_monitor_playback_controller();
                self.reply(if message.trim().is_empty() {
                    "已暂停"
                } else {
                    message.trim()
                })?;
            }
            BusinessIntent::Playback(PlaybackCommand::Resume | PlaybackCommand::Play) => {
                let message = self.player.resume_by_user()?;
                self.log_executed_command(parsed, "resume")?;
                self.update_monitor_playback_controller();
                self.reply(if message.trim().is_empty() {
                    "已恢复播放"
                } else {
                    message.trim()
                })?;
            }
            BusinessIntent::Playback(PlaybackCommand::Next) => {
                if !self.playback_queue()?.is_empty() {
                    self.consume_queue("手动下一首")?;
                    self.log_executed_command(parsed, "next queue")?;
                } else {
                    let message = self.player.next_external()?;
                    self.update_monitor_playback_controller();
                    self.log_executed_command(parsed, "next feeluown")?;
                    self.reply_player_status_after_skip(message.trim())?;
                }
            }
            BusinessIntent::Playback(PlaybackCommand::Previous) => {
                let message = self.player.previous_external()?;
                self.update_monitor_playback_controller();
                self.log_executed_command(parsed, "previous")?;
                self.reply_player_status_after_skip(message.trim())?;
            }
            BusinessIntent::Playback(PlaybackCommand::Volume(volume)) => {
                self.player.set_volume(volume)?;
                self.log_executed_command(parsed, &format!("volume {}", volume))?;
                self.reply(&format!("音量已设置为 {}", volume))?;
            }
            BusinessIntent::Playback(PlaybackCommand::Status) => {
                let status = self.player.status()?;
                self.log_executed_command(parsed, "status")?;
                self.reply(&format_status(&status))?;
            }
            BusinessIntent::Playback(PlaybackCommand::Lyrics) => {
                let status = self.player.status()?;
                self.log_executed_command(parsed, "lyrics")?;
                self.reply(&format_lyrics(&status))?;
            }
            BusinessIntent::Playback(PlaybackCommand::Queue) => {
                self.log_executed_command(parsed, "queue list")?;
                self.log_queue()?;
            }
            BusinessIntent::Playback(PlaybackCommand::QueueDelete(indexes)) => {
                if indexes.is_empty() {
                    self.log_executed_command(parsed, "queue delete invalid")?;
                    self.reply("没有匹配到有效队列序号")?;
                    return Ok(());
                }
                let removed = self
                    .business
                    .remove_playback_queue_indexes(indexes.clone())?;
                if removed.is_empty() {
                    self.log_executed_command(parsed, "queue delete none")?;
                    self.reply("队列删除失败或序号不存在")?;
                } else {
                    let removed_text = removed
                        .iter()
                        .map(|(index, item)| format!("{}.{}", index, item.keyword))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.log_executed_command(parsed, &format!("queue delete {}", removed_text))?;
                    self.reply(&format!("队列已删除: {}", removed_text))?;
                }
            }
            BusinessIntent::Playback(PlaybackCommand::QueueClear) => {
                let count = self.business.clear_playback_queue()?;
                self.log_executed_command(parsed, &format!("queue clear {}", count))?;
                if count == 0 {
                    self.reply("队列为空")?;
                } else {
                    self.reply(&format!("队列已清空: {} 首", count))?;
                }
            }
            BusinessIntent::Hall(HallCommand::Detect) => {
                self.log_executed_command(parsed, "hall detect")?;
                self.execute_hall_detect()?;
            }
            BusinessIntent::Hall(HallCommand::Time) => {
                self.log_executed_command(parsed, "hall time")?;
                self.reply_hall_time()?;
            }
            BusinessIntent::Administration(AdministrationCommand::Help) => {
                self.log_executed_command(parsed, "help")?;
                self.send_help()?;
            }
            BusinessIntent::Administration(AdministrationCommand::EntertainmentHelp) => {
                self.log_executed_command(parsed, "entertainment help")?;
                self.send_entertainment_help()?;
            }
            BusinessIntent::IdiomChain(command) => {
                if idiom_command_requires_executor(command) {
                    self.execute_idiom_explanation(&parsed.username, command)?;
                } else {
                    log::warn!("成语接龙命令错误进入主执行器，改由延迟聊天队列处理");
                    let _ = self.handle_idiom_chain_command(parsed)?;
                }
            }
            BusinessIntent::CardGame(command) => {
                self.execute_landlord_command(&parsed.username, command)?;
            }
            BusinessIntent::TurtleSoup(_) => {
                log::warn!("海龟汤命令错误进入主执行器，改由娱乐模块处理");
                let _ = self.handle_turtle_soup_command(parsed)?;
            }
            BusinessIntent::Undercover(command) => {
                self.execute_undercover_command(parsed, command)?;
            }
            BusinessIntent::Invite(invite) => {
                let request = InviteRequest::new(
                    invite.username.clone(),
                    invite.seq,
                    invite.password.clone(),
                );
                let execution = match self.business.begin_invite(request)? {
                    InviteStart::Duplicate { sequence } => {
                        log::info!("邀请参数 {} 已执行过，跳过", sequence);
                        return Ok(());
                    }
                    InviteStart::Ready(execution) => execution,
                };
                self.log_executed_command(parsed, &format!("invite {}", invite.username))?;
                execution.run(self)?;
            }
            BusinessIntent::Moderation(command) => {
                self.log_executed_command(
                    parsed,
                    &format!("{} uid {}", command.action.label(), command.uid),
                )?;
                self.execute_moderation_with_vote(command)?;
            }
            BusinessIntent::Hall(HallCommand::ToggleMicrophone { username }) => {
                log::info!("收到麦克风命令: {}", username);
                if self.check_public_hall()? {
                    self.log_executed_command(
                        parsed,
                        &format!("microphone skipped publicHall {}", username),
                    )?;
                    log::info!("麦克风: 当前在公共大厅，跳过状态切换和通告");
                } else {
                    self.log_executed_command(parsed, &format!("microphone toggle {}", username))?;
                    self.execute_microphone_command(username)?;
                }
            }
            BusinessIntent::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled,
                ..
            }) => {
                log::info!("收到{}命令", if *enabled { "启用" } else { "禁用" });
                self.business.set_commands_enabled(*enabled)?;
                self.log_executed_command(
                    parsed,
                    if *enabled {
                        "enable commands"
                    } else {
                        "disable commands"
                    },
                )?;
                self.reply(if *enabled {
                    "管理员已启用大厅命令识别功能"
                } else {
                    "管理员已禁用大厅命令识别功能"
                })?;
            }
            BusinessIntent::Administration(AdministrationCommand::IdleExit { minutes }) => {
                self.configure_idle_exit(*minutes)?;
                self.log_executed_command(parsed, &format!("idle exit {}", minutes))?;
            }
            BusinessIntent::Administration(AdministrationCommand::ChatListenerMode(command)) => {
                self.log_executed_command(parsed, &format!("chat listener {}", command.label()))?;
                log::warn!(
                    "监听模式命令未经过专用队列分发，已只记录: {}",
                    command.label()
                );
            }
            BusinessIntent::CustomWorkflow(command) => {
                self.log_executed_command(
                    parsed,
                    &format!("custom workflow {}", command.workflow),
                )?;
                self.execute_custom_workflow(command, parsed)?;
            }
        };
        Ok(())
    }

    pub(super) fn configure_idle_exit(&self, minutes: u32) -> Result<()> {
        let minutes = minutes.max(IDLE_EXIT_MIN_MINUTES);
        self.business
            .configure_idle_exit(Duration::from_secs(minutes as u64 * 60), Instant::now())?;
        log::info!(
            "已设置闲置退出: {}分钟无新命令后关闭目标游戏进程，软件主进程继续运行",
            minutes
        );
        Ok(())
    }

    pub(super) fn commands_enabled(&self) -> Result<bool> {
        Ok(self
            .business
            .operational_snapshot(Instant::now())?
            .commands_enabled())
    }

    fn execute_microphone_command(&self, username: &str) -> Result<()> {
        let outcome = self
            .hall_ui
            .submit_microphone(ToggleMicrophone)
            .context("提交麦克风切换 UI 事务")?
            .wait()
            .context("等待麦克风切换 UI 事务")?;
        match outcome.effect() {
            ToggleMicrophoneEffect::Toggled => log::info!("麦克风: 已按 N 切换状态"),
            ToggleMicrophoneEffect::Failed(failure) => {
                return Err(anyhow!("麦克风切换失败：{failure}"));
            }
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            log::error!("麦克风已切换，但一级驻留恢复失败: {failure}");
        }
        self.reply(&format!("@{} 执行了切换麦克风状态！", username))
    }

    fn execute_hall_detect(&mut self) -> Result<()> {
        let result = self.read_hall_info_ui();

        match result {
            Ok(info) => {
                let name = info.name;
                log::info!("大厅检测 OCR 结果: {}", name);
                if normalize_comparison_text(&name) == normalize_comparison_text("公共大厅") {
                    self.clear_hall_remaining_minutes()?;
                    self.reply("当前为公共大厅")?;
                } else {
                    if let Some(minutes) = info.remaining_minutes {
                        self.update_hall_remaining_minutes(minutes)?;
                        log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
                    }
                    let time_suffix = format_hall_remaining_suffix(info.remaining_minutes);
                    self.reply(&format!(
                        "当前为{}{}",
                        if name.is_empty() {
                            "未识别到大厅名称"
                        } else {
                            name.as_str()
                        },
                        time_suffix
                    ))?;
                }
            }
            Err(error) => {
                log::error!("大厅检测 OCR 失败: {error:#}");
                self.reply("大厅检测失败")?;
            }
        }
        Ok(())
    }

    fn reply_player_status_after_skip(&self, fallback: &str) -> Result<()> {
        sleep(Duration::from_millis(
            self.config.timing.playback.skip_status_initial_ms,
        ));
        for _ in 0..self.config.timing.playback.skip_status_retries {
            match self.player.status() {
                Ok(status) if is_playing(&status) || status.status == "paused" => {
                    return self.reply(&format_play_message(&status));
                }
                Ok(_) => sleep(Duration::from_millis(
                    self.config.timing.playback.skip_status_poll_ms,
                )),
                Err(error) => {
                    log::error!("切歌后查询播放状态失败: {error:#}");
                    break;
                }
            }
        }
        if fallback.is_empty() {
            self.reply("切歌完成")
        } else {
            self.reply(fallback)
        }
    }

    fn reply_hall_time(&mut self) -> Result<()> {
        let minutes = self
            .business
            .runtime_state_snapshot()?
            .hall_remaining_minutes_now();
        if let Some(minutes) = minutes.filter(|minutes| *minutes > 0) {
            return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }

        log::info!("大厅时间未知，执行一次大厅识别");
        let result = self.read_hall_info_ui();

        let info = match result {
            Ok(info) => info,
            Err(error) => {
                log::error!("大厅时间 OCR 失败: {error:#}");
                return self.reply("大厅时间未知");
            }
        };
        let is_public_hall =
            normalize_comparison_text(&info.name) == normalize_comparison_text("公共大厅");
        if is_public_hall {
            self.clear_hall_remaining_minutes()?;
            return self.reply("公共大厅无时间限制");
        }
        if let Some(minutes) = info.remaining_minutes {
            self.update_hall_remaining_minutes(minutes)?;
            return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }
        self.reply("大厅时间未知")
    }

    pub(super) fn check_public_hall(&self) -> Result<bool> {
        let outcome = self
            .hall_ui
            .submit_detect(DetectPublicHall)
            .context("提交公共大厅检测 UI 事务")?
            .wait()
            .context("等待公共大厅检测 UI 事务")?;
        let (is_public_hall, info) = match outcome.effect() {
            DetectPublicHallEffect::Detected { is_public, info } => (*is_public, info.clone()),
            DetectPublicHallEffect::Failed(failure) => {
                log::error!("大厅检测 OCR 失败，按非公共大厅处理: {failure}");
                return Ok(false);
            }
        };
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            log::error!("大厅检测已完成，但一级驻留恢复失败: {failure}");
        }
        log::info!("大厅检测 OCR 结果: {}", info.name);
        if is_public_hall {
            self.clear_hall_remaining_minutes()?;
        } else if let Some(minutes) = info.remaining_minutes {
            self.update_hall_remaining_minutes(minutes)?;
            log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
        }
        Ok(is_public_hall)
    }

    fn read_hall_info_ui(&self) -> Result<HallInfo> {
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
            log::error!("大厅信息已读取，但一级驻留恢复失败: {failure}");
        }
        Ok(info)
    }

    fn update_hall_remaining_minutes(&self, minutes: u32) -> Result<()> {
        self.business
            .update_hall_remaining_minutes(minutes)
            .map_err(anyhow::Error::from)
    }

    fn clear_hall_remaining_minutes(&self) -> Result<()> {
        self.business
            .clear_hall_remaining_minutes()
            .map_err(anyhow::Error::from)
    }

    fn send_help(&self) -> Result<()> {
        self.reply_batch(
            &[
                "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏",
                "切换网易平台: @网易点歌 歌名 歌手 伴奏,默认为QQ平台",
                "可用 @QQ点歌/@网易点歌 指定来源,@AI点歌用于智能识别歌名歌手",
            ],
            self.config.timing.command.help_batch_ms,
        )
    }

    fn send_entertainment_help(&self) -> Result<()> {
        self.reply_batch(
            &[
                "成语接龙: #接龙 成语;同音模式用 #同音接龙 成语;进行中用 #成语/#提示/#解释",
                "斗地主: #斗地主,#加入,#抢/#不抢,#牌组/#出牌组,#过;好友私聊 #手牌",
                "跑得快: #跑得快,#加入,#牌组/#出牌组,#过;好友私聊 #手牌",
                "海龟汤: #海龟汤;进行中 #状态/#结束;其他 #内容 作为问题",
                "海龟汤长答案: ##1第一段,##2第二段,最后发送##提交",
                "谁是卧底: #卧底/#卧底双;好友私聊 #加入;公屏 #开局/#状态/#退出",
                "谁是卧底: 描述用公屏 #内容;投票用好友私聊 #A 或 #投A",
            ],
            self.config.timing.command.help_batch_ms,
        )
    }

    fn execute_idiom_explanation(
        &self,
        player: &str,
        command: &idiom_chain::IdiomChainCommand,
    ) -> Result<()> {
        let outcome = self.business.explain_idiom_chain(player, command)?;
        let mut messages = vec![outcome.reply];
        if let Some(explanation) = outcome.explanation {
            messages.extend(split_numbered_chat_message("来源", &explanation.source));
            messages.extend(split_numbered_chat_message(
                "解释",
                &explanation.explanation,
            ));
        }
        let message_refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&message_refs, self.config.timing.command.help_batch_ms)
    }

    fn execute_landlord_command(&self, player: &str, command: &LandlordCommand) -> Result<()> {
        let start = self
            .business
            .begin_card_game(player, command, Instant::now())?;
        drive_card_game_start(&self.business, start, CardGameEffectLane::Formal, self)
    }

    fn execute_undercover_command(
        &self,
        parsed: &ParsedCommand,
        command: &UndercoverCommand,
    ) -> Result<()> {
        let source = match parsed.message_type.as_str() {
            "pink" => UndercoverCommandSource::Friend,
            "控制台" => UndercoverCommandSource::Console,
            _ => UndercoverCommandSource::Hall,
        };
        let start =
            self.business
                .begin_undercover(&parsed.username, source, command, Instant::now())?;
        drive_undercover_start(&self.business, start, self)
    }
}
