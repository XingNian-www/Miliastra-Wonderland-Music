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

use super::ui_locator::{UiLocator, startup_locator};
use super::window;
use super::workflow_actions;
use crate::config::AppConfig;

const ENTER_GAME_OCR_TEXT: &str = "点击进入";

#[derive(Clone, Copy, Debug)]
enum GameStartupStep {
    EnsureGameWindow,
    FocusGameWindow,
    ClickEnterGameText,
    WaitEnterGameTextGone,
    WaitPaimonMenuTemplate,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnterGameEntryResult {
    TextClicked,
    PrimaryUiDetected,
}

impl GameStartupStep {
    fn label(self) -> &'static str {
        match self {
            Self::EnsureGameWindow => "确认游戏窗口",
            Self::FocusGameWindow => "聚焦游戏窗口",
            Self::ClickEnterGameText => "点击进入入口文字",
            Self::WaitEnterGameTextGone => "等待进入入口文字消失",
            Self::WaitPaimonMenuTemplate => "等待派蒙菜单模板",
            Self::Done => "完成",
        }
    }
}

pub(super) fn start_game<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    mut should_continue: F,
    mut on_window_detection_reset: impl FnMut(&'static str),
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    log::info!("启动游戏流程: 启动游戏并进入游戏");
    execute_game_startup_steps(
        config,
        engine,
        &mut should_continue,
        &mut on_window_detection_reset,
    )?;
    log::info!("启动游戏流程: 已完成，耗时 {}ms", elapsed_ms(started));
    Ok(())
}

fn execute_game_startup_steps<F, W>(
    config: &AppConfig,
    engine: &OcrEngine,
    should_continue: &mut F,
    on_window_detection_reset: &mut W,
) -> Result<()>
where
    F: FnMut() -> bool,
    W: FnMut(&'static str),
{
    let locator = startup_locator(config);
    let mut step = GameStartupStep::EnsureGameWindow;
    let mut enter_game_deadline = None;

    loop {
        log::info!("启动游戏流程步骤: {}", step.label());
        step = match step {
            GameStartupStep::EnsureGameWindow => {
                ensure_game_window(config, should_continue, on_window_detection_reset)?;
                GameStartupStep::FocusGameWindow
            }
            GameStartupStep::FocusGameWindow => {
                workflow_actions::focus(&config.window, config.timing.input.after_activate_ms)
                    .context("启动游戏流程聚焦游戏窗口失败")?;
                on_window_detection_reset("启动游戏流程已聚焦游戏窗口");
                if config.startup.enter_game {
                    GameStartupStep::ClickEnterGameText
                } else {
                    log::info!("启动游戏流程: startup.enter_game=false，跳过进入游戏");
                    GameStartupStep::Done
                }
            }
            GameStartupStep::ClickEnterGameText => {
                let deadline = *enter_game_deadline.get_or_insert_with(|| {
                    Instant::now() + Duration::from_millis(config.startup.enter_game_timeout_ms)
                });
                match click_enter_game_text_once(
                    config,
                    engine,
                    &locator,
                    should_continue,
                    deadline,
                )? {
                    EnterGameEntryResult::TextClicked => GameStartupStep::WaitEnterGameTextGone,
                    EnterGameEntryResult::PrimaryUiDetected => {
                        on_window_detection_reset("启动游戏流程已检测到一级界面");
                        GameStartupStep::Done
                    }
                }
            }
            GameStartupStep::WaitEnterGameTextGone => {
                let deadline = enter_game_deadline.expect("进入游戏文字点击步骤应该先设置超时时间");
                wait_enter_game_text_gone(config, engine, &locator, should_continue, deadline)?;
                GameStartupStep::WaitPaimonMenuTemplate
            }
            GameStartupStep::WaitPaimonMenuTemplate => {
                wait_paimon_menu_template(config, &locator, should_continue)?;
                on_window_detection_reset("启动游戏流程进入游戏完成");
                GameStartupStep::Done
            }
            GameStartupStep::Done => return Ok(()),
        };
    }
}

fn ensure_game_window<F, W>(
    config: &AppConfig,
    should_continue: &mut F,
    on_window_detection_reset: &mut W,
) -> Result<()>
where
    F: FnMut() -> bool,
    W: FnMut(&'static str),
{
    if window::GameWindow::find(&config.window).is_ok() {
        log::info!("启动游戏流程: 已找到游戏窗口，跳过启动游戏");
        on_window_detection_reset("启动游戏流程发现已有游戏窗口");
        return Ok(());
    }
    if !config.startup.launch_game {
        return Err(window::target_window_unavailable(
            "未找到游戏窗口，且 startup.launch_game=false，已中止启动游戏流程",
        ));
    }

    let game_path = resolve_game_path(config)?;
    if !game_path.exists() {
        bail!("游戏启动路径不存在: {}", game_path.display());
    }

    let launch_exe = game_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| game_path.display().to_string());
    log::info!(
        "启动游戏流程: 启动游戏 {} target_process={}",
        game_path.display(),
        config.window.target_process
    );
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
    log::info!(
        "启动游戏流程: 已创建游戏进程 command_exe={} target_process={}",
        launch_exe,
        config.window.target_process
    );
    on_window_detection_reset("启动游戏流程已创建游戏进程");

    for attempt in 1..=config.startup.launch_retries.max(1) {
        if !should_continue() {
            bail!("启动游戏流程已取消");
        }
        sleep(Duration::from_millis(config.startup.launch_wait_ms));
        if window::GameWindow::find(&config.window).is_ok() {
            log::info!("启动游戏流程: 游戏窗口已出现 attempt={}", attempt);
            on_window_detection_reset("启动游戏流程检测到游戏窗口");
            return Ok(());
        }
        log::info!(
            "启动游戏流程: 等待游戏窗口出现 attempt={}/{}",
            attempt,
            config.startup.launch_retries
        );
    }

    Err(window::target_window_unavailable(format!(
        "启动游戏后仍未找到目标窗口进程: {}",
        config.window.target_process
    )))
}

fn click_enter_game_text_once<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    locator: &UiLocator,
    should_continue: &mut F,
    deadline: Instant,
) -> Result<EnterGameEntryResult>
where
    F: FnMut() -> bool,
{
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动游戏流程已取消");
        }
        if paimon_menu_template_visible(config, locator)? {
            log::info!(
                "启动游戏流程: 已检测到派蒙菜单模板，跳过点击 {} 并进入游戏完成",
                ENTER_GAME_OCR_TEXT
            );
            return Ok(EnterGameEntryResult::PrimaryUiDetected);
        }
        let region = locator.region(config.startup.enter_game_text_region.into());
        if let Some(hit) = region.find_text(engine, ENTER_GAME_OCR_TEXT)? {
            let point = hit.center();
            log::info!(
                "启动游戏流程: 点击 {} OCR 文本 {},{}",
                ENTER_GAME_OCR_TEXT,
                point.x,
                point.y
            );
            locator.click_point(point)?;
            return Ok(EnterGameEntryResult::TextClicked);
        }
        sleep(Duration::from_millis(config.startup.poll_ms));
    }
    bail!(
        "{}ms 内未识别到 {} 文字",
        config.startup.enter_game_timeout_ms,
        ENTER_GAME_OCR_TEXT
    )
}

fn paimon_menu_template_visible(config: &AppConfig, locator: &UiLocator) -> Result<bool> {
    Ok(locator
        .region(config.startup.main_ui_region.into())
        .find_template_with_threshold(
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
        )?
        .is_some())
}

fn wait_enter_game_text_gone<F>(
    config: &AppConfig,
    engine: &OcrEngine,
    locator: &UiLocator,
    should_continue: &mut F,
    deadline: Instant,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动游戏流程已取消");
        }
        let region = locator.region(config.startup.enter_game_text_region.into());
        if let Some(hit) = region.find_text(engine, ENTER_GAME_OCR_TEXT)? {
            let point = hit.center();
            log::info!(
                "启动游戏流程: {} 文字仍存在，继续点击 OCR 文本 {},{}",
                ENTER_GAME_OCR_TEXT,
                point.x,
                point.y
            );
            locator.click_point(point)?;
            sleep(Duration::from_millis(config.startup.poll_ms));
            continue;
        }
        log::info!(
            "启动游戏流程: {} 文字已消失，开始等待派蒙菜单模板",
            ENTER_GAME_OCR_TEXT
        );
        return Ok(());
    }
    bail!("等待 {} 文字消失超时", ENTER_GAME_OCR_TEXT)
}

fn wait_paimon_menu_template<F>(
    config: &AppConfig,
    locator: &UiLocator,
    should_continue: &mut F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(config.startup.final_primary_timeout_ms);
    while Instant::now() < deadline {
        if !should_continue() {
            bail!("启动游戏流程已取消");
        }
        if paimon_menu_template_visible(config, locator)? {
            log::info!("启动游戏流程: 已检测到派蒙菜单模板，进入游戏完成");
            return Ok(());
        }
        sleep(Duration::from_millis(locator.poll_ms()));
    }
    bail!("等待派蒙菜单模板超时")
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
    let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
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
