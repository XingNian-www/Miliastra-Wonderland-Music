use super::*;

impl ApplicationRuntime {
    pub(super) fn execute_web_tool_request(&mut self, request: WebToolRequest) -> Result<String> {
        match request {
            WebToolRequest::Ocr { rect } => {
                let frame = self.latest_frame()?;
                let image = match rect {
                    Some(rect) => crop_canvas(&frame, rect)?,
                    None => (*frame).clone(),
                };
                serde_json::to_string_pretty(
                    &self.ocr.recognize_lines(image, OcrPriority::Diagnostic)?,
                )
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::ScanChat => {
                let frame = self.latest_frame()?;
                let templates = TemplateArgs::default().resolve(&self.config);
                let prepared =
                    prepare_chat_scan(&frame, &templates, self.config.screen.chat_rect.into())?;
                serde_json::to_string_pretty(&recognize_prepared_chat(
                    &self.ocr,
                    OcrPriority::Diagnostic,
                    &templates,
                    prepared,
                    None,
                )?)
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::UiState => {
                let frame = self.latest_frame()?;
                let templates = UiTemplateArgs::default().resolve(&self.config);
                Ok(detect_ui_state(&frame, &templates, &self.config.screen)?.to_string())
            }
            WebToolRequest::HallName => {
                let frame = self.latest_frame()?;
                let image = crop_canvas(&frame, self.config.screen.hall_name_rect.into())?;
                self.ocr.merged_text(
                    image,
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::Diagnostic,
                )
            }
            WebToolRequest::MatchTemplate {
                template,
                rect,
                threshold,
                click,
            } => {
                let frame = self.latest_frame()?;
                let default_threshold = match &template {
                    WebToolTemplate::WonderlandEnterButton => {
                        self.config.startup.wonderland_enter_button_threshold
                    }
                    WebToolTemplate::PaimonMenu | WebToolTemplate::WonderlandClose => {
                        self.config.startup.template_threshold
                    }
                    WebToolTemplate::Custom(_) => self.config.custom_workflows.default_threshold,
                    _ => self.config.templates.marker_threshold,
                };
                let path = match &template {
                    WebToolTemplate::BlueMarker => self.config.templates.blue_marker.clone(),
                    WebToolTemplate::YellowMarker => self.config.templates.yellow_marker.clone(),
                    WebToolTemplate::PinkMarker => self.config.templates.pink_marker.clone(),
                    WebToolTemplate::Friend => self.config.templates.friend.clone(),
                    WebToolTemplate::SecondaryBack => self.config.templates.secondary_back.clone(),
                    WebToolTemplate::SecondaryHall => self.config.templates.secondary_hall.clone(),
                    WebToolTemplate::InviteViewStar => {
                        self.config.templates.invite_view_star.clone()
                    }
                    WebToolTemplate::InviteGotoHall => {
                        self.config.templates.invite_goto_hall.clone()
                    }
                    WebToolTemplate::InviteEnterHall => {
                        self.config.templates.invite_enter_hall.clone()
                    }
                    WebToolTemplate::FriendPanel => self.config.templates.friend_panel.clone(),
                    WebToolTemplate::FriendSearchPanel => {
                        self.config.templates.friend_search_panel.clone()
                    }
                    WebToolTemplate::FriendMoreSettings => {
                        self.config.templates.friend_more_settings.clone()
                    }
                    WebToolTemplate::FriendBlockChat => {
                        self.config.templates.friend_block_chat.clone()
                    }
                    WebToolTemplate::FriendBlacklist => {
                        self.config.templates.friend_blacklist.clone()
                    }
                    WebToolTemplate::FriendConfirm => self.config.templates.friend_confirm.clone(),
                    WebToolTemplate::WonderlandEnterButton => self
                        .config
                        .startup
                        .templates
                        .wonderland_enter_button
                        .clone(),
                    WebToolTemplate::PaimonMenu => {
                        self.config.startup.templates.paimon_menu.clone()
                    }
                    WebToolTemplate::WonderlandClose => {
                        self.config.startup.templates.wonderland_close.clone()
                    }
                    WebToolTemplate::Custom(name) => self
                        .config
                        .custom_workflows
                        .templates
                        .get(name)
                        .cloned()
                        .ok_or_else(|| anyhow!("自定义模板不存在: {name}"))?,
                };
                let threshold = threshold.unwrap_or(default_threshold);
                if click {
                    self.ensure_web_tool_input_still_idle()?;
                    let hit = best_template_hit(&frame, rect, &path, threshold)?
                        .ok_or_else(|| anyhow!("未找到超过阈值的模板: {}", template.label()))?;
                    let point = hit.center();
                    self.game_ui
                        .ensure_ready(self.config.timing.input.after_activate_ms)?;
                    self.game_ui
                        .click_point(PointConfig::new(point.x, point.y))?;
                    Ok(format!(
                        "已点击 {}: x={} y={} score={:.3}",
                        template.label(),
                        point.x,
                        point.y,
                        hit.score
                    ))
                } else {
                    serde_json::to_string_pretty(&find_template_hits(
                        &frame, rect, &path, threshold,
                    )?)
                    .map_err(|error| anyhow!(error))
                }
            }
            WebToolRequest::Click { x, y } => {
                let width = self.config.screen.expected_width as i32;
                let height = self.config.screen.expected_height as i32;
                if !(0..width).contains(&x) || !(0..height).contains(&y) {
                    return Err(anyhow!(
                        "坐标超出画布范围: x=0..{} y=0..{}",
                        width - 1,
                        height - 1
                    ));
                }
                self.ensure_web_tool_input_still_idle()?;
                self.game_ui
                    .ensure_ready(self.config.timing.input.after_activate_ms)?;
                self.game_ui.click_point(PointConfig::new(x, y))?;
                Ok(format!("已点击坐标: {x},{y}"))
            }
            WebToolRequest::Key { key } => {
                let key = parse_key(&key)?;
                self.ensure_web_tool_input_still_idle()?;
                self.game_ui
                    .ensure_ready(self.config.timing.input.after_activate_ms)?;
                self.game_ui.press_key(key)?;
                Ok("按键已发送".to_string())
            }
            WebToolRequest::ChatChangeSamples {
                samples,
                interval_ms,
            } => self.sample_web_tool_chat_changes(samples, interval_ms),
            WebToolRequest::PanelResponseBenchmark { rounds } => {
                self.run_web_tool_panel_benchmark(rounds)
            }
            WebToolRequest::OcrBackendProbe => {
                let args = OcrArgs::default().resolve(&self.config);
                let result = probe_ocr_backend_support(&args)
                    .into_iter()
                    .map(|probe| match probe.status {
                        OcrBackendProbeStatus::Available {
                            init_ms,
                            detect_ms,
                            rec_ms,
                        } => format!(
                            "{} [{}] 可用: 初始化={}ms 检测={}ms 识别={}ms",
                            probe.name,
                            if probe.gpu { "GPU" } else { "CPU" },
                            init_ms,
                            detect_ms,
                            rec_ms
                        ),
                        OcrBackendProbeStatus::Failed { elapsed_ms, error } => format!(
                            "{} [{}] 不可用: {}ms {error}",
                            probe.name,
                            if probe.gpu { "GPU" } else { "CPU" },
                            elapsed_ms
                        ),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(result)
            }
            WebToolRequest::AiSearchPreview {
                keyword,
                prefer_accompaniment,
            } => {
                if !self.ai.enabled() {
                    return Err(anyhow!("AI 点歌未启用，请先配置 ai.api_key"));
                }
                let candidates = self.player_search.search_candidates(&keyword, "")?;
                let pick = if candidates.is_empty() {
                    None
                } else {
                    Some(self.ai.pick_song_candidate(
                        &keyword,
                        prefer_accompaniment,
                        &candidates,
                    )?)
                };
                let mut lines = vec![
                    format!("用户请求: {}", keyword),
                    format!("候选数量: {}", candidates.len()),
                ];
                lines.extend(candidates.iter().enumerate().map(|(index, candidate)| {
                    format!("{}. {} -> {}", index + 1, candidate.text, candidate.uri)
                }));
                if let Some(pick) = pick {
                    lines.push(format!(
                        "AI 选择: {} score={:.2} reason={}",
                        pick.uri, pick.score, pick.reason
                    ));
                }
                Ok(lines.join("\n"))
            }
        }
    }

    fn sample_web_tool_chat_changes(&self, samples: u32, interval_ms: u64) -> Result<String> {
        let baseline = self.latest_frame()?;
        let mut previous =
            rect_chat_change_fingerprint(&baseline, self.config.screen.chat_rect.into())?;
        let templates = TemplateArgs::default().resolve(&self.config);
        let mut lines = vec![format!(
            "采样次数={} 间隔={}ms，区域为一级聊天区",
            samples, interval_ms
        )];

        for index in 1..=samples {
            sleep(Duration::from_millis(interval_ms));
            let frame = self.latest_frame()?;
            let current =
                rect_chat_change_fingerprint(&frame, self.config.screen.chat_rect.into())?;
            let stats = change_stats(&previous, &current);
            let changed = stats.mean_abs_diff >= self.config.ocr.change_mean_threshold
                || stats.changed_ratio >= self.config.ocr.change_pixel_threshold;
            let markers = if changed {
                let (blue, yellow, pink) =
                    count_chat_markers(&frame, &templates, self.config.screen.chat_rect)?;
                format!(" 蓝={} 黄={} 粉={}", blue, yellow, pink)
            } else {
                String::new()
            };
            lines.push(format!(
                "#{} mean={:.3} ratio={:.5} changed={}{}",
                index, stats.mean_abs_diff, stats.changed_ratio, changed, markers
            ));
            previous = current;
        }
        Ok(lines.join("\n"))
    }

    fn run_web_tool_panel_benchmark(&self, rounds: u32) -> Result<String> {
        const TIMEOUT_MS: u64 = 1_500;
        const POLL_MS: u64 = 50;
        const STABLE_SAMPLES: usize = 3;

        self.ensure_web_tool_input_still_idle()?;
        self.game_ui
            .ensure_ready(self.config.timing.input.after_activate_ms)?;
        let mut open_times = Vec::new();
        let mut close_times = Vec::new();
        let mut failures = 0u32;
        let detect_rect = web_tool_panel_response_rect(&self.config);

        for _ in 0..rounds {
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Escape)?;
            let closed = self.latest_frame()?;
            let closed = rect_chat_change_fingerprint(&closed, detect_rect)?;

            let opened_at = Instant::now();
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Return)?;
            let Some(opened) = self.wait_for_web_tool_change(
                &closed,
                detect_rect,
                opened_at,
                TIMEOUT_MS,
                POLL_MS,
                STABLE_SAMPLES,
            )?
            else {
                failures += 1;
                continue;
            };
            open_times.push(opened.0);

            let closed_at = Instant::now();
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Escape)?;
            let Some(closed_again) = self.wait_for_web_tool_change(
                &opened.1,
                detect_rect,
                closed_at,
                TIMEOUT_MS,
                POLL_MS,
                STABLE_SAMPLES,
            )?
            else {
                failures += 1;
                continue;
            };
            close_times.push(closed_again.0);
        }

        let _ = self.game_ui.press_key(Key::Escape);
        Ok(format!(
            "轮数={} 失败={}\n打开: {}\n关闭: {}",
            rounds,
            failures,
            format_web_tool_latency_summary(&open_times),
            format_web_tool_latency_summary(&close_times)
        ))
    }

    fn ensure_web_tool_input_still_idle(&self) -> Result<()> {
        if self
            .business
            .scheduler_snapshot()?
            .pending_labels()
            .is_empty()
        {
            Ok(())
        } else {
            Err(anyhow!("正式任务已进入队列，已取消 Web 工具输入"))
        }
    }

    fn wait_for_web_tool_change(
        &self,
        baseline: &ChangeFingerprint,
        detect_rect: Rect,
        started: Instant,
        timeout_ms: u64,
        poll_ms: u64,
        stable_samples: usize,
    ) -> Result<Option<(u128, ChangeFingerprint)>> {
        let mut previous = baseline.clone();
        let mut stable_count = 0usize;
        let deadline = started + Duration::from_millis(timeout_ms);

        while Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = self.latest_frame()?;
            let current = rect_chat_change_fingerprint(&frame, detect_rect)?;
            let from_baseline = change_stats(baseline, &current);
            let from_previous = change_stats(&previous, &current);
            let changed = from_baseline.mean_abs_diff >= self.config.ocr.change_mean_threshold
                || from_baseline.changed_ratio >= self.config.ocr.change_pixel_threshold;
            let stable = from_previous.mean_abs_diff < self.config.ocr.change_mean_threshold
                && from_previous.changed_ratio < self.config.ocr.change_pixel_threshold;

            if changed && stable {
                stable_count += 1;
                if stable_count >= stable_samples {
                    return Ok(Some((started.elapsed().as_millis(), current)));
                }
            } else if !stable {
                stable_count = 0;
            }
            previous = current;
        }
        Ok(None)
    }
}
