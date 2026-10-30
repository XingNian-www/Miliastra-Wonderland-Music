use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, wait_ms,
};
use super::state_observation::wait_for_stable_ui_kind;
use crate::adapters::game_path::resolve_game_executable;
use crate::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::geometry::{Point, Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiStateKind, UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::state::find_enter_game_text as locate_enter_game_text;
use crate::ui::state::world_is_teyvat;
use crate::ui::template::best_template_hit;
use enigo::Key;

const TEMPLATE_STABLE_HITS: u32 = 2;

struct TemplateAbsence<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    stage: &'static str,
    failure_message: &'static str,
}

struct TemplateHit<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    poll_ms: u64,
    certainty: InputCertainty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterGame {
    pub(crate) require_gate: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterWonderland {
    pub(crate) require_overworld: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterGameEffect {
    WindowReady,
    Entered,
    Skipped,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterGameOutcome {
    effect: EnterGameEffect,
}

impl EnterGameOutcome {
    pub(crate) fn effect(&self) -> &EnterGameEffect {
        &self.effect
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterWonderlandEffect {
    Entered,
    Skipped,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterWonderlandOutcome {
    effect: EnterWonderlandEffect,
    residency: Option<UiResidencyOutcome>,
}

impl EnterWonderlandOutcome {
    pub(crate) fn effect(&self) -> &EnterWonderlandEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> Option<&UiResidencyOutcome> {
        self.residency.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct StartupUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl StartupUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: StartupRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
        }
    }

    pub(crate) fn submit_enter_game(
        &self,
        request: EnterGame,
    ) -> Result<UiOperation<EnterGameOutcome>, UiSubmitError> {
        self.runtime.submit(EnterGameRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }

    pub(crate) fn submit_enter_wonderland(
        &self,
        request: EnterWonderland,
    ) -> Result<UiOperation<EnterWonderlandOutcome>, UiSubmitError> {
        self.runtime.submit(EnterWonderlandRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct StartupRoutineConfig {
    startup: StartupUiConfig,
    residency: FriendDeliveryRoutineConfig,
    target_process: String,
}

#[derive(Clone)]
pub(crate) struct StartupUiConfig {
    pub(crate) launch_game: bool,
    /// 外部聊天源配置。启用时游戏改由 Miliastra-Wonderland-ChatBridge 拉起，
    /// 因为反作弊只允许"创建游戏进程的一方"注入 DLL。
    pub(crate) external_launcher: Option<crate::config::ChatSourceConfig>,
    pub(crate) enter_game: bool,
    pub(crate) exe_path: PathBuf,
    pub(crate) game_args: String,
    pub(crate) launch_wait_ms: u64,
    pub(crate) launch_retries: u32,
    pub(crate) enter_game_timeout_ms: u64,
    pub(crate) enter_wonderland_timeout_ms: u64,
    pub(crate) wonderland_map_star_retries: u32,
    pub(crate) wonderland_map_star_retry_ms: u64,
    pub(crate) wonderland_hall_retries: u32,
    pub(crate) wonderland_hall_retry_ms: u64,
    pub(crate) wonderland_transition_timeout_ms: u64,
    pub(crate) wonderland_confirm_stable_timeout_ms: u64,
    pub(crate) final_primary_timeout_ms: u64,
    pub(crate) poll_ms: u64,
    pub(crate) stable_mean_threshold: f32,
    pub(crate) stable_changed_ratio_threshold: f32,
    pub(crate) template_threshold: f32,
    pub(crate) wonderland_confirm_threshold: f32,
    pub(crate) world_wish_threshold: f32,
    pub(crate) templates: StartupUiTemplates,
    pub(crate) enter_game_text_region: Rect,
    pub(crate) world_wish_region: Rect,
    pub(crate) wonderland_hall_ocr_region: Rect,
    pub(crate) wonderland_confirm_region: Rect,
    pub(crate) main_ui_region: Rect,
    pub(crate) wonderland_map_star_region: Rect,
}

#[derive(Clone)]
pub(crate) struct StartupUiTemplates {
    pub(crate) wonderland_map_star: PathBuf,
    pub(crate) wonderland_confirm: PathBuf,
    pub(crate) paimon_menu: PathBuf,
    pub(crate) world_wish: PathBuf,
}

impl StartupRoutineConfig {
    pub(crate) fn resolve(
        startup: StartupUiConfig,
        residency: FriendDeliveryRoutineConfig,
        target_process: String,
    ) -> Self {
        Self {
            startup,
            residency,
            target_process,
        }
    }
}

struct EnterGameRoutine {
    request: EnterGame,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl sealed::UiRoutineSealed for EnterGameRoutine {}

impl UiRoutine for EnterGameRoutine {
    type Output = EnterGameOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let mut config = self.config;
        if self.request.require_gate {
            match confirm_automatic_entry(context, &self.ocr, &config, UiStateKind::GameGate) {
                Ok(true) => {
                    config.startup.launch_game = false;
                    config.startup.enter_game = true;
                }
                Ok(false) => {
                    return EnterGameOutcome {
                        effect: EnterGameEffect::Skipped,
                    };
                }
                Err(failure) => {
                    return EnterGameOutcome {
                        effect: EnterGameEffect::Failed(failure),
                    };
                }
            }
        }
        let effect = match execute_enter_game(context, &self.ocr, &config) {
            Ok(effect) => effect,
            Err(failure) => EnterGameEffect::Failed(failure),
        };
        EnterGameOutcome { effect }
    }
}

struct EnterWonderlandRoutine {
    request: EnterWonderland,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl sealed::UiRoutineSealed for EnterWonderlandRoutine {}

impl UiRoutine for EnterWonderlandRoutine {
    type Output = EnterWonderlandOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        if self.request.require_overworld {
            match confirm_automatic_entry(context, &self.ocr, &self.config, UiStateKind::Overworld)
            {
                Ok(true) => {}
                Ok(false) => {
                    return EnterWonderlandOutcome {
                        effect: EnterWonderlandEffect::Skipped,
                        residency: None,
                    };
                }
                Err(failure) => {
                    return EnterWonderlandOutcome {
                        effect: EnterWonderlandEffect::Failed(failure),
                        residency: None,
                    };
                }
            }
        }
        let mut goal_attempted = false;
        let effect =
            match execute_enter_wonderland(context, &self.ocr, &self.config, &mut goal_attempted) {
                Ok(()) => EnterWonderlandEffect::Entered,
                Err(failure) => EnterWonderlandEffect::Failed(failure),
            };
        let residency = wait_for_primary(
            context,
            &self.config,
            !goal_attempted,
            self.config.startup.final_primary_timeout_ms,
        );
        EnterWonderlandOutcome {
            effect,
            residency: Some(residency),
        }
    }
}

fn confirm_automatic_entry(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    expected: UiStateKind,
) -> Result<bool, UiRoutineFailure> {
    for _ in 0..TEMPLATE_STABLE_HITS {
        let image = capture_normalized(
            context,
            &config.residency,
            "recheck_automatic_entry",
            InputCertainty::BeforeInput,
        )?;
        let visible = match expected {
            UiStateKind::GameGate => {
                find_enter_game_text(ocr, &image, config, InputCertainty::BeforeInput)?.is_some()
            }
            UiStateKind::Overworld => template_visible(
                &image,
                config.startup.world_wish_region,
                &config.startup.templates.world_wish,
                config.startup.world_wish_threshold,
                InputCertainty::BeforeInput,
            )?,
            _ => unreachable!("automatic entry only starts from the gate or overworld"),
        };
        if !visible {
            // 界面已变化说明玩家自己切走了，跳过是正常行为，不是失败；
            // 但「跳过了什么」对排查有价值，保持 info 偏噪音，降到 debug。
            log::debug!(
                "自动进入游戏任务跳过（步骤：复查自动进入条件）：界面已变化，不再满足进入前提，本次不执行自动进入；本次期望的界面是「{}」，实际已不是它",
                expected.label()
            );
            return Ok(false);
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "recheck_automatic_entry",
            InputCertainty::BeforeInput,
        )?;
    }
    Ok(true)
}

fn execute_enter_game(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
) -> Result<EnterGameEffect, UiRoutineFailure> {
    ensure_game_window(context, config)?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_game_window", error))?;
    if !config.startup.enter_game {
        return Ok(EnterGameEffect::WindowReady);
    }

    let deadline = Instant::now() + Duration::from_millis(config.startup.enter_game_timeout_ms);
    let mut paimon_streak = 0_u32;
    let mut clicked = false;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_enter_game",
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
            paimon_streak = paimon_streak.saturating_add(1);
            if paimon_streak >= TEMPLATE_STABLE_HITS {
                return Ok(EnterGameEffect::Entered);
            }
        } else {
            paimon_streak = 0;
        }

        if let Some(point) = find_enter_game_text(
            ocr,
            &image,
            config,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "click_enter_game",
                        format!("{error:#}"),
                    )
                })?;
            clicked = true;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_enter_game",
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
    }
    Err(UiRoutineFailure::new(
        if clicked {
            InputCertainty::AfterInputUnknown
        } else {
            InputCertainty::ConfirmedFailure
        },
        "confirm_enter_game",
        "paimon menu template did not become stable before timeout",
    ))
}

/// 让 Miliastra-Wonderland-ChatBridge 拉起游戏，并等待窗口出现。
///
/// Miliastra-Wonderland-ChatBridge 会用 CREATE_SUSPENDED 启动游戏、注入 DLL、再恢复主线程，
/// 之后游戏窗口才出现。BOT 这里只负责等。
fn ensure_game_window_via_chat_api(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    chat_source: &crate::config::ChatSourceConfig,
) -> Result<(), UiRoutineFailure> {
    crate::runtime::chat_source::launcher::ensure_running(chat_source).map_err(|error| {
        UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "launch_chat_api",
            format!("{error:#}"),
        )
    })?;
    for _ in 0..config.startup.launch_retries.max(1) {
        wait_ms(
            context,
            config.startup.launch_wait_ms,
            "wait_game_window",
            InputCertainty::AfterInputUnknown,
        )?;
        if context.device().ensure_window().is_ok() {
            return Ok(());
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_game_window",
        "Miliastra-Wonderland-ChatBridge 启动了游戏但窗口一直没出现",
    ))
}

fn ensure_game_window(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    if context.device().ensure_window().is_ok() {
        return Ok(());
    }
    // 外部聊天源接管游戏启动：只有 Miliastra-Wonderland-ChatBridge 能创建可注入的游戏进程。
    if let Some(chat_source) = config.startup.external_launcher.as_ref() {
        return ensure_game_window_via_chat_api(context, config, chat_source);
    }
    if !config.startup.launch_game {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "ensure_game_window",
            "game window is missing and startup.launch_game is false",
        ));
    }
    let executable = resolve_game_executable(&config.startup.exe_path, &config.target_process)
        .map_err(|error| before_input_failure("resolve_game_path", error))?;
    if !executable.exists() {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "resolve_game_path",
            format!("game executable does not exist: {}", executable.display()),
        ));
    }
    let args = split_command_args(&config.startup.game_args)
        .map_err(|error| before_input_failure("parse_game_args", error))?;
    context
        .device()
        .launch_game(&executable, &args)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "launch_game",
                format!("{error:#}"),
            )
        })?;
    for _ in 0..config.startup.launch_retries.max(1) {
        wait_ms(
            context,
            config.startup.launch_wait_ms,
            "wait_game_window",
            InputCertainty::AfterInputUnknown,
        )?;
        if context.device().ensure_window().is_ok() {
            return Ok(());
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_game_window",
        "launched game process but target window did not appear",
    ))
}

fn execute_enter_wonderland(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    goal_attempted: &mut bool,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_window()
        .map_err(|error| before_input_failure("ensure_wonderland_window", error))?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_wonderland_window", error))?;
    wait_for_paimon_menu(context, config)?;

    context
        .device()
        .press_key(Key::Unicode('m'))
        .map_err(|error| after_input_failure("open_wonderland_map", error))?;

    // **先确认地图真的开了，再去找星标。**
    //
    // 原来这里直接进 `wait_template_hit` 重试 120 次 x 2.5 秒（最长 5 分钟），
    // 而 `M` 是有可能不生效的（例如聊天框正开着，`M` 被打进了输入框）。
    // 那种情况下机器人会白等 5 分钟，最后把失败报成「找不到星标」——
    // **诊断是错的**，看不到「地图根本没打开」这个真正的原因。
    //
    // 为什么不需要给提瓦特地图单独建判据：第 71 节实测，提瓦特地图 4 张 + 千星地图 1 张
    // **全部不是 `overworld`**，世界判据一律报 `not_applicable`（地图盖住了左上角 HUD）；
    // 而真·提瓦特大世界 3 张对照全部是 `overworld` / `teyvat`。
    // 所以「世界判据由真变假」就是地图已开的充分可观测证据。
    // （第 71 节也试过为提瓦特地图单独建区域/二维统计判据，全部过拟合失败，见该节。）
    wait_for_wonderland_map_open(context, config)?;

    let map_attempts = capped_attempts(
        config.startup.wonderland_map_star_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_map_star_retry_ms,
    );
    let map_interval_ms = config.startup.wonderland_map_star_retry_ms.max(100);
    let map_timeout_ms = (map_attempts as u64).saturating_mul(map_interval_ms).min(
        config
            .startup
            .enter_wonderland_timeout_ms
            .max(map_interval_ms),
    );
    let map_star = wait_template_hit(
        context,
        config,
        TemplateHit {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: map_timeout_ms,
            poll_ms: map_interval_ms,
            certainty: InputCertainty::AfterInputUnknown,
        },
    )?;
    let Some(map_star) = map_star else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_map_star",
            "wonderland map star template was not found",
        ));
    };
    context
        .device()
        .click_point(map_star.x, map_star.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "click_wonderland_map_star",
                format!("{error:#}"),
            )
        })?;
    *goal_attempted = true;
    wait_template_absent(
        context,
        config,
        TemplateAbsence {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: config.startup.wonderland_transition_timeout_ms,
            stage: "confirm_wonderland_map_star_absent",
            failure_message: "wonderland map star template did not disappear",
        },
    )?;

    let hall_attempts = capped_attempts(
        config.startup.wonderland_hall_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_hall_retry_ms,
    );
    let Some(hall_point) = wait_wonderland_hall_text(context, ocr, config, hall_attempts)? else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_hall",
            "OCR did not find a 千星奇域/大厅 option",
        ));
    };
    context
        .device()
        .click_point(hall_point.x, hall_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "select_wonderland_hall",
                format!("{error:#}"),
            )
        })?;

    let Some(confirm_point) = wait_template_hit(
        context,
        config,
        TemplateHit {
            template: &config.startup.templates.wonderland_confirm,
            region: config.startup.wonderland_confirm_region,
            threshold: config.startup.wonderland_confirm_threshold,
            timeout_ms: config.startup.wonderland_transition_timeout_ms,
            poll_ms: config.startup.poll_ms,
            certainty: InputCertainty::AfterInputUnknown,
        },
    )?
    else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_confirm",
            "wonderland confirm template was not found",
        ));
    };
    context
        .device()
        .click_point(confirm_point.x, confirm_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_enter_wonderland",
                format!("{error:#}"),
            )
        })?;
    confirm_wonderland_transition(context, config)
}

/// 按 `M` 之后确认「地图真的盖住了大世界」。
///
/// 证据（第 71 节，`technical-docs/ui-state-criteria.md` 第 27 章）：
///
/// | 帧 | 实际是什么 | app 判定 |
/// |---|---|---|
/// | 提瓦特地图 x4（`c01-map` / `c02-map-fresh` / `tv_map_1..4`）| 地图 | **不是 `overworld`**，世界判据 `not_applicable` |
/// | 千星地图 x1（`map-screen.png`）| 地图 | **不是 `overworld`**，世界判据 `not_applicable` |
/// | 真·提瓦特大世界 x3（`t-00` / `t-10` / `snow-01`）| 大世界 | `overworld` / 世界判据 `teyvat` |
///
/// 所以这里**不需要**为提瓦特地图另建判据，只要看世界判据有没有由真变假。
/// 连续 `TEMPLATE_STABLE_HITS` 次为假才算数，避免中间夹一帧过渡画面就误判。
fn wait_for_wonderland_map_open(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    let mut left_world = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_wonderland_map",
            InputCertainty::AfterInputUnknown,
        )?;
        if !world_is_teyvat(&image) {
            left_world = left_world.saturating_add(1);
            if left_world >= TEMPLATE_STABLE_HITS {
                return Ok(());
            }
        } else {
            left_world = 0;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "open_wonderland_map",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "open_wonderland_map",
        "pressed M but the world criterion still holds: the map did not open (key likely swallowed by an input box)",
    ))
}

fn wait_for_paimon_menu(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(config.residency.timeout_ms());
    let mut streak = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_wonderland_primary",
            InputCertainty::ConfirmedFailure,
        )?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            InputCertainty::BeforeInput,
        )? {
            streak = streak.saturating_add(1);
            if streak >= TEMPLATE_STABLE_HITS {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "observe_wonderland_primary",
            InputCertainty::ConfirmedFailure,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "observe_wonderland_primary",
        "paimon menu template did not become stable before timeout",
    ))
}

fn confirm_wonderland_transition(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let region = config.startup.wonderland_confirm_region;
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_absent",
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            region,
            &config.startup.templates.wonderland_confirm,
            config.startup.wonderland_confirm_threshold,
            InputCertainty::AfterInputUnknown,
        )? {
            return wait_region_stable(context, config, region);
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_wonderland_absent",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_absent",
        "wonderland confirmation template did not disappear",
    ))
}

fn wait_region_stable(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    region: Rect,
) -> Result<(), UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_confirm_stable_timeout_ms);
    let image = capture_normalized(
        context,
        &config.residency,
        "observe_wonderland_transition",
        InputCertainty::AfterInputUnknown,
    )?;
    let mut previous = rect_chat_change_fingerprint(&image, region)
        .map_err(|error| after_input_failure("observe_wonderland_transition", error))?;
    while Instant::now() < deadline {
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_wonderland_stable",
            InputCertainty::AfterInputUnknown,
        )?;
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_stable",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = rect_chat_change_fingerprint(&image, region)
            .map_err(|error| after_input_failure("confirm_wonderland_stable", error))?;
        let stats = change_stats(&previous, &current);
        if stats.mean_abs_diff <= config.startup.stable_mean_threshold
            && stats.changed_ratio <= config.startup.stable_changed_ratio_threshold
        {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_stable",
        "wonderland transition region did not become stable",
    ))
}

fn wait_for_primary(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    allow_escape: bool,
    timeout_ms: u64,
) -> UiResidencyOutcome {
    let result = if allow_escape {
        match wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            None,
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::ConfirmedFailure,
        ) {
            Ok(UiStateKind::Primary) => Ok(UiStateKind::Primary),
            // 改动前主菜单是被判成 Secondary 的，这里把 MainMenu 并进同一支，
            // 保持启动流程原有行为：按 Escape 关掉浮层再等一级聊天出现（§44.10）。
            Ok(UiStateKind::Secondary | UiStateKind::MainMenu) => {
                if let Err(error) = context.device().press_key(Key::Escape) {
                    return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "recover_startup_primary",
                        format!("{error:#}"),
                    ));
                }
                wait_for_stable_ui_kind(
                    context,
                    config.residency.state_observation(),
                    Some(UiStateKind::Primary),
                    timeout_ms,
                    "confirm_startup_primary",
                    InputCertainty::AfterInputUnknown,
                )
            }
            Ok(UiStateKind::Unknown) => unreachable!("unknown UI state is never stable"),
            // Loading 也归到失败：等待的稳定状态就是「到大厅了」，
            // 而稳定停在加载界面说明卡住了，不该被当成还没开始（§55）。
            Ok(
                UiStateKind::GameGate
                | UiStateKind::Overworld
                | UiStateKind::Loading
                | UiStateKind::Panel,
            ) => Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "confirm_startup_primary",
                "game is still at the gate, loading, in the overworld, or showing a modal panel",
            )),
            Err(failure) => Err(failure),
        }
    } else {
        wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            Some(UiStateKind::Primary),
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::AfterInputUnknown,
        )
    };
    match result {
        Ok(UiStateKind::Primary) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        Ok(_) => unreachable!("primary wait returned a non-primary state"),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    }
}

fn find_enter_game_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
    certainty: InputCertainty,
) -> Result<Option<Point>, UiRoutineFailure> {
    locate_enter_game_text(ocr, image, config.startup.enter_game_text_region).map_err(|error| {
        UiRoutineFailure::new(certainty, "ocr_enter_game_text", format!("{error:#}"))
    })
}

fn wait_template_absent(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateAbsence<'_>,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms.max(1));
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            wait.stage,
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            wait.region,
            wait.template,
            wait.threshold,
            InputCertainty::AfterInputUnknown,
        )? {
            return Ok(());
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            wait.stage,
            InputCertainty::AfterInputUnknown,
        )?;
    }
    // 超时是**排障入口**：trace 级只在显式开启时可见，这里用 debug 给出
    // 「哪个模板、哪个区域、期望多少」——没有这些，投递/启动卡住时无从判断
    // 是模板不匹配还是界面根本不对。
    log::debug!(
        "等待模板「{}」消失超时（步骤：{}）：搜索区域 ({}, {}) {}×{}，期望分数 {:.3}，超时 {}ms 内一直判为可见；本次按「仍可见」继续处理",
        wait.template.display(),
        wait.stage,
        wait.region.x,
        wait.region.y,
        wait.region.width,
        wait.region.height,
        wait.threshold,
        wait.timeout_ms
    );
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        wait.stage,
        wait.failure_message,
    ))
}

fn wait_wonderland_hall_text(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    attempts: u32,
) -> Result<Option<Point>, UiRoutineFailure> {
    for _ in 0..attempts.max(1) {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_wonderland_hall",
            InputCertainty::AfterInputUnknown,
        )?;
        if let Some(point) = find_wonderland_hall_text(ocr, &image, config)? {
            return Ok(Some(point));
        }
        wait_ms(
            context,
            config.startup.wonderland_hall_retry_ms.max(100),
            "locate_wonderland_hall",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Ok(None)
}

fn find_wonderland_hall_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
) -> Result<Option<Point>, UiRoutineFailure> {
    let region = config.startup.wonderland_hall_ocr_region;
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_wonderland_hall", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_wonderland_hall", error.into()))?;
    Ok(lines.into_iter().find_map(|line| {
        is_wonderland_hall_text(&line.text).then(|| {
            Point::new(
                region.x + line.bbox.center().x,
                region.y + line.bbox.center().y,
            )
        })
    }))
}

fn is_wonderland_hall_text(text: &str) -> bool {
    let normalized = normalize_lock_text(text);
    normalized.contains("千星奇域") || normalized.contains("大厅")
}

fn wait_template_hit(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateHit<'_>,
) -> Result<Option<Point>, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_startup_template",
            wait.certainty,
        )?;
        if let Some(hit) =
            best_template_hit(&image, Some(wait.region), wait.template, wait.threshold).map_err(
                |error| {
                    UiRoutineFailure::new(
                        wait.certainty,
                        "locate_startup_template",
                        format!("{error:#}"),
                    )
                },
            )?
        {
            return Ok(Some(hit.center()));
        }
        wait_ms(
            context,
            wait.poll_ms.max(1),
            "locate_startup_template",
            wait.certainty,
        )?;
    }
    log::debug!(
        "等待模板「{}」出现超时（步骤：定位启动流程模板）：搜索区域 ({}, {}) {}×{}，期望分数 {:.3}，超时 {}ms 内未命中；本次按「未找到」继续",
        wait.template.display(),
        wait.region.x,
        wait.region.y,
        wait.region.width,
        wait.region.height,
        wait.threshold,
        wait.timeout_ms
    );
    Ok(None)
}

fn template_visible(
    image: &image::DynamicImage,
    region: Rect,
    template: &Path,
    threshold: f32,
    certainty: InputCertainty,
) -> Result<bool, UiRoutineFailure> {
    best_template_hit(image, Some(region), template, threshold)
        .map(|hit| hit.is_some())
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "match_startup_template", format!("{error:#}"))
        })
}

fn split_command_args(value: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if quoted {
        anyhow::bail!("startup.game_args contains an unclosed quote");
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn capped_attempts(configured_retries: u32, timeout_ms: u64, interval_ms: u64) -> u32 {
    let interval_ms = interval_ms.max(1);
    let attempts = timeout_ms.max(interval_ms).div_ceil(interval_ms) as u32;
    configured_retries.max(1).min(attempts)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use anyhow::Result;
    use image::DynamicImage;

    use super::*;

    struct PaimonOnlyDevice {
        frame: DynamicImage,
        /// 按下 M 之后改用的真实帧（例如提瓦特地图）；None 表示画面不变。
        after_m: Option<DynamicImage>,
        keys: Arc<Mutex<Vec<Key>>>,
    }

    struct EmptyOcr;

    impl OcrDevice for EmptyOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    impl UiDevice for PaimonOnlyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            if let Some(after_m) = &self.after_m {
                if self.keys.lock().unwrap().contains(&Key::Unicode('m')) {
                    return Ok(after_m.clone());
                }
            }
            Ok(self.frame.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }

        fn ensure_window(&mut self) -> Result<()> {
            Ok(())
        }

        fn focus(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }
    }

    fn test_startup_config(app: &AppConfig) -> StartupRoutineConfig {
        let startup = &app.startup;
        StartupRoutineConfig::resolve(
            StartupUiConfig {
                external_launcher: None,
                launch_game: startup.launch_game,
                enter_game: startup.enter_game,
                exe_path: startup.exe_path.clone(),
                game_args: startup.game_args.clone(),
                launch_wait_ms: startup.launch_wait_ms,
                launch_retries: startup.launch_retries,
                enter_game_timeout_ms: startup.enter_game_timeout_ms,
                enter_wonderland_timeout_ms: startup.enter_wonderland_timeout_ms,
                wonderland_map_star_retries: startup.wonderland_map_star_retries,
                wonderland_map_star_retry_ms: startup.wonderland_map_star_retry_ms,
                wonderland_hall_retries: startup.wonderland_hall_retries,
                wonderland_hall_retry_ms: startup.wonderland_hall_retry_ms,
                wonderland_transition_timeout_ms: startup.wonderland_transition_timeout_ms,
                wonderland_confirm_stable_timeout_ms: startup.wonderland_confirm_stable_timeout_ms,
                final_primary_timeout_ms: startup.final_primary_timeout_ms,
                poll_ms: 1,
                stable_mean_threshold: startup.stable_mean_threshold,
                stable_changed_ratio_threshold: startup.stable_changed_ratio_threshold,
                template_threshold: startup.template_threshold,
                wonderland_confirm_threshold: startup.wonderland_confirm_threshold,
                world_wish_threshold: app.templates.marker_threshold,
                templates: StartupUiTemplates {
                    wonderland_map_star: startup.templates.wonderland_map_star.clone(),
                    wonderland_confirm: startup.templates.wonderland_confirm.clone(),
                    paimon_menu: startup.templates.paimon_menu.clone(),
                    world_wish: app.templates.world_wish.clone(),
                },
                enter_game_text_region: startup.enter_game_text_region.into(),
                world_wish_region: app.screen.world_wish_rect.into(),
                wonderland_hall_ocr_region: startup.wonderland_hall_ocr_region.into(),
                wonderland_confirm_region: startup.wonderland_confirm_region.into(),
                main_ui_region: startup.main_ui_region.into(),
                wonderland_map_star_region: startup.wonderland_map_star_region.into(),
            },
            FriendDeliveryRoutineConfig::from_app(app),
            app.window.target_process.clone(),
        )
    }

    #[test]
    fn enter_wonderland_accepts_a_paimon_only_primary_screen() {
        let app = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let mut config = test_startup_config(&app);
        config.startup.enter_wonderland_timeout_ms = 1;
        config.startup.wonderland_map_star_retries = 1;
        config.startup.wonderland_map_star_retry_ms = 1;

        // 真实提瓦特大世界帧：派蒙图标天然在左上角，但没有千星家园模板。
        // 按 M 之后切到真实提瓦特地图帧——地图上没有千星星标，
        // 所以例程必须失败在 locate_wonderland_map_star。
        let frame = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg")
            .expect("打开真实提瓦特帧失败");
        let map = image::open("tests/fixtures/ui/map-teyvat-1920x1080.png")
            .expect("打开真实提瓦特地图帧失败");

        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                after_m: Some(map),
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        config.startup.final_primary_timeout_ms = 1;
        let outcome = ui_runtime
            .handle()
            .submit(EnterWonderlandRoutine {
                request: EnterWonderland::default(),
                ocr: ocr_runtime.handle(),
                config,
            })
            .unwrap()
            .wait()
            .unwrap();

        let EnterWonderlandEffect::Failed(failure) = outcome.effect() else {
            panic!("the fixture intentionally has no wonderland home template");
        };
        assert_eq!(failure.stage(), "locate_wonderland_map_star");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        assert_eq!(keys.lock().unwrap().as_slice(), &[Key::Unicode('m')]);
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn automatic_entry_skips_both_actions_when_the_screen_changed() {
        let app = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let config = test_startup_config(&app);
        let keys = Arc::new(Mutex::new(Vec::new()));
        // 真实过场（暗色加载屏）帧：游戏还在加载，既没有"点击进入"文案，
        // 也没有大世界顶栏锚点——"界面已变化"的真实形态，两个自动进入动作都必须跳过。
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame: image::open("tests/fixtures/ui/loading-dark-1920x1080.jpg")
                    .expect("打开真实过场帧失败"),
                after_m: None,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let startup = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config);
        let gate = startup
            .submit_enter_game(EnterGame { require_gate: true })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(gate.effect(), &EnterGameEffect::Skipped);
        let world = startup
            .submit_enter_wonderland(EnterWonderland {
                require_overworld: true,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(world.effect(), &EnterWonderlandEffect::Skipped);
        assert!(world.residency().is_none());
        assert!(keys.lock().unwrap().is_empty());
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn entering_game_finishes_in_the_overworld_without_claiming_chat_residency() {
        let app = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let mut config = test_startup_config(&app);
        config.startup.enter_game = true;
        // 真实提瓦特大世界帧：进入游戏后的落地画面，派蒙图标在左上角。
        let frame = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg")
            .expect("打开真实提瓦特帧失败");
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                after_m: None,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let outcome = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config)
            .submit_enter_game(EnterGame::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(outcome.effect(), &EnterGameEffect::Entered);
        assert!(keys.lock().unwrap().is_empty());
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn wonderland_hall_text_accepts_either_marker() {
        assert!(is_wonderland_hall_text("千星奇域·大厅"));
        assert!(is_wonderland_hall_text("千星奇域"));
        assert!(is_wonderland_hall_text("大厅"));
        assert!(!is_wonderland_hall_text("千星"));
    }

    #[test]
    fn split_command_args_keeps_quoted_text() {
        assert_eq!(
            split_command_args(r#"-screen-fullscreen 0 "-window-title test""#).unwrap(),
            ["-screen-fullscreen", "0", "-window-title test"]
        );
    }

    #[test]
    fn split_command_args_rejects_unclosed_quote() {
        assert!(split_command_args(r#""abc"#).is_err());
    }
}
