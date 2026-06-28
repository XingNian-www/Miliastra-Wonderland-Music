use anyhow::{bail, Context, Result};

#[cfg(target_os = "windows")]
pub fn set_text(text: &str) -> Result<()> {
    use std::ptr::copy_nonoverlapping;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

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
struct ClipboardGuard;

#[cfg(target_os = "windows")]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
        }
    }
}
