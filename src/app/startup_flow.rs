use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::config::{AppConfig, PointConfig, RectConfig};
use super::geometry::Point;
use super::template_match::{TemplateHit, best_template_hit};
use super::ui_locator::{UiLocator, startup_locator, startup_transition_locator};
use super::window;
use super::workflow_actions;
use super::workflow_actions::{HitAction, PixelStability, TemplateMode};

const STARTUP_TEMPLATE_STABLE_HITS: u32 = 2;

#[derive(Clone, Copy, Debug)]
enum WonderlandStep {
    OpenWonderlandHome,
    ClickWonderlandCard,
    WaitConfirmGone,
}

impl WonderlandStep {
    fn label(self) -> &'static str {
        match self {
            Self::OpenWonderlandHome => "打开千星奇域主页",
            Self::ClickWonderlandCard => "点击千星奇域卡片",
            Self::WaitConfirmGone => "等待前往大厅按钮消失",
        }
    }
}

pub(super) fn enter_wonderland<F>(config: &AppConfig, mut should_continue: F) -> Result<()>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    log::info!("进入千星流程: 开始");
    window::GameWindow::find(&config.window)
        .context("进入千星前未找到游戏窗口，请先执行启动游戏任务")?;
    workflow_actions::focus(&config.window, config.timing.input.after_activate_ms)
        .context("进入千星流程聚焦游戏窗口失败")?;

    let locator = startup_locator(config);
    execute_wonderland_steps(config, &locator, &mut should_continue)?;

    log::info!("进入千星流程: 已完成，耗时 {}ms", elapsed_ms(started));
    Ok(())
}

fn execute_wonderland_steps<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let mut step = WonderlandStep::OpenWonderlandHome;
    loop {
        log::info!("进入千星流程步骤: {}", step.label());
        step = match step {
            WonderlandStep::OpenWonderlandHome => {
                open_wonderland_home(config, locator, should_continue)?;
                WonderlandStep::ClickWonderlandCard
            }
            WonderlandStep::ClickWonderlandCard => {
                click_wonderland_card(config, locator, should_continue)?;
                WonderlandStep::WaitConfirmGone
            }
            WonderlandStep::WaitConfirmGone => {
                wait_enter_confirm_gone(config, should_continue)?;
                return Ok(());
            }
        };
    }
}

fn open_wonderland_home<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let retry_ms = config.startup.wonderland_home_retry_ms.max(100);
    let attempts = capped_attempts(
        config.startup.wonderland_home_retries,
        config.startup.enter_wonderland_timeout_ms,
        retry_ms,
    );
    for attempt in 1..=attempts {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        workflow_actions::press_key_text("f6", &config.window)
            .context("进入千星流程按 F6 打开千星奇域失败")?;
        sleep(Duration::from_millis(retry_ms));
        if template_stable_visible(
            config,
            locator,
            &config.startup.templates.wonderland_close,
            config.startup.wonderland_close_region,
            "千星奇域主页关闭按钮",
            should_continue,
        )? {
            log::info!(
                "进入千星流程: 已打开千星奇域主页 attempt={}/{}",
                attempt,
                attempts
            );
            return Ok(());
        }
        log::info!(
            "进入千星流程: 未检测到千星奇域主页 attempt={}/{}",
            attempt,
            attempts
        );
    }
    bail!("等待千星奇域主页出现超时")
}

fn click_wonderland_card<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let prompt_locator = startup_transition_locator(config);
    let prompt_timeout_ms = config.startup.wonderland_card_retry_ms.max(100);
    let attempts = capped_attempts(
        config.startup.wonderland_card_retries,
        config.startup.enter_wonderland_timeout_ms,
        prompt_timeout_ms,
    );
    for attempt in 1..=attempts {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        log::info!(
            "进入千星流程: 点击千星奇域卡片 {},{} attempt={}",
            config.startup.wonderland_card_point.x,
            config.startup.wonderland_card_point.y,
            attempt
        );
        locator.click_point(Point::new(
            config.startup.wonderland_card_point.x,
            config.startup.wonderland_card_point.y,
        ))?;
        let confirm = workflow_actions::wait_or_click_template(
            &prompt_locator,
            &config.startup.templates.wonderland_enter_button,
            config.startup.wonderland_enter_button_region,
            config.startup.wonderland_enter_button_threshold,
            prompt_timeout_ms,
            HitAction::Click {
                offset: PointConfig::new(0, 0),
            },
            || should_continue(),
        )?;
        if let Some(confirm) = confirm {
            log::info!(
                "进入千星流程: 已检测到前往大厅按钮，点击一次 {},{} score={:.3} attempt={}/{}",
                confirm.center().x,
                confirm.center().y,
                confirm.score,
                attempt,
                attempts
            );
            return Ok(());
        }
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        log::info!(
            "进入千星流程: 未检测到前往大厅按钮 attempt={}/{}",
            attempt,
            attempts
        );
    }
    bail!("等待千星奇域大厅确认按钮超时")
}

fn wait_enter_confirm_gone<F>(config: &AppConfig, should_continue: &mut F) -> Result<()>
where
    F: FnMut() -> bool,
{
    let locator = startup_transition_locator(config);
    workflow_actions::locate_template(
        &locator,
        &config.startup.templates.wonderland_enter_button,
        config.startup.wonderland_enter_button_region,
        config.startup.wonderland_enter_button_threshold,
        config.startup.wonderland_confirm_absent_timeout_ms,
        TemplateMode::Absent {
            stability: Some(PixelStability {
                timeout_ms: config.startup.wonderland_confirm_stable_timeout_ms,
                mean_threshold: config.startup.stable_mean_threshold,
                changed_ratio_threshold: config.startup.stable_changed_ratio_threshold,
            }),
        },
        should_continue,
    )
    .context("点击前往大厅按钮后等待模板消失或区域稳定失败")?;
    log::info!("进入千星流程: 前往大厅按钮已消失且区域稳定，判定已进入千星");
    Ok(())
}

fn template_on_frame(
    config: &AppConfig,
    image: &image::DynamicImage,
    template: &PathBuf,
    region: RectConfig,
) -> Result<Option<TemplateHit>> {
    best_template_hit(
        image,
        Some(region.into()),
        template,
        config.startup.template_threshold,
    )
}

fn template_stable_visible<F>(
    config: &AppConfig,
    locator: &UiLocator,
    template: &PathBuf,
    region: RectConfig,
    label: &str,
    should_continue: &mut F,
) -> Result<bool>
where
    F: FnMut() -> bool,
{
    let mut stable_hits = 0_u32;
    while stable_hits < STARTUP_TEMPLATE_STABLE_HITS {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        if let Some(hit) =
            locate_template(config, locator, template, region, label, should_continue)?
        {
            stable_hits += 1;
            log::info!(
                "进入千星流程: 检测到模板 {} {},{} score={:.3} stable={}/{}",
                label,
                hit.center().x,
                hit.center().y,
                hit.score,
                stable_hits,
                STARTUP_TEMPLATE_STABLE_HITS
            );
        } else if stable_hits > 0 {
            log::info!("进入千星流程: 模板 {} 稳定命中中断，重新等待", label);
            return Ok(false);
        }
        sleep(Duration::from_millis(config.startup.poll_ms));
    }
    Ok(true)
}

fn locate_template<F>(
    config: &AppConfig,
    locator: &UiLocator,
    template: &PathBuf,
    region: RectConfig,
    label: &str,
    should_continue: &mut F,
) -> Result<Option<TemplateHit>>
where
    F: FnMut() -> bool,
{
    if !should_continue() {
        bail!("进入千星流程已取消");
    }
    let frame = locator.capture()?;
    let hit = template_on_frame(config, &frame.image, template, region)?;
    if hit.is_none() {
        log::debug!("进入千星流程: 未检测到模板 {}", label);
    }
    Ok(hit)
}

fn capped_attempts(configured_retries: u32, timeout_ms: u64, interval_ms: u64) -> u32 {
    configured_retries
        .max(1)
        .min(attempt_count(timeout_ms, interval_ms))
}

fn attempt_count(timeout_ms: u64, interval_ms: u64) -> u32 {
    let interval_ms = interval_ms.max(1);
    ((timeout_ms.max(interval_ms) + interval_ms - 1) / interval_ms) as u32
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
