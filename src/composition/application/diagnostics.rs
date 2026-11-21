use super::*;

impl AppServices {
    /// 执行一次网页面板发起的工具调用。
    ///
    /// 只有 [`&AppServices`](AppServices)：所需句柄要么在共享服务里，要么由
    /// `UiServices::ui_handle` 提供，没有任何需要「反向重建运行时」的部分。
    pub(super) fn execute_web_tool_request(&self, request: WebToolRequest) -> Result<String> {
        let live_config = self.shared.live_configs.snapshot();
        match request {
            WebToolRequest::Ocr { rect } => {
                let frame = self.latest_frame()?;
                let image = match rect {
                    Some(rect) => crop_canvas(&frame, rect)?,
                    None => (*frame).clone(),
                };
                serde_json::to_string_pretty(
                    &self
                        .ui
                        .ocr
                        .recognize_lines(image, OcrPriority::Diagnostic)?,
                )
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::ScanChat => {
                let frame = self.latest_frame()?;
                let prepared = prepare_chat_scan(
                    &frame,
                    &self.ui.chat_templates,
                    self.shared.config.screen.chat_rect.into(),
                )?;
                serde_json::to_string_pretty(&recognize_prepared_chat(
                    &self.ui.ocr,
                    OcrPriority::Diagnostic,
                    &self.ui.chat_templates,
                    prepared,
                    None,
                )?)
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::UiState => {
                capture_ui_state_diagnostic(&self.ui.game_ui, &self.ui.ui_handle)
            }
            WebToolRequest::HallName => {
                let frame = self.latest_frame()?;
                let image = crop_canvas(&frame, self.shared.config.screen.hall_name_rect.into())?;
                Ok(self.ui.ocr.merged_text(
                    image,
                    self.shared.config.ocr.same_line_y_tolerance,
                    OcrPriority::Diagnostic,
                )?)
            }
            WebToolRequest::MatchTemplate {
                template,
                rect,
                threshold,
                click,
            } => {
                let frame = self.latest_frame()?;
                let default_threshold = match &template {
                    WebToolTemplate::WorldWish => self.shared.config.templates.marker_threshold,
                    WebToolTemplate::PaimonMenu => self.shared.config.startup.template_threshold,
                    WebToolTemplate::WonderlandMapStar => {
                        self.shared.config.startup.template_threshold
                    }
                    WebToolTemplate::Custom(_) => live_config.custom_workflows.default_threshold,
                    _ => self.shared.config.templates.marker_threshold,
                };
                let path = match &template {
                    WebToolTemplate::BlueMarker => self.shared.config.templates.blue_marker.clone(),
                    WebToolTemplate::YellowMarker => {
                        self.shared.config.templates.yellow_marker.clone()
                    }
                    WebToolTemplate::PinkMarker => self.shared.config.templates.pink_marker.clone(),
                    WebToolTemplate::SecondaryBack => {
                        self.shared.config.templates.secondary_back.clone()
                    }
                    WebToolTemplate::SecondaryHall => {
                        self.shared.config.templates.secondary_hall.clone()
                    }
                    WebToolTemplate::WorldWish => self.shared.config.templates.world_wish.clone(),
                    WebToolTemplate::InviteViewStar => {
                        self.shared.config.templates.invite_view_star.clone()
                    }
                    WebToolTemplate::InviteGotoHall => {
                        self.shared.config.templates.invite_goto_hall.clone()
                    }
                    WebToolTemplate::InviteEnterHall => {
                        self.shared.config.templates.invite_enter_hall.clone()
                    }
                    WebToolTemplate::FriendPanel => {
                        self.shared.config.templates.friend_panel.clone()
                    }
                    WebToolTemplate::FriendSearchPanel => {
                        self.shared.config.templates.friend_search_panel.clone()
                    }
                    WebToolTemplate::FriendMoreSettings => {
                        self.shared.config.templates.friend_more_settings.clone()
                    }
                    WebToolTemplate::FriendBlockChat => {
                        self.shared.config.templates.friend_block_chat.clone()
                    }
                    WebToolTemplate::FriendBlacklist => {
                        self.shared.config.templates.friend_blacklist.clone()
                    }
                    WebToolTemplate::ConfirmButton => {
                        self.shared.config.templates.confirm_button.clone()
                    }
                    WebToolTemplate::PaimonMenu => {
                        self.shared.config.startup.templates.paimon_menu.clone()
                    }
                    WebToolTemplate::WonderlandMapStar => self
                        .shared
                        .config
                        .startup
                        .templates
                        .wonderland_map_star
                        .clone(),
                    WebToolTemplate::Custom(name) => live_config
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
                    self.ui
                        .game_ui
                        .ensure_ready(self.shared.config.timing.input.after_activate_ms)?;
                    self.ui
                        .game_ui
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
                let width = self.shared.config.screen.expected_width as i32;
                let height = self.shared.config.screen.expected_height as i32;
                if !(0..width).contains(&x) || !(0..height).contains(&y) {
                    return Err(anyhow!(
                        "坐标超出画布范围: x=0..{} y=0..{}",
                        width - 1,
                        height - 1
                    ));
                }
                self.ensure_web_tool_input_still_idle()?;
                self.ui
                    .game_ui
                    .ensure_ready(self.shared.config.timing.input.after_activate_ms)?;
                self.ui.game_ui.click_point(PointConfig::new(x, y))?;
                Ok(format!("已点击坐标: {x},{y}"))
            }
            WebToolRequest::Key { key } => {
                let key = parse_key(&key)?;
                self.ensure_web_tool_input_still_idle()?;
                self.ui
                    .game_ui
                    .ensure_ready(self.shared.config.timing.input.after_activate_ms)?;
                self.ui.game_ui.press_key(key)?;
                Ok("按键已发送".to_string())
            }
            WebToolRequest::ChatChangeSamples {
                samples,
                interval_ms,
            } => self.sample_web_tool_chat_changes(samples, interval_ms),
            WebToolRequest::PanelResponseBenchmark { rounds } => {
                self.run_web_tool_panel_benchmark(rounds)
            }
            WebToolRequest::CaptureApiBenchmark { rounds } => {
                self.ensure_web_tool_input_still_idle()?;
                self.ui.game_ui.benchmark_capture(rounds)
            }
            WebToolRequest::OcrBackendProbe => {
                let result = probe_ocr_backend_support(&self.ui.ocr_args)
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
                if !self.business.ai.enabled() {
                    return Err(anyhow!("AI 点歌未启用，请先配置 ai.api_key"));
                }
                let candidates = self
                    .playback
                    .player_search
                    .search_candidates(&keyword, "")?;
                let pick = if candidates.is_empty() {
                    None
                } else {
                    Some(self.business.ai.pick_song_candidate(
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
                    format!(
                        "{}. {} -> {}",
                        index + 1,
                        candidate.text,
                        candidate.track_ref.key
                    )
                }));
                if let Some(pick) = pick {
                    lines.push(format!(
                        "AI 选择: #{} score={:.2} reason={}",
                        pick.index, pick.score, pick.reason
                    ));
                }
                Ok(lines.join("\n"))
            }
        }
    }

    fn sample_web_tool_chat_changes(&self, samples: u32, interval_ms: u64) -> Result<String> {
        let baseline = self.latest_frame()?;
        let mut previous =
            rect_chat_change_fingerprint(&baseline, self.shared.config.screen.chat_rect.into())?;
        let mut lines = vec![format!(
            "采样次数={} 间隔={}ms，区域为一级聊天区",
            samples, interval_ms
        )];

        for index in 1..=samples {
            sleep(Duration::from_millis(interval_ms));
            let frame = self.latest_frame()?;
            let current =
                rect_chat_change_fingerprint(&frame, self.shared.config.screen.chat_rect.into())?;
            let stats = change_stats(&previous, &current);
            let changed = stats.mean_abs_diff >= self.shared.config.ocr.change_mean_threshold
                || stats.changed_ratio >= self.shared.config.ocr.change_pixel_threshold;
            let markers = if changed {
                let (blue, yellow, pink) = count_chat_markers(
                    &frame,
                    &self.ui.chat_templates,
                    self.shared.config.screen.chat_rect,
                )?;
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
        self.ui
            .game_ui
            .ensure_ready(self.shared.config.timing.input.after_activate_ms)?;
        let mut open_times = Vec::new();
        let mut close_times = Vec::new();
        let mut failures = 0u32;
        let detect_rect = web_tool_panel_response_rect(&self.shared.config);

        for _ in 0..rounds {
            self.ensure_web_tool_input_still_idle()?;
            self.ui.game_ui.press_key(Key::Escape)?;
            let closed = self.latest_frame()?;
            let closed = rect_chat_change_fingerprint(&closed, detect_rect)?;

            let opened_at = Instant::now();
            self.ensure_web_tool_input_still_idle()?;
            self.ui.game_ui.press_key(Key::Return)?;
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
            self.ui.game_ui.press_key(Key::Escape)?;
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

        let _ = self.ui.game_ui.press_key(Key::Escape);
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
            .task_engine
            .snapshot()?
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
            let changed = from_baseline.mean_abs_diff
                >= self.shared.config.ocr.change_mean_threshold
                || from_baseline.changed_ratio >= self.shared.config.ocr.change_pixel_threshold;
            let stable = from_previous.mean_abs_diff < self.shared.config.ocr.change_mean_threshold
                && from_previous.changed_ratio < self.shared.config.ocr.change_pixel_threshold;

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

/// 读取「此刻的界面状态」诊断文本。
///
/// **这是 `GET /tools/ui-state` 的唯一实现**（[`WebToolRequest::UiState`] 直接指向它）。
/// 修复前它读的是 `UiBundle.ui_runtime`——那个字段只属于主循环，在正式任务上下文里
/// 恒为 `None`，于是面板永远收到「UI runtime 尚未产生模板状态观察」。
///
/// 现在依赖被显式限制成两个**始终真实接线**的句柄：
/// - `game_ui`：走 UI 队列的截图前端（会让界面状态观察随帧推进）；
/// - `ui_handle`：界面状态观察的读侧句柄，与拥有的 `UiRuntime` 共享同一份状态。
///
/// 先主动截一帧，面板看到的就是「此刻」的状态，而不是上一次扫描留下的旧值。
/// 还没产生过任何观察时诚实返回错误：面板要能区分「还没扫过」与「状态正常」。
fn capture_ui_state_diagnostic(game_ui: &GameUi, ui_handle: &UiRuntimeHandle) -> Result<String> {
    game_ui.capture()?;
    ui_handle
        .latest_ui_state()
        .map(|state| state.diagnostic())
        .ok_or_else(|| anyhow!("UI runtime 尚未产生模板状态观察"))
}

#[cfg(test)]
mod ui_state_tests {
    use std::time::Duration;

    use anyhow::Result;
    use image::DynamicImage;

    use super::capture_ui_state_diagnostic;
    use crate::runtime::ui::{
        UiDevice, UiRuntime, UiStateClassification, UiStateKind, UiStateObservation,
    };
    use crate::ui::atoms::GameUi;

    /// 固定输出「一级界面」的分类器：让观察结果可预期，测试不依赖模板匹配。
    struct AlwaysPrimaryClassifier;

    impl crate::runtime::ui::UiStateClassifier for AlwaysPrimaryClassifier {
        fn classify(&mut self, _image: &DynamicImage) -> Result<UiStateClassification> {
            Ok(UiStateClassification::new(
                UiStateKind::Primary,
                "primary:regression",
            ))
        }
    }

    /// 空设备：截图永远成功，返回固定尺寸的空白图。
    struct BlankDevice;

    impl UiDevice for BlankDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(16, 16))
        }
    }

    /// `GET /tools/ui-state` 回归：正式接线下的共享句柄必须读到真实观察。
    ///
    /// 缺陷背景：旧实现读 `UiBundle.ui_runtime`（拥有的运行时）。正式任务上下文
    /// 没有它，于是 `GET /tools/ui-state` **必然**报「UI runtime 尚未产生模板状态观察」。
    /// 这条测试按生产接线的真实路径驱动：`UiRuntime` 产生观察 →
    /// 通过它的克隆句柄（`UiServices::ui_handle` 持有的正是同一份）读取。
    #[test]
    fn web_tool_ui_state_reads_the_observation_from_the_shared_handle() {
        // 稳定样本数 1：第一帧就产生稳定分类，测试不必等第二次截图。
        let runtime =
            UiRuntime::start_with_state_classifier(BlankDevice, 4, AlwaysPrimaryClassifier, 1)
                .expect("启动 UI 运行时");
        let handle = runtime.handle();
        let game_ui = GameUi::runtime(handle.clone());

        let diagnostic =
            capture_ui_state_diagnostic(&game_ui, &handle).expect("共享句柄必须读到界面状态");
        assert!(
            diagnostic.contains("primary:regression"),
            "诊断文本应来自真实分类器输出，实际：{diagnostic}"
        );
        assert!(
            !diagnostic.contains("尚未产生模板状态观察"),
            "共享句柄路径不允许再报「尚未产生模板状态观察」，实际：{diagnostic}"
        );
        // 截图确实推进了观察：句柄上的观察与截出来的帧对得上。
        let observation: UiStateObservation = handle.latest_ui_state().expect("观察必须存在");
        assert!(matches!(observation, UiStateObservation::Classified(_)));

        runtime.shutdown().expect("关闭 UI 运行时");
    }

    /// 还没产生观察时仍要诚实报错，不能拿默认值冒充「状态正常」。
    ///
    /// 没有分类器的运行时永远不写 `latest_ui_state`：这正是面板在极早期调用时
    /// 会遇到的真实情形。错误文案必须能让面板区分「还没扫过」与「状态正常」。
    #[test]
    fn web_tool_ui_state_reports_honestly_before_any_observation_exists() {
        let runtime = UiRuntime::start(BlankDevice, 4).expect("启动 UI 运行时");
        let handle = runtime.handle();
        assert!(
            handle.latest_ui_state().is_none(),
            "无分类器的运行时不应产生界面状态观察"
        );
        let game_ui = GameUi::runtime(handle.clone());

        let error =
            capture_ui_state_diagnostic(&game_ui, &handle).expect_err("没有观察时必须返回错误");
        assert!(
            error.to_string().contains("尚未产生模板状态观察"),
            "错误文案要保留既有口径，实际：{error}"
        );

        // 截图本身仍然成功：失败的是「没有观察」，不是「读不到画面」。
        assert!(game_ui.capture().is_ok(), "截图分支不应因为缺少观察而失败");
        // 让运行时关闭时不与仍在排队的截图竞争。
        std::thread::sleep(Duration::from_millis(10));
        runtime.shutdown().expect("关闭 UI 运行时");
    }

    /// 分类失败与「还没扫过」是两件事：前者必须**成功返回**诊断文本。
    ///
    /// 这是面板诊断里最容易混淆的一处：分类器出错时 `latest_ui_state` 里是
    /// [`UiStateObservation::Failed`]，不是 `None`。若 `capture_ui_state_diagnostic`
    /// 把它当成「没有观察」，面板只会收到一句「尚未产生模板状态观察」，
    /// 真实原因（例如模板文件缺失）被完全掩盖，排查时无从下手。
    ///
    /// 因此这里逐条锁死语义：有观察（哪怕是失败观察）就必须 `Ok`，
    /// 且文本里带得出分类器给出的原因。
    #[test]
    fn web_tool_ui_state_reports_a_classification_failure_instead_of_hiding_it() {
        struct FailingClassifier;

        impl crate::runtime::ui::UiStateClassifier for FailingClassifier {
            fn classify(&mut self, _image: &DynamicImage) -> Result<UiStateClassification> {
                anyhow::bail!("模板文件缺失")
            }
        }

        let runtime = UiRuntime::start_with_state_classifier(BlankDevice, 4, FailingClassifier, 1)
            .expect("启动 UI 运行时");
        let handle = runtime.handle();
        let game_ui = GameUi::runtime(handle.clone());

        let diagnostic = capture_ui_state_diagnostic(&game_ui, &handle)
            .expect("有失败观察时必须如实返回诊断文本，而不是报「没有观察」");
        assert!(
            diagnostic.contains("ui-state-error"),
            "失败观察的诊断必须以 ui-state-error 开头，实际：{diagnostic}"
        );
        assert!(
            diagnostic.contains("模板文件缺失"),
            "分类失败原因必须原样带出，实际：{diagnostic}"
        );
        assert!(
            !diagnostic.contains("尚未产生模板状态观察"),
            "失败观察不能被误报成「还没有观察」，实际：{diagnostic}"
        );
        assert!(
            matches!(
                handle.latest_ui_state(),
                Some(UiStateObservation::Failed { .. })
            ),
            "分类失败必须落成失败观察，面板才能区分缺证据与缺模板"
        );

        runtime.shutdown().expect("关闭 UI 运行时");
    }

    /// 接线锁定：`WebToolRequest::UiState` 只允许读**共享**的两个句柄。
    ///
    /// 类型层面已经堵死了老缺陷的一半：`AppServices` 里根本没有 `owned` 字段，
    /// 读不到主循环独占的 `UiRuntime`。但「再加一个字段把运行时塞回共享服务」
    /// 这种回退是能通过编译的，所以这里把派发点的实参写成断言：
    ///
    /// - 必须调 `capture_ui_state_diagnostic(&self.ui.game_ui, &self.ui.ui_handle)`；
    /// - 不允许出现 `ui_runtime`（拥有型运行时在任务上下文里恒不可用）。
    ///
    /// 源码级断言不是风格洁癖：这条分支曾经就是「面板永远报没有观察」的根因，
    /// 而它的回归不会产生任何编译错误或运行时 panic，只有面板功能静默失效。
    #[test]
    fn web_tool_ui_state_dispatch_reads_only_the_shared_handles() {
        const SOURCE: &str = include_str!("diagnostics.rs");
        let arm = SOURCE
            .split("WebToolRequest::UiState =>")
            .nth(1)
            .expect("派发点必须保留 UiState 分支");
        let arm = &arm[..arm.find('}').expect("分支必须是一条表达式")];
        assert!(
            arm.contains("capture_ui_state_diagnostic(&self.ui.game_ui, &self.ui.ui_handle)"),
            "UiState 必须读取共享的 game_ui 与 ui_handle，实际分支：{arm}"
        );

        // 注释里**必须**留着旧字段名（那是这条缺陷的由来），所以只检查代码行。
        // 待查词本身也要拼出来：写成字面量会让本测试自己的这一行成为命中项。
        let forbidden = concat!("ui_", "runtime");
        let offenders: Vec<&str> = SOURCE
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains(forbidden))
            .collect();
        assert!(
            offenders.is_empty(),
            "诊断模块的代码不得引用拥有型运行时（它在正式任务上下文里不可用，\
旧实现正是因此让 /tools/ui-state 必然失败）：{offenders:?}"
        );
    }

    /// 截图失败必须原样冒泡：面板要能看到真实原因，而不是被「没有观察」掩盖。
    #[test]
    fn web_tool_ui_state_surfaces_the_capture_failure() {
        struct FailingDevice;

        impl UiDevice for FailingDevice {
            fn capture(&mut self) -> Result<DynamicImage> {
                anyhow::bail!("simulated capture failure")
            }
        }

        let runtime =
            UiRuntime::start_with_state_classifier(FailingDevice, 4, AlwaysPrimaryClassifier, 1)
                .expect("启动 UI 运行时");
        let handle = runtime.handle();
        let game_ui = GameUi::runtime(handle.clone());

        // 直接走生产函数：失败原因必须冒泡，不能静默变成「没有观察」。
        let error =
            capture_ui_state_diagnostic(&game_ui, &handle).expect_err("截图失败时必须返回错误");
        let text = format!("{error:#}");
        assert!(
            text.contains("simulated capture failure"),
            "截图失败原因必须冒泡，实际：{text}"
        );

        runtime.shutdown().expect("关闭 UI 运行时");
    }
}
