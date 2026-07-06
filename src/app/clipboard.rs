use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug)]
struct TextSummary {
    chars: usize,
    fingerprint: u64,
}

impl TextSummary {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().count(),
            fingerprint: text_fingerprint(text),
        }
    }
}

#[cfg(target_os = "windows")]
pub fn get_text() -> Result<Option<String>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
            return Ok(None);
        }
        OpenClipboard(None).context("open clipboard")?;
        let _guard = ClipboardGuard;

        let handle = GetClipboardData(CF_UNICODETEXT).context("get clipboard data")?;
        if handle.is_invalid() {
            return Ok(None);
        }
        let memory = HGLOBAL(handle.0);
        let locked = GlobalLock(memory);
        if locked.is_null() {
            bail!("lock clipboard memory failed");
        }
        let wide = locked.cast::<u16>();
        let mut len = 0usize;
        while *wide.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        let _ = GlobalUnlock(memory);
        Ok(Some(text))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn get_text() -> Result<Option<String>> {
    bail!("clipboard paste is only supported on Windows")
}

#[cfg(target_os = "windows")]
pub fn set_text(text: &str) -> Result<()> {
    use std::ptr::copy_nonoverlapping;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    let mut wide = text.encode_utf16().collect::<Vec<_>>();
    wide.push(0);
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        OpenClipboard(None).context("open clipboard")?;
        let _guard = ClipboardGuard;
        EmptyClipboard().context("empty clipboard")?;

        let memory = GlobalAlloc(GMEM_MOVEABLE, byte_len).context("allocate clipboard memory")?;
        let locked = GlobalLock(memory);
        if locked.is_null() {
            bail!("lock clipboard memory failed");
        }
        copy_nonoverlapping(wide.as_ptr(), locked.cast::<u16>(), wide.len());
        let _ = GlobalUnlock(memory);

        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).context("set clipboard data")?;
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_text(_text: &str) -> Result<()> {
    bail!("clipboard paste is only supported on Windows")
}

#[cfg(target_os = "windows")]
pub fn clear() -> Result<()> {
    use windows::Win32::System::DataExchange::{EmptyClipboard, OpenClipboard};

    unsafe {
        OpenClipboard(None).context("open clipboard")?;
        let _guard = ClipboardGuard;
        EmptyClipboard().context("empty clipboard")?;
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn clear() -> Result<()> {
    bail!("clipboard paste is only supported on Windows")
}

pub struct TextRestoreGuard {
    restore: Option<RestoreTarget>,
}

impl TextRestoreGuard {
    pub fn replace_with(text: &str) -> Result<Self> {
        let replacement = TextSummary::new(text);
        let restore = match get_text() {
            Ok(Some(previous)) => {
                let previous_summary = TextSummary::new(&previous);
                log::info!(
                    "剪贴板临时占用: 写入chars={} 写入hash={:016x} 原文本存在=true 原文本chars={} 原文本hash={:016x}",
                    replacement.chars,
                    replacement.fingerprint,
                    previous_summary.chars,
                    previous_summary.fingerprint
                );
                RestoreTarget::Text(previous)
            }
            Ok(None) => {
                log::info!(
                    "剪贴板临时占用: 写入chars={} 写入hash={:016x} 原文本存在=false",
                    replacement.chars,
                    replacement.fingerprint
                );
                RestoreTarget::NoText
            }
            Err(error) => {
                log::warn!("读取原剪贴板文本失败，粘贴结束后会清空临时文本: {error:#}");
                RestoreTarget::NoText
            }
        };
        set_text(text).context("设置剪贴板文本")?;
        Ok(Self {
            restore: Some(restore),
        })
    }
}

impl Drop for TextRestoreGuard {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            match restore {
                RestoreTarget::Text(previous) => {
                    let previous_summary = TextSummary::new(&previous);
                    if let Err(error) = set_text(&previous) {
                        log::warn!("恢复原剪贴板文本失败: {error:#}");
                    } else {
                        log::info!(
                            "剪贴板临时占用结束: 已恢复原文本 chars={} hash={:016x}",
                            previous_summary.chars,
                            previous_summary.fingerprint
                        );
                    }
                }
                RestoreTarget::NoText => {
                    if let Err(error) = clear() {
                        log::warn!("清空临时剪贴板文本失败: {error:#}");
                    } else {
                        log::info!("剪贴板临时占用结束: 已清空临时文本");
                    }
                }
            }
        }
    }
}

enum RestoreTarget {
    Text(String),
    NoText,
}

fn text_fingerprint(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(target_os = "windows")]
struct ClipboardGuard;

#[cfg(target_os = "windows")]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
        }
    }
}
