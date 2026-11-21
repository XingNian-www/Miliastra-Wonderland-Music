//! UI 诊断存档的进程级开关与目录解析。
//!
//! 用途：UI 例程识别/执行失败时把**当时那一帧画面**落盘，便于事后复核
//! "当时到底是黑屏、冻结帧，还是没建模的新界面"。只存图片与元数据，
//! 文件名不含任何正文/UID/昵称（隐私要求见 `crate::privacy`）。
//!
//! 开关只有**一个来源**：配置段 `diagnostics.ui_frames`（Web 面板可改，
//! 标为 Live，保存后立即生效）。刻意不再支持 `MILIASTRA_UI_DIAGNOSTICS`
//! 环境变量——排查问题不该要求用户去改启动脚本，面板能改的地方就都在面板改。
//!
//! 为什么要有全局开关而不只是把参数一路传下去：落盘点分散在
//! `ui::routines::transaction`（通用事务包装）与 `runtime::ui`
//! （`save_failure_frame`）两处，前者拿不到配置快照、后者在
//! `UiRuntimeHandle` 里。搞成进程级 `AtomicBool` 后，配置保存时刷新一次，
//! 落盘点只读这一个位，避免为了一个诊断开关把 `AppConfig` 穿过整条调用链。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// 是否把 UI 帧落盘。默认关闭（避免无人看管时白占磁盘）。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 用配置值刷新开关；配置中心保存后由 live 应用路径调用。
pub(crate) fn set_enabled(enabled: bool) {
    let previous = ENABLED.swap(enabled, Ordering::AcqRel);
    if previous != enabled {
        log::info!(
            "UI 诊断存档{}（图片只含画面，文件名不含正文/UID/昵称）",
            if enabled { "已开启" } else { "已关闭" }
        );
    }
}

/// 当前是否开启。
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// 诊断存档目录：`<exe>/deps/diagnostics`。
///
/// 与失败帧目录（`deps/logs/ui-failures`）分开：那边是**始终开启**的失败证据，
/// 这边是可以随时开关、频率更高的过程帧；混在一个目录里会让失败证据
/// 被海量过程帧淹没。
pub(crate) fn directory() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join("deps").join("diagnostics"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 全局开关是进程级单例，测试必须串行，否则互相踩状态。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 开关只由配置位驱动；配置关着就必须不落盘。
    #[test]
    fn switch_follows_the_config_value_only() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        set_enabled(false);
        assert!(!enabled(), "默认/关闭时必须不开启");

        set_enabled(true);
        assert!(enabled(), "配置打开后必须生效");

        set_enabled(false);
        assert!(!enabled(), "配置关闭后必须立即停止落盘");
    }

    /// 重复设置同一个值不应产生两次"状态变化"日志。
    #[test]
    fn setting_the_same_value_is_idempotent() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        set_enabled(true);
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        set_enabled(false);
        assert!(!enabled());
    }
}
