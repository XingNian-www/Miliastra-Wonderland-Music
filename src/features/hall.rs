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
use crate::features::reply_template::{ReplyVars, hall as templates};
use crate::text::normalize_comparison_text;
use miliastra_contracts::StateStore;
use miliastra_kernel::clock::{Clock, WallClock};

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
    countdown_elapsed_before_start: Duration,
}

impl HallStateService {
    pub(crate) fn load(
        path: PathBuf,
        store: Arc<dyn StateStore>,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
    ) -> Result<Self> {
        let state = PersistentHallState::load(path, store)?;
        Ok(Self::from_state(state, clock, wall_clock))
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
            countdown_elapsed_before_start: Duration::ZERO,
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
                .countdown_elapsed_before_start
                .saturating_add(self.clock.now().saturating_duration_since(updated_at))
                .as_secs()
                / 60;
            snapshot.remaining_minutes =
                Some(u64::from(minutes).saturating_sub(elapsed_minutes) as u32);
        }
        snapshot
    }

    pub(crate) fn countdown_anchor(&self) -> (Option<u32>, Option<u64>) {
        let state = self.state.state();
        (state.remaining_minutes, state.remaining_updated_at)
    }

    pub(crate) fn patch(&mut self, patch: HallStatePatch) -> Result<()> {
        let countdown_changed =
            patch.remaining_minutes.is_some() || patch.remaining_updated_at.is_some();
        self.state.update(|state| {
            state.apply_patch(patch);
            true
        })?;
        if countdown_changed {
            self.refresh_countdown_anchor();
        }
        Ok(())
    }

    pub(crate) fn update_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        let updated_at = self.wall_clock.unix_seconds();
        self.state.update(|state| {
            state.update_remaining_minutes(minutes, updated_at);
            true
        })?;
        self.countdown_updated_at = Some(self.clock.now());
        self.countdown_elapsed_before_start = Duration::ZERO;
        Ok(())
    }

    pub(crate) fn clear_remaining_minutes(&mut self) -> Result<()> {
        self.state.update(|state| {
            state.clear_remaining_minutes();
            true
        })?;
        self.countdown_updated_at = None;
        self.countdown_elapsed_before_start = Duration::ZERO;
        Ok(())
    }

    pub(crate) fn clear_countdown_cache(&mut self) -> Result<bool> {
        let cleared = self.state.update(HallRuntimeState::clear_countdown_cache)?;
        self.countdown_updated_at = None;
        self.countdown_elapsed_before_start = Duration::ZERO;
        Ok(cleared)
    }

    fn refresh_countdown_anchor(&mut self) {
        self.countdown_elapsed_before_start = Duration::ZERO;
        self.countdown_updated_at = match (
            self.state.state().remaining_minutes,
            self.state.state().remaining_updated_at,
        ) {
            (Some(_), Some(updated_at)) => {
                // 墙上时间计入停机时长；活动倒计时使用单调时钟。
                self.countdown_elapsed_before_start =
                    Duration::from_secs(self.wall_clock.unix_seconds().saturating_sub(updated_at));
                Some(self.clock.now())
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

    /// 按模板键回复。
    ///
    /// 业务层只声明"用哪条模板、带哪些变量"，**不知道配置存在**；
    /// 覆盖文案的查找由实现方（composition 层）负责，features 层不反向依赖配置中心。
    /// 默认实现只渲染内置文案，保证不接配置的调用方（测试替身）也能工作。
    fn reply_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &ReplyVars,
    ) -> Result<()> {
        let rendered = crate::features::reply_template::render(spec, None, vars);
        self.reply(&rendered.text)
    }

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

    /// 按模板键回复；语义同 HallApplicationPort::reply_template。
    fn reply_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &ReplyVars,
    ) -> Result<()> {
        let rendered = crate::features::reply_template::render(spec, None, vars);
        self.reply(&rendered.text)
    }

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
        // minutes == 0 与 minutes > 0 是两条不同语义的文案：合成一条带可选后缀的
        // 模板会让用户在覆盖时留出空槽位，所以保持两个独立 key。
        let spec = if minutes == 0 {
            &templates::EXPIRING_WARNING
        } else {
            &templates::EXPIRING_WARNING_MINUTES
        };
        let vars = ReplyVars::new().with("剩余分钟", minutes.to_string());
        port.reply_template(spec, &vars)?;
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
                log::info!("收到「{}」的麦克风命令", username);
                if self.check_public_hall(port)? {
                    port.log_executed(
                        context,
                        &format!("microphone skipped publicHall {}", username),
                    )?;
                    log::debug!("麦克风：当前在公共大厅，跳过状态切换和通告");
                    Ok(())
                } else {
                    port.log_executed(context, &format!("microphone toggle {}", username))?;
                    port.toggle_microphone()?;
                    log::info!("麦克风状态已切换（按 N 键）");
                    port.reply_template(
                        &templates::MICROPHONE_TOGGLED,
                        &ReplyVars::new().with("玩家", username.as_str()),
                    )
                }
            }
        }
    }

    fn execute_detect<P: HallApplicationPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let info = match port.read_hall_info() {
            Ok(info) => info,
            Err(error) => {
                log::error!(
                    "大厅检测失败（步骤：识别当前大厅名称）；无法判断所在大厅，本次命令未执行: {error:#}"
                );
                return port.reply_template(&templates::DETECT_FAILED, &ReplyVars::new());
            }
        };
        log::info!("大厅检测结果：当前大厅为「{}」", info.name);
        if is_public_hall(&info.name) {
            port.clear_hall_remaining_minutes()?;
            port.reply_template(&templates::PUBLIC_HALL_CURRENT, &ReplyVars::new())
        } else {
            if let Some(minutes) = info.remaining_minutes {
                port.update_hall_remaining_minutes(minutes)?;
                log::info!("大厅剩余时间识别结果：{} 分钟", minutes);
            }
            let name = if info.name.is_empty() {
                "未识别到大厅名称"
            } else {
                info.name.as_str()
            };
            // 后缀（含前导全角逗号）由调用方组织：没有倒计时时是空串，
            // 这样回复不会多出一个孤零零的逗号。
            let suffix = info
                .remaining_minutes
                .map(|minutes| format!("，剩余{}分钟", minutes))
                .unwrap_or_default();
            port.reply_template(
                &templates::CURRENT,
                &ReplyVars::new().with("大厅名", name).with("后缀", suffix),
            )
        }
    }

    fn reply_hall_time<P: HallApplicationPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        if let Some(minutes) = port
            .hall_remaining_minutes()?
            .filter(|minutes| *minutes > 0)
        {
            return port.reply_template(
                &templates::TIME_REMAINING,
                &ReplyVars::new().with("剩余分钟", minutes.to_string()),
            );
        }

        log::info!("当前没有已知的大厅剩余时间，先执行一次大厅识别");
        let info = match port.read_hall_info() {
            Ok(info) => info,
            Err(error) => {
                log::error!(
                    "读取大厅剩余时间失败（步骤：识别大厅时间文字）；无法给出剩余时长，本次命令未执行: {error:#}"
                );
                return port.reply_template(&templates::TIME_UNKNOWN, &ReplyVars::new());
            }
        };
        if is_public_hall(&info.name) {
            port.clear_hall_remaining_minutes()?;
            return port.reply_template(&templates::PUBLIC_HALL_NO_LIMIT, &ReplyVars::new());
        }
        if let Some(minutes) = info.remaining_minutes {
            port.update_hall_remaining_minutes(minutes)?;
            return port.reply_template(
                &templates::TIME_REMAINING,
                &ReplyVars::new().with("剩余分钟", minutes.to_string()),
            );
        }
        // 两处"大厅时间未知"共用同一条模板：语义相同，不该让用户维护两份覆盖。
        port.reply_template(&templates::TIME_UNKNOWN, &ReplyVars::new())
    }

    pub(crate) fn check_public_hall<P: HallDetectionPort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<bool> {
        let Some(info) = port.detect_public_hall()? else {
            return Ok(false);
        };
        log::info!("大厅检测结果：当前大厅为「{}」", info.name);
        let public = is_public_hall(&info.name);
        if public {
            port.clear_hall_remaining_minutes()?;
        } else if let Some(minutes) = info.remaining_minutes {
            port.update_hall_remaining_minutes(minutes)?;
            log::info!("大厅剩余时间识别结果：{} 分钟", minutes);
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
                _ => return None,
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
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;
    use miliastra_kernel::clock::ManualClock;

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
        let state =
            PersistentHallState::load(path.clone(), crate::test_support::test_state_store())
                .expect("load hall state");
        let mut service = HallStateService::new_with_time(state, clock.clone(), clock.clone());

        service.update_remaining_minutes(5).unwrap();
        clock.advance(Duration::from_secs(61)).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(snapshot.remaining_minutes_now(), Some(4));
        assert_eq!(snapshot.remaining_updated_at, Some(1_234));
        assert_eq!(service.countdown_anchor(), (Some(5), Some(1_234)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn restarting_preserves_countdown_deadline_and_expiry_warning() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "miliastra-hall-restart-{}-{suffix}.json",
            std::process::id()
        ));
        let store = crate::test_support::test_state_store();
        let initial_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 1_234));
        let mut service = HallStateService::load(
            path.clone(),
            store.clone(),
            initial_clock.clone(),
            initial_clock,
        )
        .unwrap();
        service.update_remaining_minutes(5).unwrap();
        service
            .patch(HallStatePatch {
                expiring_warning_sent: Some(true),
                ..HallStatePatch::default()
            })
            .unwrap();
        drop(service);

        let restarted_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 1_234 + 121));
        let service = HallStateService::load(
            path.clone(),
            store.clone(),
            restarted_clock.clone(),
            restarted_clock.clone(),
        )
        .unwrap();
        assert_eq!(service.snapshot().remaining_minutes_now(), Some(3));
        assert!(service.snapshot().expiring_warning_sent);

        restarted_clock.advance(Duration::from_secs(179)).unwrap();
        assert_eq!(service.snapshot().remaining_minutes_now(), Some(0));
        drop(service);

        restarted_clock.advance(Duration::from_secs(60)).unwrap();
        let expired = HallStateService::load(
            path.clone(),
            store,
            restarted_clock.clone(),
            restarted_clock,
        )
        .unwrap();
        assert_eq!(expired.snapshot().remaining_minutes_now(), Some(0));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_hall_state_write_keeps_the_previous_runtime_state() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let blocker = std::env::temp_dir().join(format!(
            "miliastra-hall-write-blocker-{}-{suffix}",
            std::process::id()
        ));
        fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("hall-state.json");
        let clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 1_234));
        let state = PersistentHallState::load(path, crate::test_support::test_state_store())
            .expect("load hall state");
        let mut service = HallStateService::new_with_time(state, clock.clone(), clock.clone());

        let error = service
            .update_remaining_minutes(5)
            .expect_err("blocked parent must reject the state write");
        clock.advance(Duration::from_secs(61)).unwrap();

        assert!(error.to_string().contains("大厅状态目录"));
        assert_eq!(service.snapshot(), HallRuntimeState::default());
        fs::remove_file(blocker).unwrap();
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

    /// 只接受模板回复的替身。
    ///
    /// 走 reply(&format!(..)) 的老写法会把文案塞进 raw_replies，
    /// 于是"这条路径忘了模板化"能被测试抓出来，而不是悄悄回退成硬编码。
    #[derive(Default)]
    struct TemplateRecordingPort {
        read_hall_info: Option<HallObservation>,
        detected_hall: Option<HallObservation>,
        remaining_minutes: Option<u32>,
        idle: bool,
        warning_sent: bool,
        rendered: Vec<(&'static str, String)>,
        raw_replies: Vec<String>,
        toggled: bool,
        marked: bool,
    }

    impl TemplateRecordingPort {
        fn with_hall(info: HallObservation) -> Self {
            Self {
                read_hall_info: Some(info),
                idle: true,
                ..Self::default()
            }
        }

        fn rendered_keys(&self) -> Vec<&'static str> {
            self.rendered.iter().map(|(key, _)| *key).collect()
        }

        fn assert_only_templates_were_used(&self) {
            assert!(
                self.raw_replies.is_empty(),
                "这些文案绕过了 reply_template：{:?}",
                self.raw_replies
            );
        }
    }

    impl HallDetectionPort for TemplateRecordingPort {
        fn detect_public_hall(&mut self) -> Result<Option<HallObservation>> {
            Ok(self.detected_hall.clone())
        }

        fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
            self.remaining_minutes = Some(minutes);
            Ok(())
        }

        fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
            self.remaining_minutes = None;
            Ok(())
        }
    }

    impl HallApplicationPort for TemplateRecordingPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.raw_replies.push(message.to_string());
            Ok(())
        }

        fn reply_template(
            &mut self,
            spec: &crate::features::reply_template::ReplyTemplate,
            vars: &ReplyVars,
        ) -> Result<()> {
            let rendered = crate::features::reply_template::render(spec, None, vars);
            assert!(
                rendered.missing.is_empty(),
                "{} 渲染时缺少变量 {:?}",
                spec.key,
                rendered.missing
            );
            self.rendered.push((spec.key, rendered.text));
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
            self.read_hall_info
                .clone()
                .ok_or_else(|| anyhow::anyhow!("大厅信息读取失败：测试注入"))
        }

        fn toggle_microphone(&mut self) -> Result<()> {
            self.toggled = true;
            Ok(())
        }

        fn hall_remaining_minutes(&mut self) -> Result<Option<u32>> {
            Ok(self.remaining_minutes)
        }
    }

    impl HallMaintenancePort for TemplateRecordingPort {
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
            self.raw_replies.push(message.to_string());
            Ok(())
        }

        fn reply_template(
            &mut self,
            spec: &crate::features::reply_template::ReplyTemplate,
            vars: &ReplyVars,
        ) -> Result<()> {
            let rendered = crate::features::reply_template::render(spec, None, vars);
            self.rendered.push((spec.key, rendered.text));
            Ok(())
        }

        fn mark_hall_expiring_warning_sent(&mut self) -> Result<()> {
            self.marked = true;
            Ok(())
        }
    }

    fn hall_context() -> HallCommandContext {
        HallCommandContext {
            message_type: "blue".to_string(),
            username: "测试".to_string(),
            user_command: "@大厅检测".to_string(),
        }
    }

    #[test]
    fn hall_detection_replies_through_the_current_template_keys() {
        // 读取失败 → hall.detect.failed
        let mut failing = TemplateRecordingPort::default();
        HallApplication
            .execute(&hall_context(), &HallCommand::Detect, &mut failing)
            .expect("hall detect failure path");
        assert_eq!(failing.rendered_keys(), ["hall.detect.failed"]);
        assert_eq!(failing.rendered[0].1, "大厅检测失败");
        failing.assert_only_templates_were_used();

        // 公共大厅 → hall.public.current
        let mut public = TemplateRecordingPort::with_hall(HallObservation {
            name: "公共大厅".to_string(),
            remaining_minutes: None,
        });
        HallApplication
            .execute(&hall_context(), &HallCommand::Detect, &mut public)
            .expect("public hall detect");
        assert_eq!(public.rendered_keys(), ["hall.public.current"]);
        assert_eq!(public.rendered[0].1, "当前为公共大厅");
        public.assert_only_templates_were_used();

        // 具名大厅 + 剩余时间 → hall.current，后缀仍是"，剩余N分钟"
        let mut named = TemplateRecordingPort::with_hall(HallObservation {
            name: "一级大厅".to_string(),
            remaining_minutes: Some(7),
        });
        HallApplication
            .execute(&hall_context(), &HallCommand::Detect, &mut named)
            .expect("named hall detect");
        assert_eq!(named.rendered_keys(), ["hall.current"]);
        assert_eq!(named.rendered[0].1, "当前为一级大厅，剩余7分钟");
        assert_eq!(named.remaining_minutes, Some(7));
        named.assert_only_templates_were_used();

        // 无倒计时 → 不得留下孤立的逗号
        let mut no_timer = TemplateRecordingPort::with_hall(HallObservation {
            name: "未识别到大厅名称".to_string(),
            remaining_minutes: None,
        });
        HallApplication
            .execute(&hall_context(), &HallCommand::Detect, &mut no_timer)
            .expect("hall detect without countdown");
        assert_eq!(no_timer.rendered[0].1, "当前为未识别到大厅名称");
        no_timer.assert_only_templates_were_used();
    }

    #[test]
    fn hall_time_replies_through_the_current_template_keys() {
        // 已有倒计时 → hall.time.remaining
        let mut cached = TemplateRecordingPort {
            remaining_minutes: Some(3),
            idle: true,
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(&hall_context(), &HallCommand::Time, &mut cached)
            .expect("cached hall time");
        assert_eq!(cached.rendered_keys(), ["hall.time.remaining"]);
        assert_eq!(cached.rendered[0].1, "大厅到期时间，剩余3分钟");
        cached.assert_only_templates_were_used();

        // OCR 失败 → hall.time.unknown
        let mut failing = TemplateRecordingPort {
            idle: true,
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(&hall_context(), &HallCommand::Time, &mut failing)
            .expect("hall time read failure");
        assert_eq!(failing.rendered_keys(), ["hall.time.unknown"]);
        assert_eq!(failing.rendered[0].1, "大厅时间未知");
        failing.assert_only_templates_were_used();

        // 公共大厅 → hall.public.no_limit
        let mut public = TemplateRecordingPort {
            read_hall_info: Some(HallObservation {
                name: "公共大厅".to_string(),
                remaining_minutes: None,
            }),
            idle: true,
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(&hall_context(), &HallCommand::Time, &mut public)
            .expect("public hall time");
        assert_eq!(public.rendered_keys(), ["hall.public.no_limit"]);
        assert_eq!(public.rendered[0].1, "公共大厅无时间限制");
        public.assert_only_templates_were_used();

        // 识别出倒计时 → hall.time.remaining 并写回状态
        let mut detected = TemplateRecordingPort {
            read_hall_info: Some(HallObservation {
                name: "一级大厅".to_string(),
                remaining_minutes: Some(12),
            }),
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(&hall_context(), &HallCommand::Time, &mut detected)
            .expect("detected hall time");
        assert_eq!(detected.rendered_keys(), ["hall.time.remaining"]);
        assert_eq!(detected.rendered[0].1, "大厅到期时间，剩余12分钟");
        assert_eq!(detected.remaining_minutes, Some(12));
        detected.assert_only_templates_were_used();

        // 识别成功但没读到倒计时 → 仍回落到 hall.time.unknown
        let mut unknown = TemplateRecordingPort {
            read_hall_info: Some(HallObservation {
                name: "一级大厅".to_string(),
                remaining_minutes: None,
            }),
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(&hall_context(), &HallCommand::Time, &mut unknown)
            .expect("unknown hall time");
        assert_eq!(unknown.rendered_keys(), ["hall.time.unknown"]);
        unknown.assert_only_templates_were_used();
    }

    #[test]
    fn microphone_and_expiry_warnings_use_templates_too() {
        // 非公共大厅 → 切换麦克风并通告
        let mut toggle = TemplateRecordingPort {
            detected_hall: Some(HallObservation {
                name: "一级大厅".to_string(),
                remaining_minutes: None,
            }),
            ..TemplateRecordingPort::default()
        };
        HallApplication
            .execute(
                &hall_context(),
                &HallCommand::ToggleMicrophone {
                    username: "小明".to_string(),
                },
                &mut toggle,
            )
            .expect("microphone toggle");
        assert!(toggle.toggled);
        assert_eq!(toggle.rendered_keys(), ["hall.microphone.toggled"]);
        assert_eq!(toggle.rendered[0].1, "小明 执行了切换麦克风状态！");
        toggle.assert_only_templates_were_used();

        // 到期警告：>0 用带分钟的那条，==0 用不带分钟的
        let mut warn_minutes = TemplateRecordingPort {
            remaining_minutes: Some(4),
            idle: true,
            ..TemplateRecordingPort::default()
        };
        assert!(
            HallApplication
                .maybe_warn_expiring(&mut warn_minutes)
                .expect("expiry warning with minutes")
        );
        assert_eq!(
            warn_minutes.rendered_keys(),
            ["hall.expiring.warning_minutes"]
        );
        assert_eq!(warn_minutes.rendered[0].1, "大厅即将到期，剩余4分钟");
        warn_minutes.assert_only_templates_were_used();

        let mut warn_expired = TemplateRecordingPort {
            remaining_minutes: Some(0),
            idle: true,
            ..TemplateRecordingPort::default()
        };
        assert!(
            HallApplication
                .maybe_warn_expiring(&mut warn_expired)
                .expect("expiry warning without minutes")
        );
        assert_eq!(warn_expired.rendered_keys(), ["hall.expiring.warning"]);
        assert_eq!(warn_expired.rendered[0].1, "大厅即将到期");
        warn_expired.assert_only_templates_were_used();
    }

    #[test]
    fn every_key_used_by_hall_is_registered_in_the_catalog() {
        // 调用点用的字面量 key 必须与模板目录登记的一致，否则覆盖永远命中不到。
        let registered: std::collections::BTreeSet<&str> =
            crate::features::reply_template::catalog()
                .map(|spec| spec.key)
                .collect();
        for key in [
            "hall.detect.failed",
            "hall.public.current",
            "hall.current",
            "hall.time.remaining",
            "hall.time.unknown",
            "hall.public.no_limit",
            "hall.microphone.toggled",
            "hall.expiring.warning",
            "hall.expiring.warning_minutes",
        ] {
            assert!(registered.contains(key), "{key} 未登记进模板目录");
        }
    }
}
