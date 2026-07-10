use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::config::{AppConfig, RectConfig};
use super::geometry::Point;
use super::template_match::{TemplateHit, best_template_hit};
use super::ui_locator::{UiLocator, startup_locator};
use super::window;
use super::workflow_actions;

const STARTUP_TEMPLATE_STABLE_HITS: u32 = 2;
const ENTERED_WONDERLAND_CONFIRM_TIMEOUT_MS: u64 = 20_000;
const ENTERED_WONDERLAND_CONFIRM_REGION: RectConfig = RectConfig {
    x: 1100,
    y: 900,
    width: 100,
    height: 100,
};

#[derive(Clone, Copy, Debug)]
enum WonderlandStep {
    OpenWonderlandHome,
    ClickWonderlandCard,
    ConfirmEnter,
    WaitConfirmGone,
    WaitEnteredWonderlandConfirm,
}

impl WonderlandStep {
    fn label(self) -> &'static str {
        match self {
            Self::OpenWonderlandHome => "打开千星奇域主页",
            Self::ClickWonderlandCard => "点击千星奇域卡片",
            Self::ConfirmEnter => "确认进入大厅",
            Self::WaitConfirmGone => "等待确认弹窗消失",
            Self::WaitEnteredWonderlandConfirm => "等待千星内确认按钮",
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
                WonderlandStep::ConfirmEnter
            }
            WonderlandStep::ConfirmEnter => {
                click_enter_confirm(config, locator, should_continue)?;
                WonderlandStep::WaitConfirmGone
            }
            WonderlandStep::WaitConfirmGone => {
                wait_enter_confirm_gone(config, locator, should_continue)?;
                WonderlandStep::WaitEnteredWonderlandConfirm
            }
            WonderlandStep::WaitEnteredWonderlandConfirm => {
                wait_entered_wonderland_confirm_reappeared(config, locator, should_continue)?;
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
    for attempt in 1..=attempt_count(
        config.startup.enter_wonderland_timeout_ms,
        config.startup.f6_retry_ms,
    ) {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        workflow_actions::press_key_text("f6", &config.window)
            .context("进入千星流程按 F6 打开千星奇域失败")?;
        sleep(Duration::from_millis(config.startup.f6_retry_ms));
        if template_stable_visible(
            config,
            locator,
            &config.startup.templates.wonderland_close,
            config.startup.wonderland_close_region,
            "千星奇域主页关闭按钮",
            should_continue,
        )? {
            log::info!("进入千星流程: 已打开千星奇域主页 attempt={}", attempt);
            return Ok(());
        }
        log::info!("进入千星流程: 未检测到千星奇域主页 attempt={}", attempt);
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
    for attempt in 1..=attempt_count(
        config.startup.enter_wonderland_timeout_ms,
        config.startup.poll_ms.max(800),
    ) {
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
        sleep(Duration::from_millis(config.startup.poll_ms.max(800)));
        if template_stable_visible(
            config,
            locator,
            &config.startup.templates.confirm_black,
            config.startup.prompt_confirm_text_region,
            "千星大厅黑色确认按钮",
            should_continue,
        )? {
            log::info!("进入千星流程: 已检测到千星大厅黑色确认按钮");
            return Ok(());
        }
    }
    bail!("等待千星奇域大厅确认按钮超时")
}

fn click_enter_confirm<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let Some(confirm) = locate_template(
        config,
        locator,
        &config.startup.templates.confirm_black,
        config.startup.prompt_confirm_text_region,
        "千星大厅黑色确认按钮",
        should_continue,
    )?
    else {
        bail!("确认进入大厅时未找到千星确认按钮");
    };
    log::info!(
        "进入千星流程: 点击千星确认按钮 {},{} score={:.3}",
        confirm.center().x,
        confirm.center().y,
        confirm.score
    );
    locator.click_point(confirm.center())?;
    Ok(())
}

fn wait_enter_confirm_gone<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.stable_timeout_ms.max(5000));
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        if locate_template(
            config,
            locator,
            &config.startup.templates.confirm_black,
            config.startup.prompt_confirm_text_region,
            "千星确认按钮",
            should_continue,
        )?
        .is_none()
        {
            log::info!("进入千星流程: 千星确认按钮已消失");
            workflow_actions::wait(1000);
            return Ok(());
        }
        sleep(Duration::from_millis(config.startup.poll_ms.max(1000)));
    }
    bail!("千星确认按钮点击后未消失")
}

fn wait_entered_wonderland_confirm_reappeared<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let timeout_ms = config
        .startup
        .entered_wonderland_confirm_timeout_ms
        .unwrap_or(ENTERED_WONDERLAND_CONFIRM_TIMEOUT_MS);
    let region = config
        .startup
        .entered_wonderland_confirm_region
        .unwrap_or(ENTERED_WONDERLAND_CONFIRM_REGION);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("进入千星流程已取消");
        }
        if let Some(hit) = locate_template(
            config,
            locator,
            &config.startup.templates.confirm_black,
            region,
            "千星内黑色确认按钮",
            should_continue,
        )? {
            log::info!(
                "进入千星流程: 已检测到千星内确认按钮 {},{} score={:.3}",
                hit.center().x,
                hit.center().y,
                hit.score
            );
            return Ok(());
        }
        sleep(Duration::from_millis(locator.poll_ms()));
    }
    bail!("等待千星内确认按钮再次出现超时: {}ms", timeout_ms)
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

fn attempt_count(timeout_ms: u64, interval_ms: u64) -> u32 {
    let interval_ms = interval_ms.max(1);
    ((timeout_ms.max(interval_ms) + interval_ms - 1) / interval_ms) as u32
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
