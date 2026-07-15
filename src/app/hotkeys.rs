use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_HOTKEY, WM_QUIT,
};

use crate::config::HotkeyConfig;

const PAUSE_ID: i32 = 1;
const EXIT_ID: i32 = 2;

pub(crate) struct HotkeyRuntime {
    thread_id: u32,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HotkeyRuntime {
    pub(crate) fn shutdown(mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if self.worker.is_some() {
            unsafe {
                if !PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).as_bool() {
                    log::warn!("发送热键线程退出消息失败");
                }
            }
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("热键线程 panic"))?;
        }
        Ok(())
    }
}

pub fn start(
    config: &HotkeyConfig,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> Result<HotkeyRuntime> {
    if !config.enabled {
        return Ok(HotkeyRuntime {
            thread_id: 0,
            running,
            worker: None,
        });
    }
    let pause_key = parse_virtual_key(&config.pause_key)?;
    let exit_key = parse_virtual_key(&config.exit_key)?;
    log::info!(
        "全局热键已启用: 暂停={} 退出={}",
        config.pause_key,
        config.exit_key
    );
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let worker_running = Arc::clone(&running);
    let worker = thread::spawn(move || {
        hotkey_loop(pause_key, exit_key, worker_running, paused, ready_sender)
    });
    let thread_id = ready_receiver
        .recv()
        .map_err(|_| anyhow::anyhow!("热键线程未能初始化消息队列"))?;
    Ok(HotkeyRuntime {
        thread_id,
        running,
        worker: Some(worker),
    })
}

fn hotkey_loop(
    pause_key: VIRTUAL_KEY,
    exit_key: VIRTUAL_KEY,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    ready: SyncSender<u32>,
) {
    unsafe {
        let thread_id = GetCurrentThreadId();
        let mut queue_probe = MSG::default();
        let _ = PeekMessageW(&mut queue_probe, None, 0, 0, PM_NOREMOVE);
        let _ = ready.send(thread_id);
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
