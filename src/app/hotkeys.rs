use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_HOTKEY,
};

use super::config::HotkeyConfig;

const PAUSE_ID: i32 = 1;
const EXIT_ID: i32 = 2;

pub fn start(
    config: &HotkeyConfig,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let pause_key = parse_virtual_key(&config.pause_key)?;
    let exit_key = parse_virtual_key(&config.exit_key)?;
    log::info!(
        "全局热键已启用: 暂停={} 退出={}",
        config.pause_key,
        config.exit_key
    );
    thread::spawn(move || hotkey_loop(pause_key, exit_key, running, paused));
    Ok(())
}

fn hotkey_loop(
    pause_key: VIRTUAL_KEY,
    exit_key: VIRTUAL_KEY,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) {
    unsafe {
        if let Err(error) = RegisterHotKey(None, PAUSE_ID, MOD_NOREPEAT, pause_key.0 as u32) {
            log::error!("注册暂停热键失败: {error:#}");
            return;
        }
        if let Err(error) = RegisterHotKey(None, EXIT_ID, MOD_NOREPEAT, exit_key.0 as u32) {
            log::error!("注册退出热键失败: {error:#}");
            let _ = UnregisterHotKey(None, PAUSE_ID);
            return;
        }

        let mut message = MSG::default();
        while running.load(Ordering::SeqCst)
            && GetMessageW(&mut message, Some(HWND(std::ptr::null_mut())), 0, 0).as_bool()
        {
            if message.message == WM_HOTKEY {
                match message.wParam.0 as i32 {
                    PAUSE_ID => {
                        let now_paused = !paused.load(Ordering::SeqCst);
                        paused.store(now_paused, Ordering::SeqCst);
                        log::info!("脚本{}", if now_paused { "已暂停" } else { "已恢复" });
                    }
                    EXIT_ID => {
                        log::info!("收到退出热键");
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = UnregisterHotKey(None, PAUSE_ID);
        let _ = UnregisterHotKey(None, EXIT_ID);
    }
}

fn parse_virtual_key(value: &str) -> Result<VIRTUAL_KEY> {
    let normalized = value.trim().to_ascii_uppercase();
    let key = match normalized.as_str() {
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        single if single.len() == 1 => single.as_bytes()[0] as u16,
        _ => bail!("unsupported hotkey: {}", value),
    };
    Ok(VIRTUAL_KEY(key))
}
