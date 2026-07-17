use std::fmt;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Mouse};
use image::DynamicImage;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, ClientToScreen, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, SetFocus, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GA_ROOT, GetAncestor, GetClientRect, GetForegroundWindow,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SW_RESTORE,
    SetForegroundWindow, ShowWindow, WM_CLOSE, WindowFromPoint,
};
use windows::core::BOOL;

use crate::config::{PointConfig, WindowConfig};

const DRAG_STEPS: i32 = 8;
const DRAG_STEP_MS: u64 = 5;

#[derive(Clone, Debug)]
pub struct TargetWindowUnavailable {
    message: String,
}

impl TargetWindowUnavailable {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TargetWindowUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TargetWindowUnavailable {}

pub fn target_window_unavailable(message: impl Into<String>) -> anyhow::Error {
    TargetWindowUnavailable::new(message).into()
}

#[derive(Clone, Copy, Debug)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug)]
pub struct GameWindow {
    hwnd: HWND,
    process_id: u32,
    client_left: i32,
    client_top: i32,
    client_width: i32,
    client_height: i32,
    content_width: i32,
    content_height: i32,
}

impl GameWindow {
    pub fn find(config: &WindowConfig) -> Result<Self> {
        let target = find_window_by_process(&config.target_process)?;
        let mut window = Self {
            hwnd: target.hwnd,
            process_id: target.process_id,
            client_left: 0,
            client_top: 0,
            client_width: 0,
            client_height: 0,
            content_width: config.content_width as i32,
            content_height: config.content_height as i32,
        };
        window.refresh_client_area(config)?;
        Ok(window)
    }

    pub fn screen_point(&self, point: PointConfig) -> ScreenPoint {
        let x = self.client_left + scale_i32(point.x, self.content_width, self.client_width);
        let y = self.client_top + scale_i32(point.y, self.content_height, self.client_height);
        ScreenPoint { x, y }
    }

    pub fn click(&mut self, enigo: &mut Enigo, point: PointConfig) -> Result<()> {
        self.click_focused(enigo, point)
    }

    pub fn click_focused(&mut self, enigo: &mut Enigo, point: PointConfig) -> Result<()> {
        self.refresh_client_area_for_click()?;
        let screen = self.screen_point(point);
        self.click_screen(enigo, screen)
    }

    pub fn scroll(
        &mut self,
        enigo: &mut Enigo,
        point: PointConfig,
        length: i32,
        axis: Axis,
    ) -> Result<()> {
        self.refresh_client_area_for_click()?;
        let screen = self.screen_point(point);
        self.ensure_point_targets_window(screen)?;
        enigo
            .move_mouse(screen.x, screen.y, Coordinate::Abs)
            .context("move mouse for scroll")?;
        enigo.scroll(length, axis).context("scroll mouse")
    }

    pub fn drag(&mut self, enigo: &mut Enigo, from: PointConfig, to: PointConfig) -> Result<()> {
        self.refresh_client_area_for_click()?;
        let from = self.screen_point(from);
        let to = self.screen_point(to);
        self.ensure_point_targets_window(from)?;
        self.ensure_point_targets_window(to)?;
        enigo
            .move_mouse(from.x, from.y, Coordinate::Abs)
            .context("move mouse to drag start")?;
        enigo
            .button(Button::Left, Direction::Press)
            .context("press mouse for drag")?;

        let drag_result = (|| -> Result<()> {
            for step in 1..=DRAG_STEPS {
                let x = from.x + (to.x - from.x) * step / DRAG_STEPS;
                let y = from.y + (to.y - from.y) * step / DRAG_STEPS;
                enigo
                    .move_mouse(x, y, Coordinate::Abs)
                    .context("move pressed mouse during drag")?;
                sleep(Duration::from_millis(DRAG_STEP_MS));
            }
            Ok(())
        })();
        let release_result = enigo
            .button(Button::Left, Direction::Release)
            .context("release mouse after drag");
        drag_result?;
        release_result
    }

    pub fn activate(&mut self, after_activate_ms: u64) -> Result<()> {
        let started = Instant::now();
        if self.is_foreground_process() {
            let result = self.ensure_foreground();
            log::info!(target: "timing",
                "激活游戏窗口耗时: {}ms already_foreground=true",
                elapsed_ms(started)
            );
            return result;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
        }
        send_alt_keypress();
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
        if !self.is_foreground_window() {
            unsafe {
                let _ = BringWindowToTop(self.hwnd);
            }
        }
        if !self.is_foreground_window() {
            self.activate_with_attached_input();
        }
        if after_activate_ms > 0 {
            sleep(Duration::from_millis(after_activate_ms));
        }
        let result = self.ensure_foreground();
        log::info!(target: "timing",
            "激活游戏窗口耗时: {}ms success={}",
            elapsed_ms(started),
            result.is_ok()
        );
        result
    }

    pub fn is_foreground(&self) -> bool {
        self.is_foreground_process()
    }

    pub fn focus_game(&mut self, enigo: &mut Enigo, point: PointConfig) -> Result<()> {
        let started = Instant::now();
        self.click_focused(enigo, point)?;
        let result = self.ensure_foreground();
        log::info!(target: "timing",
            "聚焦游戏耗时: {}ms success={}",
            elapsed_ms(started),
            result.is_ok()
        );
        result
    }

    pub fn ensure_foreground(&self) -> Result<()> {
        if self.is_foreground_process() {
            Ok(())
        } else {
            Err(target_window_unavailable(
                "当前前台窗口不是目标游戏进程，已中止输入",
            ))
        }
    }

    fn click_screen(&self, enigo: &mut Enigo, screen: ScreenPoint) -> Result<()> {
        let started = Instant::now();
        self.ensure_point_targets_window(screen)?;
        let check_ms = elapsed_ms(started);
        let input_started = Instant::now();
        enigo
            .move_mouse(screen.x, screen.y, Coordinate::Abs)
            .context("move mouse")?;
        enigo
            .button(Button::Left, Direction::Click)
            .context("click mouse")?;
        log::info!(target: "timing",
            "点击输入耗时: total={}ms check={}ms input={}ms x={} y={}",
            elapsed_ms(started),
            check_ms,
            elapsed_ms(input_started),
            screen.x,
            screen.y
        );
        Ok(())
    }

    fn refresh_client_area_for_click(&mut self) -> Result<()> {
        self.refresh_client_area_values()
    }

    fn refresh_client_area(&mut self, config: &WindowConfig) -> Result<()> {
        self.refresh_client_area_values()?;
        if self.client_width != config.content_width as i32
            || self.client_height != config.content_height as i32
        {
            log::warn!(
                "目标窗口客户区为 {}x{}，配置有效内容为 {}x{}，点击会按比例换算",
                self.client_width,
                self.client_height,
                config.content_width,
                config.content_height
            );
        }
        Ok(())
    }

    fn refresh_client_area_values(&mut self) -> Result<()> {
        let mut client_rect = RECT::default();
        if let Err(error) = unsafe { GetClientRect(self.hwnd, &mut client_rect) } {
            return Err(target_window_unavailable(format!(
                "GetClientRect failed: {error}"
            )));
        }

        let mut top_left = POINT { x: 0, y: 0 };
        if !unsafe { ClientToScreen(self.hwnd, &mut top_left).as_bool() } {
            return Err(target_window_unavailable("ClientToScreen failed"));
        }

        let client_width = client_rect.right - client_rect.left;
        let client_height = client_rect.bottom - client_rect.top;
        if client_width <= 0 || client_height <= 0 {
            return Err(target_window_unavailable(format!(
                "目标窗口客户区尺寸无效: {}x{}",
                client_width, client_height
            )));
        }

        self.client_left = top_left.x;
        self.client_top = top_left.y;
        self.client_width = client_width;
        self.client_height = client_height;
        Ok(())
    }

    fn is_foreground_window(&self) -> bool {
        unsafe { GetForegroundWindow() == self.hwnd }
    }

    fn is_foreground_process(&self) -> bool {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return false;
        }
        let mut process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut process_id)) };
        process_id != 0 && process_id == self.process_id
    }

    fn activate_with_attached_input(&self) {
        let foreground = unsafe { GetForegroundWindow() };
        let current_thread = unsafe { GetCurrentThreadId() };
        let target_thread = unsafe { GetWindowThreadProcessId(self.hwnd, None) };
        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(foreground, None) }
        };

        let attached_foreground = foreground_thread != 0
            && foreground_thread != current_thread
            && unsafe { AttachThreadInput(current_thread, foreground_thread, true).as_bool() };
        let attached_target = target_thread != 0
            && target_thread != current_thread
            && target_thread != foreground_thread
            && unsafe { AttachThreadInput(current_thread, target_thread, true).as_bool() };

        unsafe {
            let _ = BringWindowToTop(self.hwnd);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.hwnd));
        }

        if attached_target {
            unsafe {
                let _ = AttachThreadInput(current_thread, target_thread, false);
            }
        }
        if attached_foreground {
            unsafe {
                let _ = AttachThreadInput(current_thread, foreground_thread, false);
            }
        }
    }

    fn ensure_point_targets_window(&self, point: ScreenPoint) -> Result<()> {
        let target = unsafe {
            WindowFromPoint(POINT {
                x: point.x,
                y: point.y,
            })
        };
        if target.is_invalid() {
            return Err(target_window_unavailable(format!(
                "目标点击点不在任何窗口内: {},{}",
                point.x, point.y
            )));
        }
        let root = unsafe { GetAncestor(target, GA_ROOT) };
        if root != self.hwnd {
            return Err(target_window_unavailable(format!(
                "目标点击点当前不是游戏窗口，已取消点击: {},{}",
                point.x, point.y
            )));
        }
        Ok(())
    }

    pub fn capture(&self) -> Result<DynamicImage> {
        capture_client_area(self.hwnd, self.client_width, self.client_height)
    }
}

pub fn capture_game(config: &WindowConfig) -> Result<DynamicImage> {
    GameWindow::find(config)?.capture()
}

pub fn ensure_foreground(config: &WindowConfig) -> Result<()> {
    GameWindow::find(config)?.ensure_foreground()
}

pub fn close_game(config: &WindowConfig) -> Result<()> {
    let hwnd = find_window_by_process_for_close(&config.target_process)?;
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .context("PostMessageW WM_CLOSE failed")
}

fn capture_client_area(hwnd: HWND, width: i32, height: i32) -> Result<DynamicImage> {
    unsafe {
        let source_dc = GetDC(Some(hwnd));
        if source_dc.is_invalid() {
            return Err(target_window_unavailable("GetDC failed for game window"));
        }

        let memory_dc = CreateCompatibleDC(Some(source_dc));
        if memory_dc.is_invalid() {
            ReleaseDC(Some(hwnd), source_dc);
            return Err(target_window_unavailable("CreateCompatibleDC failed"));
        }

        let bitmap = CreateCompatibleBitmap(source_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory_dc);
            ReleaseDC(Some(hwnd), source_dc);
            return Err(target_window_unavailable("CreateCompatibleBitmap failed"));
        }

        let old_object = SelectObject(memory_dc, HGDIOBJ::from(bitmap));
        if old_object.is_invalid() {
            let _ = DeleteObject(HGDIOBJ::from(bitmap));
            let _ = DeleteDC(memory_dc);
            ReleaseDC(Some(hwnd), source_dc);
            return Err(target_window_unavailable("SelectObject failed"));
        }

        if let Err(error) = BitBlt(
            memory_dc,
            0,
            0,
            width,
            height,
            Some(source_dc),
            0,
            0,
            SRCCOPY,
        ) {
            SelectObject(memory_dc, old_object);
            let _ = DeleteObject(HGDIOBJ::from(bitmap));
            let _ = DeleteDC(memory_dc);
            ReleaseDC(Some(hwnd), source_dc);
            return Err(target_window_unavailable(format!(
                "BitBlt game window failed: {error}"
            )));
        }

        let mut bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: (width as u32) * (height as u32) * 4,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            ..Default::default()
        };

        let mut bgra = vec![0_u8; (width as usize) * (height as usize) * 4];
        SelectObject(memory_dc, old_object);

        let lines = GetDIBits(
            memory_dc,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        );

        let _ = DeleteObject(HGDIOBJ::from(bitmap));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(Some(hwnd), source_dc);

        if lines == 0 {
            return Err(target_window_unavailable(
                "GetDIBits failed for game window",
            ));
        }

        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }

        let image = image::RgbaImage::from_raw(width as u32, height as u32, bgra)
            .ok_or_else(|| anyhow!("failed to construct captured game image"))?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

#[derive(Clone, Copy, Debug)]
struct ProcessWindow {
    hwnd: HWND,
    process_id: u32,
}

struct SearchState {
    targets: Vec<String>,
    target_label: String,
    found: Option<ProcessWindow>,
    include_minimized: bool,
    hidden_target_windows: u32,
    minimized_target_windows: u32,
}

fn find_window_by_process(target_process: &str) -> Result<ProcessWindow> {
    let mut state = SearchState {
        targets: normalize_process_names(target_process)?,
        target_label: target_process.to_string(),
        found: None,
        include_minimized: false,
        hidden_target_windows: 0,
        minimized_target_windows: 0,
    };
    find_window_by_process_with_state(&mut state)
}

fn find_window_by_process_for_close(target_process: &str) -> Result<HWND> {
    let mut state = SearchState {
        targets: normalize_process_names(target_process)?,
        target_label: target_process.to_string(),
        found: None,
        include_minimized: true,
        hidden_target_windows: 0,
        minimized_target_windows: 0,
    };
    find_window_by_process_with_state(&mut state).map(|window| window.hwnd)
}

fn find_window_by_process_with_state(state: &mut SearchState) -> Result<ProcessWindow> {
    let enum_result = unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM((state as *mut SearchState) as isize),
        )
    };
    if state.found.is_none() {
        enum_result.context("EnumWindows failed")?;
    }
    state.found.ok_or_else(|| {
        if state.hidden_target_windows > 0 || state.minimized_target_windows > 0 {
            target_window_unavailable(format!(
                "未找到可用目标游戏窗口进程: {} hidden_windows={} minimized_windows={}",
                state.target_label, state.hidden_target_windows, state.minimized_target_windows
            ))
        } else {
            target_window_unavailable(format!(
                "未找到目标游戏窗口进程: {} candidates={}",
                state.target_label,
                state.targets.join(",")
            ))
        }
    })
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut SearchState) };
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 {
        return true.into();
    }

    match process_name(process_id) {
        Ok(name) => {
            log::debug!("窗口进程: pid={} exe={}", process_id, name);
            if state
                .targets
                .iter()
                .any(|target| normalize_process_name(&name) == *target)
            {
                if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                    state.hidden_target_windows += 1;
                    return true.into();
                }
                if !state.include_minimized && unsafe { IsIconic(hwnd).as_bool() } {
                    state.minimized_target_windows += 1;
                    return true.into();
                }
                state.found = Some(ProcessWindow { hwnd, process_id });
                return false.into();
            }
        }
        Err(error) => {
            log::debug!("读取窗口进程名失败: pid={} error={:#}", process_id, error);
        }
    }
    true.into()
}

fn process_name(process_id: u32) -> Result<String> {
    let process = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .with_context(|| format!("OpenProcess failed for pid {}", process_id))?
    };

    let mut buffer = vec![0_u16; 32768];
    let mut len = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .with_context(|| format!("QueryFullProcessImageNameW failed for pid {}", process_id))?
    };
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    unsafe {
        let _ = CloseHandle(process);
    }
    Ok(Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or(path))
}

fn normalize_process_name(value: &str) -> String {
    let mut name = value.trim().to_ascii_lowercase();
    if !name.ends_with(".exe") {
        name.push_str(".exe");
    }
    name
}

fn normalize_process_names(value: &str) -> Result<Vec<String>> {
    let targets = value
        .split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(normalize_process_name)
        .collect::<Vec<_>>();
    if targets.is_empty() {
        bail!("未配置目标游戏窗口进程名");
    }
    Ok(targets)
}

fn scale_i32(value: i32, from: i32, to: i32) -> i32 {
    ((value as f32 / from as f32) * to as f32).round() as i32
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn send_alt_keypress() -> u32 {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x12),
                    ..Default::default()
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x12),
                    dwFlags: KEYEVENTF_KEYUP,
                    ..Default::default()
                },
            },
        },
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_multiple_target_process_names() {
        let names = normalize_process_names("yuanshen.exe, GenshinImpact").expect("process names");

        assert_eq!(
            names,
            vec!["yuanshen.exe".to_string(), "genshinimpact.exe".to_string()]
        );
    }
}
