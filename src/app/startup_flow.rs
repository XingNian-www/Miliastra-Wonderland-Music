use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ocr_rs::OcrEngine;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::w;

use super::FrameArgs;
use super::config::{AppConfig, RectConfig};
use super::frame_source::Canvas;
use super::geometry::Point;
use super::template_match::best_template_hit;
use super::ui_locator::UiLocator;
use super::window;
use super::workflow_actions::{self, PixelStability};

pub(super) fn start_game_and_enter_wonderland<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    mut should_continue: F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    log::info!("启动流程: 自动启动游戏并进入千星奇域");
    ensure_game_window(config, &mut should_continue)?;
    workflow_actions::focus(&config.window, config.timing.input.after_activate_ms)
        .context("启动流程聚焦游戏窗口失败")?;

    let locator = startup_locator(config);
    if config.startup.enter_wonderland {
        open_wonderland_home(config, engine, &locator, &mut should_continue)?;
        enter_wonderland_lobby(config, engine, &locator, &mut should_continue)?;
    } else if config.startup.enter_game {
        open_game_gate_only(config, engine, &locator, &mut should_continue)?;
    }

    log::info!("启动流程: 已完成，耗时 {}ms", elapsed_ms(started));
    Ok(())
}

fn ensure_game_window<F>(config: &AppConfig, should_continue: &mut F) -> Result<()>
where
    F: FnMut() -> bool,
{
    if window::GameWindow::find(&config.window).is_ok() {
        log::info!("启动流程: 已找到游戏窗口，跳过启动游戏");
        return Ok(());
    }
    if !config.startup.launch_game {
        bail!("未找到游戏窗口，且 startup.launch_game=false，已中止启动流程");
    }

    let game_path = resolve_game_path(config)?;
    if !game_path.exists() {
        bail!("游戏启动路径不存在: {}", game_path.display());
    }

    log::info!("启动流程: 启动游戏 {}", game_path.display());
    let mut command = ProcessCommand::new(&game_path);
    if let Some(parent) = game_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        command.current_dir(parent);
    }
    for arg in split_command_args(&config.startup.game_args)? {
        command.arg(arg);
    }
    command
        .spawn()
        .with_context(|| format!("启动游戏失败: {}", game_path.display()))?;

    for attempt in 1..=config.startup.launch_retries.max(1) {
        if !should_continue() {
            bail!("启动流程已取消");
        }
        sleep(Duration::from_millis(config.startup.launch_wait_ms));
        if window::GameWindow::find(&config.window).is_ok() {
            log::info!("启动流程: 游戏窗口已出现 attempt={}", attempt);
            return Ok(());
        }
        log::info!(
            "启动流程: 等待游戏窗口出现 attempt={}/{}",
            attempt,
            config.startup.launch_retries
        );
    }

    bail!(
        "启动游戏后仍未找到目标窗口进程: {}",
        config.window.target_process
    )
}

fn open_wonderland_home<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.enter_wonderland_timeout_ms);
    let mut last_f6_at = Instant::now()
        .checked_sub(Duration::from_millis(config.startup.f6_retry_ms))
        .unwrap_or_else(Instant::now);

    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动流程已取消");
        }
        if find_any_text(
            locator,
            engine,
            &config.startup.wonderland_home_texts,
            config.startup.wonderland_home_text_region,
        )?
        .is_some()
        {
            log::info!("启动流程: 已检测到千星奇域界面");
            return Ok(());
        }
        if click_any_text(
            locator,
            engine,
            &config.startup.prompt_confirm_texts,
            config.startup.prompt_confirm_text_region,
            "启动弹窗确认",
        )? {
            wait_region_stable(
                config,
                locator,
                config.startup.prompt_confirm_text_region,
                should_continue,
            )?;
            continue;
        }
        if config.startup.enter_game
            && click_any_text(
                locator,
                engine,
                &config.startup.enter_game_texts,
                config.startup.enter_game_text_region,
                "进入游戏",
            )?
        {
            wait_region_stable(
                config,
                locator,
                config.startup.enter_game_text_region,
                should_continue,
            )?;
            continue;
        }
        if last_f6_at.elapsed() >= Duration::from_millis(config.startup.f6_retry_ms) {
            workflow_actions::press_key_text("f6", &config.window)
                .context("启动流程按 F6 打开千星奇域失败")?;
            last_f6_at = Instant::now();
        }
        sleep(Duration::from_millis(config.startup.poll_ms));
    }

    bail!("等待千星奇域界面出现超时");
}

fn enter_wonderland_lobby<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    log::info!(
        "启动流程: 点击千星奇域卡片 {},{}",
        config.startup.wonderland_card_point.x,
        config.startup.wonderland_card_point.y
    );
    locator.click_point(Point::new(
        config.startup.wonderland_card_point.x,
        config.startup.wonderland_card_point.y,
    ))?;
    wait_region_stable(
        config,
        locator,
        config.startup.wonderland_home_text_region,
        &mut *should_continue,
    )?;

    let deadline =
        Instant::now() + Duration::from_millis(config.startup.enter_wonderland_timeout_ms);
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动流程已取消");
        }
        if click_any_text(
            locator,
            engine,
            &config.startup.wonderland_enter_texts,
            config.startup.wonderland_enter_text_region,
            "前往千星奇域大厅",
        )? {
            wait_region_stable(
                config,
                locator,
                config.startup.wonderland_enter_text_region,
                should_continue,
            )?;
            wait_primary_ui(config, locator, should_continue)?;
            log::info!("启动流程: 已进入千星奇域大厅，不执行返回提瓦特");
            return Ok(());
        }
        sleep(Duration::from_millis(config.startup.poll_ms));
    }

    bail!("等待千星奇域大厅确认按钮超时");
}

fn open_game_gate_only<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(config.startup.enter_game_timeout_ms);
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动流程已取消");
        }
        if wait_primary_ui_once(config, locator)? {
            return Ok(());
        }
        let clicked_confirm = click_any_text(
            locator,
            engine,
            &config.startup.prompt_confirm_texts,
            config.startup.prompt_confirm_text_region,
            "启动弹窗确认",
        )?;
        let clicked_enter = click_any_text(
            locator,
            engine,
            &config.startup.enter_game_texts,
            config.startup.enter_game_text_region,
            "进入游戏",
        )?;
        if clicked_confirm || clicked_enter {
            wait_region_stable(
                config,
                locator,
                config.startup.enter_game_text_region,
                should_continue,
            )?;
        }
        sleep(Duration::from_millis(config.startup.poll_ms));
    }
    bail!("等待进入游戏完成超时");
}

fn wait_primary_ui<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let hit = locator
        .region(config.screen.enter_rect.into())
        .wait_template_while(
            &config.templates.enter,
            config.templates.marker_threshold,
            config.startup.final_primary_timeout_ms,
            should_continue,
        )?;
    if hit.is_some() {
        Ok(())
    } else {
        bail!("等待千星奇域大厅主界面超时")
    }
}

fn wait_primary_ui_once(config: &AppConfig, locator: &UiLocator) -> Result<bool> {
    let frame = locator.capture()?;
    Ok(best_template_hit(
        &frame.image,
        Some(config.screen.enter_rect.into()),
        &config.templates.enter,
        config.templates.marker_threshold,
    )?
    .is_some())
}

fn wait_region_stable<F>(
    config: &AppConfig,
    locator: &UiLocator,
    region: RectConfig,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let result = workflow_actions::wait_pixels_stable(
        locator,
        region,
        PixelStability {
            timeout_ms: config.startup.stable_timeout_ms,
            mean_threshold: config.startup.stable_mean_threshold,
            changed_ratio_threshold: config.startup.stable_changed_ratio_threshold,
        },
        &mut *should_continue,
    );
    match result {
        Ok(()) => Ok(()),
        Err(error) if should_continue() => {
            log::warn!("启动流程: 等待像素稳定未完成，继续后续检测: {error:#}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn click_any_text(
    locator: &UiLocator,
    engine: &OcrEngine,
    texts: &[String],
    region: RectConfig,
    label: &str,
) -> Result<bool> {
    let Some(point) = find_any_text(locator, engine, texts, region)? else {
        return Ok(false);
    };
    log::info!("启动流程: 点击 OCR 文本 {} {},{}", label, point.x, point.y);
    locator.click_point(point)?;
    Ok(true)
}

fn find_any_text(
    locator: &UiLocator,
    engine: &OcrEngine,
    texts: &[String],
    region: RectConfig,
) -> Result<Option<Point>> {
    let region = locator.region(region.into());
    let labels = texts
        .iter()
        .map(String::as_str)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    Ok(region
        .find_any_text(engine, &labels)?
        .map(|hit| hit.center()))
}

fn startup_locator(config: &AppConfig) -> UiLocator {
    UiLocator::new(
        Canvas {
            width: config.screen.expected_width,
            height: config.screen.expected_height,
            resize: true,
        },
        FrameArgs { image: None },
        config.window.clone(),
        config.startup.poll_ms,
    )
}

fn resolve_game_path(config: &AppConfig) -> Result<PathBuf> {
    let exe_path = &config.startup.exe_path;
    if !exe_path.as_os_str().is_empty() {
        if exe_path.is_dir() {
            return resolve_game_path_from_dir(exe_path, &config.window.target_process);
        }
        return Ok(exe_path.clone());
    }
    if let Some(path) = registry_game_path() {
        return Ok(path);
    }
    Err(anyhow!(
        "未配置 startup.exe_path，且未能从米哈游启动器注册表找到官服/国际服安装路径"
    ))
}

fn resolve_game_path_from_dir(dir: &Path, target_process: &str) -> Result<PathBuf> {
    for candidate in startup_exe_candidates(target_process) {
        let path = dir.join(&candidate);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "启动 EXE 所在目录下未找到目标进程对应的 exe: {}",
        dir.display()
    )
}

fn startup_exe_candidates(target_process: &str) -> Vec<String> {
    let mut candidates = target_process
        .split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            if item.to_ascii_lowercase().ends_with(".exe") {
                item.to_string()
            } else {
                format!("{item}.exe")
            }
        })
        .collect::<Vec<_>>();
    for fallback in ["YuanShen.exe", "GenshinImpact.exe"] {
        if !candidates
            .iter()
            .any(|item| item.eq_ignore_ascii_case(fallback))
        {
            candidates.push(fallback.to_string());
        }
    }
    candidates
}

fn registry_game_path() -> Option<PathBuf> {
    for (key, exe) in [
        (w!("Software\\miHoYo\\HYP\\1_1\\hk4e_cn"), "YuanShen.exe"),
        (
            w!("Software\\Cognosphere\\HYP\\1_0\\hk4e_global"),
            "GenshinImpact.exe",
        ),
    ] {
        let Some(dir) = registry_string(key, w!("GameInstallPath")) else {
            continue;
        };
        let path = Path::new(dir.trim()).join(exe);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn registry_string(key: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<String> {
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 || byte_len == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; (byte_len as usize + 1) / 2];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

fn split_command_args(value: &str) -> Result<Vec<String>> {
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
        bail!("startup.game_args 引号未闭合");
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_args_keeps_quoted_text() {
        let args =
            split_command_args(r#"-screen-fullscreen 0 "-window-title test""#).expect("split args");

        assert_eq!(
            args,
            vec![
                "-screen-fullscreen".to_string(),
                "0".to_string(),
                "-window-title test".to_string()
            ]
        );
    }

    #[test]
    fn split_command_args_rejects_unclosed_quote() {
        assert!(split_command_args(r#""abc"#).is_err());
    }

    #[test]
    fn startup_exe_candidates_adds_exe_suffix_and_fallbacks() {
        let candidates = startup_exe_candidates("yuanshen.exe, GenshinImpact");

        assert_eq!(candidates[0], "yuanshen.exe");
        assert_eq!(candidates[1], "GenshinImpact.exe");
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case("YuanShen.exe"))
        );
    }
}
