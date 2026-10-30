use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

pub fn set_process_dpi_awareness() {
    if let Err(error) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        log::debug!(
            "未能启用进程级 DPI 感知（可能已由系统或清单设定），按当前缩放继续运行：{error}"
        );
    }
}
