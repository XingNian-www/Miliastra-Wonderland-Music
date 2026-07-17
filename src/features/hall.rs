use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod state;

pub(crate) use state::{HallRuntimeState, HallStatePatch, PersistentHallState};

use crate::features::chat_text::{
    CommandSyntax, command_identity, parse_prefixed_command, strip_ascii_case_prefix,
};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::runtime::clock::{Clock, WallClock};
use crate::text::normalize_comparison_text;

pub(crate) const HALL_EXPIRING_WARNING_MINUTES: u32 = 10;

pub(crate) enum HallMutationIntent {
    PatchState(HallStatePatch),
}

pub(crate) enum HallMutationOutcome {
    StatePatched,
}

pub(crate) struct HallStateService {
    state: PersistentHallState,
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
    countdown_updated_at: Option<Instant>,
}

impl HallStateService {
    pub(crate) fn load(
        path: PathBuf,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
    ) -> Result<Self> {
        let state = PersistentHallState::load(path)?;
        let mut service = Self::from_state(state, clock, wall_clock);
        if service.clear_countdown_cache()? {
            log::info!("启动时已清理上次运行的大厅倒计时缓存，等待本次大厅检测重新确认");
        }
        Ok(service)
    }

    #[cfg(test)]
    pub(crate) fn new_with_time(
        state: PersistentHallState,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
    ) -> Self {
        Self::from_state(state, clock, wall_clock)
    }

    fn from_state(
        state: PersistentHallState,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
    ) -> Self {
        let mut service = Self {
            state,
            clock,
            wall_clock,
            countdown_updated_at: None,
        };
        service.refresh_countdown_anchor();
        service
    }

    pub(crate) fn snapshot(&self) -> HallRuntimeState {
        let mut snapshot = self.state.state().clone();
        if let (Some(minutes), Some(updated_at)) =
            (snapshot.remaining_minutes, self.countdown_updated_at)
        {
            let elapsed_minutes = self
                .clock
                .now()
                .saturating_duration_since(updated_at)
                .as_secs()
                / 60;
            snapshot.remaining_minutes = Some(minutes.saturating_sub(elapsed_minutes as u32));
        }
        snapshot
    }

    pub(crate) fn patch(&mut self, patch: HallStatePatch) -> Result<()> {
        let countdown_changed =
            patch.remaining_minutes.is_some() || patch.remaining_updated_at.is_some();
        self.state.state_mut().apply_patch(patch);
        if countdown_changed {
            self.refresh_countdown_anchor();
        }
        self.state.save()
    }

    pub(crate) fn update_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        let updated_at = self.wall_clock.unix_seconds();
        self.state
            .state_mut()
            .update_remaining_minutes(minutes, updated_at);
        self.countdown_updated_at = (minutes > 0).then(|| self.clock.now());
        self.state.save()
    }

    pub(crate) fn clear_remaining_minutes(&mut self) -> Result<()> {
        self.state.state_mut().clear_remaining_minutes();
        self.countdown_updated_at = None;
        self.state.save()
    }

    pub(crate) fn clear_countdown_cache(&mut self) -> Result<bool> {
        let cleared = self.state.state_mut().clear_countdown_cache();
        self.countdown_updated_at = None;
        if cleared {
            self.state.save()?;
        }
        Ok(cleared)
    }

    fn refresh_countdown_anchor(&mut self) {
        self.countdown_updated_at = match (
            self.state.state().remaining_minutes,
            self.state.state().remaining_updated_at,
        ) {
            (Some(minutes), Some(updated_at)) if minutes > 0 => {
                let now = self.clock.now();
                let elapsed = self.wall_clock.unix_seconds().saturating_sub(updated_at);
                now.checked_sub(Duration::from_secs(elapsed)).or(Some(now))
            }
            _ => None,
        };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HallCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HallObservation {
    pub(crate) name: String,
    pub(crate) remaining_minutes: Option<u32>,
}

pub(crate) trait HallDetectionPort {
    fn detect_public_hall(&mut self) -> Result<Option<HallObservation>>;
    fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()>;
    fn clear_hall_remaining_minutes(&mut self) -> Result<()>;
}

pub(crate) trait HallApplicationPort: HallDetectionPort {
    fn reply(&mut self, message: &str) -> Result<()>;
    fn log_executed(&mut self, context: &HallCommandContext, final_command: &str) -> Result<()>;
    fn read_hall_info(&mut self) -> Result<HallObservation>;
    fn toggle_microphone(&mut self) -> Result<()>;
    fn hall_remaining_minutes(&mut self) -> Result<Option<u32>>;
}

pub(crate) trait HallMaintenancePort {
    fn executor_is_idle(&mut self) -> Result<bool>;
    fn hall_expiring_warning_sent(&mut self) -> Result<bool>;
    fn hall_remaining_minutes(&mut self) -> Result<Option<u32>>;
    fn reply(&mut self, message: &str) -> Result<()>;
    fn mark_hall_expiring_warning_sent(&mut self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HallApplication;

impl HallApplication {
    pub(crate) fn maybe_warn_expiring<P: HallMaintenancePort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<bool> {
        if !port.executor_is_idle()? || port.hall_expiring_warning_sent()? {
            return Ok(false);
        }
        let Some(minutes) = port.hall_remaining_minutes()? else {
            return Ok(false);
        };
        if minutes > HALL_EXPIRING_WARNING_MINUTES {
            return Ok(false);
        }
        let message = if minutes == 0 {
            "大厅即将到期".to_string()
        } else {
            format!("大厅即将到期，剩余{}分钟", minutes)
        };
        port.reply(&message)?;
        port.mark_hall_expiring_warning_sent()?;
        Ok(true)
    }

    pub(crate) fn execute<P: HallApplicationPort + ?Sized>(
        &self,
        context: &HallCommandContext,
        command: &HallCommand,
        port: &mut P,
    ) -> Result<()> {
        match command {
            HallCommand::Detect => {
                port.log_executed(context, "hall detect")?;
                self.execute_detect(port)
            }
            HallCommand::Time => {
                port.log_executed(context, "hall time")?;
                self.reply_hall_time(port)
            }
            HallCommand::ToggleMicrophone { username } => {
                log::info!("收到麦克风命令: {}", username);
                if self.check_public_hall(port)? {
                    port.log_executed(
                        context,
                        &format!("microphone skipped publicHall {}", username),
                    )?;
                    log::info!("麦克风: 当前在公共大厅，跳过状态切换和通告");
                    Ok(())
                } else {
                    port.log_executed(context, &format!("microphone toggle {}", username))?;
                    port.toggle_microphone()?;
                    log::info!("麦克风: 已按 N 切换状态");
                    port.reply(&format!("@{} 执行了切换麦克风状态！", username))
                }
            }
        }
    }

    fn execute_detect<P: HallApplicationPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let info = match port.read_hall_info() {
            Ok(info) => info,
            Err(error) => {
                log::error!("大厅检测 OCR 失败: {error:#}");
                return port.reply("大厅检测失败");
            }
        };
        log::info!("大厅检测 OCR 结果: {}", info.name);
        if is_public_hall(&info.name) {
            port.clear_hall_remaining_minutes()?;
            port.reply("当前为公共大厅")
        } else {
            if let Some(minutes) = info.remaining_minutes {
                port.update_hall_remaining_minutes(minutes)?;
                log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
            }
            let name = if info.name.is_empty() {
                "未识别到大厅名称"
            } else {
                info.name.as_str()
            };
            let suffix = info
                .remaining_minutes
                .map(|minutes| format!("，剩余{}分钟", minutes))
                .unwrap_or_default();
            port.reply(&format!("当前为{}{}", name, suffix))
        }
    }

    fn reply_hall_time<P: HallApplicationPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        if let Some(minutes) = port
            .hall_remaining_minutes()?
            .filter(|minutes| *minutes > 0)
        {
            return port.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }

        log::info!("大厅时间未知，执行一次大厅识别");
        let info = match port.read_hall_info() {
            Ok(info) => info,
            Err(error) => {
                log::error!("大厅时间 OCR 失败: {error:#}");
                return port.reply("大厅时间未知");
            }
        };
        if is_public_hall(&info.name) {
            port.clear_hall_remaining_minutes()?;
            return port.reply("公共大厅无时间限制");
        }
        if let Some(minutes) = info.remaining_minutes {
            port.update_hall_remaining_minutes(minutes)?;
            return port.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }
        port.reply("大厅时间未知")
    }

    pub(crate) fn check_public_hall<P: HallDetectionPort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<bool> {
        let Some(info) = port.detect_public_hall()? else {
            return Ok(false);
        };
        log::info!("大厅检测 OCR 结果: {}", info.name);
        let public = is_public_hall(&info.name);
        if public {
            port.clear_hall_remaining_minutes()?;
        } else if let Some(minutes) = info.remaining_minutes {
            port.update_hall_remaining_minutes(minutes)?;
            log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
        }
        Ok(public)
    }
}

fn is_public_hall(name: &str) -> bool {
    normalize_comparison_text(name) == normalize_comparison_text("公共大厅")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum HallCommand {
    Detect,
    Time,
    ToggleMicrophone { username: String },
}

impl HallCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        if envelope.prefix() != CommandPrefix::At {
            return false;
        }
        match envelope.authority() {
            CommandAuthority::HallMember => ["大厅检测", "大厅时间"]
                .iter()
                .any(|prefix| envelope.command_text().starts_with(prefix)),
            CommandAuthority::Friend => {
                strip_ascii_case_prefix(envelope.command_text(), "麦克风").is_some()
            }
        }
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        match envelope.authority() {
            CommandAuthority::HallMember => {
                let parsed = Self::parse_hall(envelope.command_text())?;
                let raw = if parsed.argument.is_empty() {
                    parsed.matched.to_string()
                } else {
                    format!("{} {}", parsed.matched, parsed.argument)
                };
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    raw,
                    parsed.command,
                ))
            }
            CommandAuthority::Friend => {
                Self::parse_friend(envelope.command_text(), envelope.username()).map(|command| {
                    FeatureCommandMatch::new(
                        "麦克风",
                        format!("麦克风 {}", envelope.username()),
                        command,
                    )
                })
            }
        }
    }

    pub(crate) fn parse_hall(text: &str) -> Option<CommandSyntax<'_, Self>> {
        for prefix in ["大厅检测", "大厅时间"] {
            let Some(argument) = parse_prefixed_command(text, prefix, false) else {
                continue;
            };
            let command = match prefix {
                "大厅检测" => Self::Detect,
                "大厅时间" => Self::Time,
                _ => unreachable!("all hall prefixes are handled"),
            };
            return Some(CommandSyntax {
                matched: prefix,
                argument,
                command,
            });
        }
        None
    }

    pub(crate) fn parse_friend(text: &str, username: &str) -> Option<Self> {
        let rest = strip_ascii_case_prefix(text, "麦克风")?;
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        (rest.is_empty() || rest.starts_with([']', '】'])).then(|| Self::ToggleMicrophone {
            username: username.to_string(),
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Detect => "hall_detect".to_string(),
            Self::Time => "hall_time".to_string(),
            Self::ToggleMicrophone { username } => {
                format!("microphone:{}", command_identity(username))
            }
        }
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ToggleMicrophone { username: left },
                Self::ToggleMicrophone { username: right },
            ) => command_identity(left) == command_identity(right),
            _ => self.lock_key() == other.lock_key(),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runtime::clock::ManualClock;

    struct MaintenancePort {
        idle: bool,
        warning_sent: bool,
        remaining_minutes: Option<u32>,
        replies: Vec<String>,
        marked: bool,
    }

    impl HallMaintenancePort for MaintenancePort {
        fn executor_is_idle(&mut self) -> Result<bool> {
            Ok(self.idle)
        }

        fn hall_expiring_warning_sent(&mut self) -> Result<bool> {
            Ok(self.warning_sent)
        }

        fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
            Ok(self.remaining_minutes)
        }

        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn mark_hall_expiring_warning_sent(&mut self) -> Result<()> {
            self.marked = true;
            Ok(())
        }
    }

    fn maintenance_port(minutes: Option<u32>) -> MaintenancePort {
        MaintenancePort {
            idle: true,
            warning_sent: false,
            remaining_minutes: minutes,
            replies: Vec::new(),
            marked: false,
        }
    }

    #[test]
    fn countdown_snapshot_uses_the_injected_monotonic_clock() {
        let path = std::env::temp_dir().join(format!(
            "miliastra-hall-clock-{}-{}.json",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 1_234));
        let state = PersistentHallState::load(path.clone()).expect("load hall state");
        let mut service = HallStateService::new_with_time(state, clock.clone(), clock.clone());

        service.update_remaining_minutes(5).unwrap();
        clock.advance(Duration::from_secs(61)).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(snapshot.remaining_minutes_now(), Some(4));
        assert_eq!(snapshot.remaining_updated_at, Some(1_234));
        let _ = std::fs::remove_file(path);
    }

    struct PublicHallPort {
        replies: Vec<String>,
        cleared: bool,
    }

    impl HallDetectionPort for PublicHallPort {
        fn detect_public_hall(&mut self) -> Result<Option<HallObservation>> {
            Ok(Some(HallObservation {
                name: "公共大厅".to_string(),
                remaining_minutes: None,
            }))
        }

        fn update_hall_remaining_minutes(&mut self, _minutes: u32) -> Result<()> {
            Ok(())
        }

        fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
            self.cleared = true;
            Ok(())
        }
    }

    impl HallApplicationPort for PublicHallPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn log_executed(
            &mut self,
            _context: &HallCommandContext,
            _final_command: &str,
        ) -> Result<()> {
            Ok(())
        }

        fn read_hall_info(&mut self) -> Result<HallObservation> {
            Ok(HallObservation {
                name: "公共大厅".to_string(),
                remaining_minutes: None,
            })
        }

        fn toggle_microphone(&mut self) -> Result<()> {
            unreachable!("public halls do not toggle the microphone")
        }

        fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
            Ok(Some(5))
        }
    }

    #[test]
    fn detecting_a_public_hall_clears_the_countdown() {
        let mut port = PublicHallPort {
            replies: Vec::new(),
            cleared: false,
        };
        let context = HallCommandContext {
            message_type: "blue".to_string(),
            username: "测试".to_string(),
            user_command: "@大厅检测".to_string(),
        };

        HallApplication
            .execute(&context, &HallCommand::Detect, &mut port)
            .expect("hall detection");

        assert!(port.cleared);
        assert_eq!(port.replies, ["当前为公共大厅"]);
    }

    #[test]
    fn idle_hall_warns_once_when_the_countdown_reaches_the_threshold() {
        let mut port = maintenance_port(Some(HALL_EXPIRING_WARNING_MINUTES));

        let warned = HallApplication
            .maybe_warn_expiring(&mut port)
            .expect("hall expiry warning");

        assert!(warned);
        assert_eq!(port.replies, ["大厅即将到期，剩余10分钟"]);
        assert!(port.marked);
    }

    #[test]
    fn hall_expiry_warning_waits_for_an_idle_executor() {
        let mut port = maintenance_port(Some(5));
        port.idle = false;

        let warned = HallApplication
            .maybe_warn_expiring(&mut port)
            .expect("busy executor check");

        assert!(!warned);
        assert!(port.replies.is_empty());
        assert!(!port.marked);
    }

    #[test]
    fn hall_expiry_warning_is_not_repeated_or_sent_before_the_threshold() {
        let mut already_sent = maintenance_port(Some(5));
        already_sent.warning_sent = true;
        let mut too_early = maintenance_port(Some(HALL_EXPIRING_WARNING_MINUTES + 1));

        assert!(
            !HallApplication
                .maybe_warn_expiring(&mut already_sent)
                .expect("already-sent warning check")
        );
        assert!(
            !HallApplication
                .maybe_warn_expiring(&mut too_early)
                .expect("early warning check")
        );
        assert!(already_sent.replies.is_empty());
        assert!(too_early.replies.is_empty());
        assert!(!already_sent.marked);
        assert!(!too_early.marked);
    }

    #[test]
    fn expired_hall_uses_the_message_without_a_zero_minute_suffix() {
        let mut port = maintenance_port(Some(0));

        let warned = HallApplication
            .maybe_warn_expiring(&mut port)
            .expect("expired hall warning");

        assert!(warned);
        assert_eq!(port.replies, ["大厅即将到期"]);
        assert!(port.marked);
    }
}
