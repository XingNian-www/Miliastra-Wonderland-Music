//! 聊天区变化门控。
//!
//! 2026-10-17 从 `composition/application/shared_observer.rs` 移到这里：
//! composition 在 `lib.rs` 里是 `#[cfg(target_os = "windows")]`，放那儿在 Linux 上
//! 连编译都到不了、更别说测试；门控本身只依赖 `ChangeFingerprint`，属于聊天观察领域。
//!
//! 判据与 v5.9.4 完全同口径：104×36 灰度指纹的 `mean_abs_diff` / `changed_ratio`
//! 任一超阈值才算「聊天区变了」。旧实现是整段聊天区的 u64 原始像素哈希等值比较，
//! 3D 场景透出、粒子、半透明底纹的抖动都会让它恒为 changed，扫描只能靠 fallback 兜底。

use std::time::{Duration, Instant};

use crate::ui::change_detection::{ChangeFingerprint, change_stats};

/// 均值差或变化比例任一超阈值 ⇒ 聊天区变了（v5.9.4 口径）。
pub(crate) fn fingerprint_changed(
    previous: &ChangeFingerprint,
    current: &ChangeFingerprint,
    mean_threshold: f32,
    ratio_threshold: f32,
) -> bool {
    let stats = change_stats(previous, current);
    stats.mean_abs_diff >= mean_threshold || stats.changed_ratio >= ratio_threshold
}

/// 被动扫描门控：变化 + 稳定防抖 + 兜底周期。
#[derive(Default)]
pub(crate) struct PassiveScanGate {
    published: Option<ChangeFingerprint>,
    candidate: Option<(ChangeFingerprint, Instant)>,
    pending: Option<ChangeFingerprint>,
    last_scan: Option<Instant>,
}

impl PassiveScanGate {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_due(
        &mut self,
        fingerprint: &ChangeFingerprint,
        now: Instant,
        forced: bool,
        fallback: Duration,
        debounce: Duration,
        mean_threshold: f32,
        ratio_threshold: f32,
    ) -> bool {
        let fallback_due = self
            .last_scan
            .is_none_or(|last| now.saturating_duration_since(last) >= fallback);
        let changed = self.published.as_ref().is_none_or(|previous| {
            fingerprint_changed(previous, fingerprint, mean_threshold, ratio_threshold)
        });
        if !changed {
            self.candidate = None;
        } else if self.candidate.as_ref().is_none_or(|(previous, _)| {
            fingerprint_changed(previous, fingerprint, mean_threshold, ratio_threshold)
        }) {
            self.candidate = Some((fingerprint.clone(), now));
        }
        let settled = self
            .candidate
            .as_ref()
            .is_some_and(|(_, first)| now.saturating_duration_since(*first) >= debounce);
        let due = forced || fallback_due || (changed && settled);
        self.pending = due.then(|| fingerprint.clone());
        due
    }

    pub(crate) fn complete(&mut self, now: Instant) {
        self.published = self.pending.take();
        self.last_scan = Some(now);
        self.candidate = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同值指纹 = 没变化；不同值 = 变化（均值差 255，远超阈值）。
    fn fp(value: u8) -> ChangeFingerprint {
        ChangeFingerprint {
            pixels: vec![value; 104 * 36],
            width: 104,
            height: 36,
        }
    }

    const MEAN: f32 = 6.0;
    const RATIO: f32 = 0.03;

    #[test]
    fn unchanged_frames_skip_ocr_until_fallback() {
        let now = Instant::now();
        let mut gate = PassiveScanGate::default();
        let fallback = Duration::from_secs(5);
        let debounce = Duration::from_millis(100);
        assert!(gate.scan_due(&fp(10), now, false, fallback, debounce, MEAN, RATIO));
        gate.complete(now);
        assert!(!gate.scan_due(
            &fp(10),
            now + Duration::from_secs(1),
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
        assert!(gate.scan_due(&fp(10), now + fallback, false, fallback, debounce, MEAN, RATIO));
    }

    /// 轻微抖动（均值差 2、变化比例 0）= 被阈值吸收，不触发扫描。
    #[test]
    fn minor_background_jitter_is_absorbed_by_thresholds() {
        let now = Instant::now();
        let mut gate = PassiveScanGate::default();
        let fallback = Duration::from_secs(5);
        let debounce = Duration::from_millis(100);
        assert!(gate.scan_due(&fp(10), now, false, fallback, debounce, MEAN, RATIO));
        gate.complete(now);
        assert!(!gate.scan_due(
            &fp(12),
            now + Duration::from_millis(500),
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
        assert!(!gate.scan_due(
            &fp(11),
            now + Duration::from_millis(700),
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
    }

    #[test]
    fn changed_frames_debounce_and_failed_scan_stays_due() {
        let now = Instant::now();
        let mut gate = PassiveScanGate::default();
        let fallback = Duration::from_secs(5);
        let debounce = Duration::from_millis(100);
        assert!(gate.scan_due(&fp(0), now, false, fallback, debounce, MEAN, RATIO));
        gate.complete(now);
        assert!(!gate.scan_due(
            &fp(255),
            now + debounce,
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
        assert!(gate.scan_due(
            &fp(255),
            now + debounce * 2,
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
        assert!(gate.scan_due(
            &fp(255),
            now + debounce * 3,
            false,
            fallback,
            debounce,
            MEAN,
            RATIO
        ));
    }

    #[test]
    fn forced_refresh_bypasses_unchanged_cache() {
        let requested = Instant::now();
        let mut gate = PassiveScanGate::default();
        let sample = fp(7);
        assert!(gate.scan_due(&sample, requested, false, Duration::from_secs(5), Duration::ZERO, MEAN, RATIO));
        gate.complete(requested);
        assert!(gate.scan_due(
            &sample,
            requested,
            true,
            Duration::from_secs(5),
            Duration::from_secs(1),
            MEAN,
            RATIO
        ));
    }
}