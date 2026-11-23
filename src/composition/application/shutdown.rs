//! 关停**顺序**、关停**原因**与**有界等待**的单一事实源（架构重构 W1a 前置）。
//!
//! 本模块只做三件事，不做 supervisor 大改：
//!
//! 1. 把 [`ApplicationRuntime::run`] 里 19 步手写关停**原样搬进** [`ApplicationRuntime::shutdown_all`]，
//!    并给每一步打点（[`ShutdownReport::enter`]），使「顺序」第一次成为可断言的数据
//!    （[`SHUTDOWN_STEPS`]）而不是注释。
//! 2. 把两处**无界 `join()`**（延迟聊天发送线程、播放监控线程）换成
//!    [`join_worker_threads`]：超期只 warn + 记账，**不放弃其后的任何一步关停**。
//! 3. 把 4 态 [`ShutdownReason`] 的**按原因分叉的收尾逻辑**收成一张表（[`ShutdownReason::finale`]），
//!    新增变体时编译器会在那张表上直接报错。
//!
//! 默认路径（所有线程都在预算内退出）与本模块引入前**逐字节等价**：唯一的差别是
//! 线程池等待多了 `is_finished()` 轮询（`is_finished() == true` 时 `join()` 必定立即返回，
//! 所以结果与裸 `join()` 完全相同，包括 panic 载荷）。新增日志只出现在「超期」路径
//! （一条 warn）与 debug 级别（一行步数汇总）。

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::RunOutcome;
use crate::runtime::ui::panic_payload_text;

/// 关停时等待后台线程退出（join）的总预算。
///
/// 这是 W1a 唯一一处**行为变更**：超过预算仍不退出的线程会被 detach（丢弃句柄，
/// 线程继续跑到自己结束，进程不再为它陪跑），并记进 [`ShutdownReport`]。
/// 取 5 秒与 [`crate::interfaces::http::protocol`] 里 HTTP 面板的既有宽限期一致，
/// 不引入第二个「关停多久算久」的口径。
pub(in crate::composition) const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_secs(5);

/// 有界等待的轮询粒度。只影响「多久发现线程已退出」，不影响结果。
const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------------------
// 关停原因
// ---------------------------------------------------------------------------

/// 本次进程退出的**动机**。4 态，原子字编码（[`ShutdownState`]）。
///
/// # 4 态与「按 reason 分叉的收尾逻辑」全表
///
/// **关键事实：19 步关停序列对 4 态完全一致。** reason 只决定**收尾映射**，
/// 没有任何一步关停动作因为 reason 而被跳过、加重或重排（[`SHUTDOWN_STEPS`] 与
/// [`ShutdownReason`] 正交，见 `lifecycle_tests::shutdown_reason_distinguishes_config_reload_from_user_exit`）。
///
/// | reason | `RunOutcome`（退出码） | monitor 文案 | 收尾（`committed_run_result`）出错时 | 出错日志 |
/// |---|---|---|---|---|
/// | `Running` | `Stopped`(0) | 已退出 | **原样上抛**给看门狗（普通故障，该重启就重启） | 无（调用方自打） |
/// | `ConfigReload` | `Reload`(75) | 配置重载关停完成 | **保留 `Reload`**，只记 error | 「配置重载已提交，但关停收尾出错…」 |
/// | `ConfigReloadWithStartup` | `ReloadWithStartup`(77) | 配置重载关停完成 | **保留 `ReloadWithStartup`**，只记 error | 「配置重载已提交，但关停收尾出错…」 |
/// | `UserExit` | `Stopped`(0) | 已退出 | **保留 `Stopped`**，只记 error | 「用户退出已提交，但关停收尾出错…」 |
///
/// 为什么后三态要「吞掉」收尾错误：任务引擎停止接单之后进程替代已不可逆。
/// 把这类错误上抛会让看门狗把「用户明确退出」或「已提交的配置重载」误判成崩溃，
/// 于是重新拉起一个刚被停掉的进程——这正是「关停收尾」不能随便传播错误的理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(in crate::composition) enum ShutdownReason {
    Running = 0,
    ConfigReload = 1,
    ConfigReloadWithStartup = 2,
    UserExit = 3,
}

/// 一种关停原因的**收尾动作**（见 [`ShutdownReason`] 的分叉表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::composition) struct ShutdownFinale {
    /// 本次 `run()` 应向看门狗/父进程汇报的退出结论。
    pub outcome: RunOutcome,
    /// monitor 面板上显示的最终状态文案。
    pub status_label: &'static str,
    /// 收尾阶段（关停序列之后）出现错误时，是否仍然采纳 [`Self::outcome`]。
    ///
    /// `true` = 吞掉并记 error（进程替代已提交，不可逆）；
    /// `false` = 原样上抛，交给看门狗做错误恢复。
    pub survives_tail_error: bool,
    /// `survives_tail_error` 时使用的 error 日志前缀（含末尾的「：」）。
    pub tail_error_prefix: Option<&'static str>,
}

impl ShutdownReason {
    /// 4 态的穷尽清单：新增变体而忘了在这里登记，编译器会报「数组长度不符」。
    pub(in crate::composition) const ALL: [Self; 4] = [
        Self::Running,
        Self::ConfigReload,
        Self::ConfigReloadWithStartup,
        Self::UserExit,
    ];

    /// 原子字编码。显式 `as u8` 而不是 `mem::discriminant`，因为要落进 `AtomicU8`。
    pub(in crate::composition) const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由原子字还原；非法值立刻 panic（与引入本模块之前的行为一致，不静默兜底）。
    fn from_u8(value: u8) -> Self {
        for reason in Self::ALL {
            if reason.as_u8() == value {
                return reason;
            }
        }
        panic!("invalid shutdown reason value: {value}");
    }

    /// 本原因的收尾动作。**这是 4 态唯一的按原因分叉点**（表见类型文档）。
    pub(in crate::composition) const fn finale(self) -> ShutdownFinale {
        match self {
            Self::Running => ShutdownFinale {
                outcome: RunOutcome::Stopped,
                status_label: "已退出",
                survives_tail_error: false,
                tail_error_prefix: None,
            },
            Self::ConfigReload => ShutdownFinale {
                outcome: RunOutcome::Reload,
                status_label: "配置重载关停完成",
                survives_tail_error: true,
                tail_error_prefix: Some(
                    "配置重载已提交，但关停收尾出错（步骤：提交重载后关停各子系统），部分子系统可能没有干净退出：",
                ),
            },
            Self::ConfigReloadWithStartup => ShutdownFinale {
                outcome: RunOutcome::ReloadWithStartup,
                status_label: "配置重载关停完成",
                survives_tail_error: true,
                tail_error_prefix: Some(
                    "配置重载已提交，但关停收尾出错（步骤：提交重载后关停各子系统），部分子系统可能没有干净退出：",
                ),
            },
            Self::UserExit => ShutdownFinale {
                outcome: RunOutcome::Stopped,
                status_label: "已退出",
                survives_tail_error: true,
                tail_error_prefix: Some(
                    "用户退出已提交，但关停收尾出错（步骤：提交退出后关停各子系统），部分子系统可能没有干净退出：",
                ),
            },
        }
    }
}

/// 关停原因的原子状态机（配置重载认领 / 用户退出 / 重载后是否再跑启动自动化）。
///
/// 优先级的唯一出处在这里：**用户退出永远压过配置重载**，所以
/// [`Self::request_user_exit`] 用 `store`（无条件），而重载相关转换全部用
/// `compare_exchange`（只在「当前是 `Running`」/「当前是 `ConfigReload`」时生效）。
#[derive(Clone, Debug)]
pub(in crate::composition) struct ShutdownState {
    reason: Arc<AtomicU8>,
}

impl ShutdownState {
    pub(super) fn new() -> Self {
        Self {
            reason: Arc::new(AtomicU8::new(ShutdownReason::Running.as_u8())),
        }
    }

    pub(super) fn reason(&self) -> ShutdownReason {
        ShutdownReason::from_u8(self.reason.load(AtomicOrdering::SeqCst))
    }

    pub(super) fn try_claim_config_reload(&self) -> bool {
        self.reason
            .compare_exchange(
                ShutdownReason::Running.as_u8(),
                ShutdownReason::ConfigReload.as_u8(),
                AtomicOrdering::SeqCst,
                AtomicOrdering::SeqCst,
            )
            .is_ok()
    }

    pub(super) fn release_config_reload_claim(&self) {
        let _ = self.reason.compare_exchange(
            ShutdownReason::ConfigReload.as_u8(),
            ShutdownReason::Running.as_u8(),
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        );
    }

    pub(super) fn require_startup_after_reload(&self) {
        let _ = self.reason.compare_exchange(
            ShutdownReason::ConfigReload.as_u8(),
            ShutdownReason::ConfigReloadWithStartup.as_u8(),
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        );
    }

    pub(super) fn request_user_exit(&self) {
        self.reason
            .store(ShutdownReason::UserExit.as_u8(), AtomicOrdering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 关停顺序（golden 数据）
// ---------------------------------------------------------------------------

/// 关停序列里的一步。名字是**稳定标识**（golden 数据与日志共用），改名等于改 golden。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::composition) enum ShutdownStep {
    SharedChatObserver,
    ExternalChatWorkers,
    RunningFlag,
    TaskEngineWake,
    BackgroundLyrics,
    Hotkeys,
    HttpServer,
    DeferredChatSender,
    PlaybackMonitor,
    ModerationWorkers,
    FormalTaskRuntime,
    ListenerModePersist,
    BusinessRuntime,
    UiRuntime,
    OcrRuntime,
    PlayerRuntime,
    LoginHelper,
    NativePlayback,
    OpenAiRuntime,
}

impl ShutdownStep {
    /// golden 数据、日志与测试共用的稳定名字。
    pub(in crate::composition) const fn as_str(self) -> &'static str {
        match self {
            Self::SharedChatObserver => "shared-chat-observer",
            Self::ExternalChatWorkers => "external-chat",
            Self::RunningFlag => "running-flag",
            Self::TaskEngineWake => "task-engine-wake",
            Self::BackgroundLyrics => "background-lyrics",
            Self::Hotkeys => "hotkeys",
            Self::HttpServer => "http-server",
            Self::DeferredChatSender => "deferred-chat-sender",
            Self::PlaybackMonitor => "playback-monitor",
            Self::ModerationWorkers => "moderation-workers",
            Self::FormalTaskRuntime => "formal-task-runtime",
            Self::ListenerModePersist => "listener-mode-persist",
            Self::BusinessRuntime => "business-runtime",
            Self::UiRuntime => "ui-runtime",
            Self::OcrRuntime => "ocr-runtime",
            Self::PlayerRuntime => "player-runtime",
            Self::LoginHelper => "login-helper",
            Self::NativePlayback => "native-playback",
            Self::OpenAiRuntime => "openai-runtime",
        }
    }
}

impl ShutdownStep {
    /// 全部变体（**无序**集合，用于名字反查与穷尽自检；顺序只认 [`SHUTDOWN_STEPS`]）。
    ///
    /// 新增一个变体而忘了在这里登记，编译器会报「数组长度不符」。
    pub(in crate::composition) const ALL: [Self; 19] = [
        Self::BackgroundLyrics,
        Self::BusinessRuntime,
        Self::DeferredChatSender,
        Self::ExternalChatWorkers,
        Self::FormalTaskRuntime,
        Self::Hotkeys,
        Self::HttpServer,
        Self::ListenerModePersist,
        Self::LoginHelper,
        Self::ModerationWorkers,
        Self::NativePlayback,
        Self::OcrRuntime,
        Self::OpenAiRuntime,
        Self::PlaybackMonitor,
        Self::PlayerRuntime,
        Self::RunningFlag,
        Self::SharedChatObserver,
        Self::TaskEngineWake,
        Self::UiRuntime,
    ];

    /// 源码里的变体标识符（`ShutdownStep::UiRuntime` → `"UiRuntime"`）。
    ///
    /// 与 [`Self::as_str`] 刻意分开：`as_str` 是 golden 数据/日志里的短横线名（可以改），
    /// 这个是 Rust 标识符（改了就是编译错误）。
    pub(in crate::composition) const fn variant_name(self) -> &'static str {
        match self {
            Self::SharedChatObserver => "SharedChatObserver",
            Self::ExternalChatWorkers => "ExternalChatWorkers",
            Self::RunningFlag => "RunningFlag",
            Self::TaskEngineWake => "TaskEngineWake",
            Self::BackgroundLyrics => "BackgroundLyrics",
            Self::Hotkeys => "Hotkeys",
            Self::HttpServer => "HttpServer",
            Self::DeferredChatSender => "DeferredChatSender",
            Self::PlaybackMonitor => "PlaybackMonitor",
            Self::ModerationWorkers => "ModerationWorkers",
            Self::FormalTaskRuntime => "FormalTaskRuntime",
            Self::ListenerModePersist => "ListenerModePersist",
            Self::BusinessRuntime => "BusinessRuntime",
            Self::UiRuntime => "UiRuntime",
            Self::OcrRuntime => "OcrRuntime",
            Self::PlayerRuntime => "PlayerRuntime",
            Self::LoginHelper => "LoginHelper",
            Self::NativePlayback => "NativePlayback",
            Self::OpenAiRuntime => "OpenAiRuntime",
        }
    }

    pub(in crate::composition) fn from_variant_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|step| step.variant_name() == name)
    }
}

/// 源码里 `report.enter(ShutdownStep::X)` 的**出现顺序**——golden 测试的「实测」一侧。
///
/// 只认代码里的那一次调用；`lifecycle.rs` 的文档注释用 `[`ShutdownStep::X`]` 形式引用，
/// 不会命中。未知变体名直接 panic：说明有人加了步骤却没登记进 `ShutdownStep`。
pub(in crate::composition) fn recorded_shutdown_steps(source: &str) -> Vec<ShutdownStep> {
    const MARKER: &str = "report.enter(ShutdownStep::";
    source
        .match_indices(MARKER)
        .map(|(index, _)| &source[index + MARKER.len()..])
        .map(|rest| {
            rest.split(|ch: char| !ch.is_ascii_alphanumeric())
                .next()
                .unwrap_or_default()
        })
        .map(|name| {
            ShutdownStep::from_variant_name(name)
                .unwrap_or_else(|| panic!("未知的关停步骤变体 `{name}`"))
        })
        .collect()
}

/// 19 步关停顺序的**golden 数据**：`shutdown_all` 必须按这个顺序执行。
///
/// 初值 = 引入 supervisor 之前的真实顺序（`lifecycle.rs::run` 的 17 个组件停止 +
/// 2 个状态收尾，逐项人工核对）。改动顺序必须先改这里并说明理由 —— 这是 W1 之后
/// 所有关停重构的安全网。
///
/// # 顺序理由（不是随便排的）
///
/// **1. 生产者先于消费者停（本序列里唯一「先启动的先停」的一对）。**
/// `shared-chat-observer`（下标 0）必须先于 `external-chat`（下标 1）停稳：
/// 外部源线程的退出路径会 `set_active(false)`，如果观察线程那一刻还在跑，
/// 它会把「外部源不再接管」判成需要回落 OCR 扫描——关停期平白多一次截图与识别，
/// 日志里还会出现误导性的 OCR 噪音。先 join 观察线程，外部源状态就一直保持到
/// 没有读侧为止。注意它**先启动**（`run()` 开头就 subscribe + spawn），所以这一对
/// 是「先启动的先停」（LIFO），而不是「先启动的后停」。
///
/// **2. 全局开关先落，再停具体组件。** `running-flag` / `task-engine-wake` 在第 3、4 步：
/// 先把 `running = false` 并唤醒任务引擎，让各循环在**等待**时就看见「该走了」，
/// 后面每个 `shutdown()` 才是在「已经没人接新活」的前提下停线程。
///
/// **3. 两处裸线程 join 紧跟服务端关闭之后。** `deferred-chat-sender` /
/// `playback-monitor`（第 8、9 步）都是无外部宽限期的裸线程，且有界化之后超期
/// 也必须继续走完第 10–19 步（[`join_worker_threads`] 不 `break`）。
///
/// **4. 业务先于 UI/OCR/播放器，正式任务先于业务。** 正式任务运行时的线程持有
/// `AppServices`（`Arc`），业务运行时的期限处理依赖它自己还活着；UI/OCR 又是它们的
/// 下游执行器。反过来停会让下游在上游已死之后被调用一次。
///
/// **5. `openai-runtime` 最后。** 它的关停不改任何其它组件的状态，且它的
/// 失败文案是 warning 级；放在最后保证「真正常驻的东西先停干净」。
pub(in crate::composition) const SHUTDOWN_STEPS: &[ShutdownStep] = &[
    ShutdownStep::SharedChatObserver,
    ShutdownStep::ExternalChatWorkers,
    ShutdownStep::RunningFlag,
    ShutdownStep::TaskEngineWake,
    ShutdownStep::BackgroundLyrics,
    ShutdownStep::Hotkeys,
    ShutdownStep::HttpServer,
    ShutdownStep::DeferredChatSender,
    ShutdownStep::PlaybackMonitor,
    ShutdownStep::ModerationWorkers,
    ShutdownStep::FormalTaskRuntime,
    ShutdownStep::ListenerModePersist,
    ShutdownStep::BusinessRuntime,
    ShutdownStep::UiRuntime,
    ShutdownStep::OcrRuntime,
    ShutdownStep::PlayerRuntime,
    ShutdownStep::LoginHelper,
    ShutdownStep::NativePlayback,
    ShutdownStep::OpenAiRuntime,
];

// ---------------------------------------------------------------------------
// 有界等待
// ---------------------------------------------------------------------------

/// 一次有界 join 的两种失败。**超时不是错误**（继续关停其余服务），崩溃才是。
#[derive(Debug)]
enum JoinFailure {
    /// 预算内没退出：句柄被丢弃（线程继续跑，进程不再等它）。
    TimedOut,
    /// 线程因 panic 结束。
    Panicked(Box<dyn Any + Send>),
}

/// 一条**裸 `JoinHandle`** 的关停凭据：线程名 + 句柄 + 崩溃日志文案。
///
/// 之所以不让 `workers.rs` 直接返回它：`DeferredChatSender` / `PlaybackMonitorWorker`
/// 的 spawn 点是 `AppServices` 侧（`workers.rs:442` / `:969`），而等待点只有关停序列
/// 一处；把包装放在等待侧，spawn 侧保持「返回裸句柄」不变，diff 面最小。
pub(in crate::composition) struct WorkerThread {
    name: &'static str,
    handle: JoinHandle<()>,
    /// 线程崩溃时的日志前缀（含末尾的「：」）。
    panic_log: &'static str,
}

impl WorkerThread {
    pub(in crate::composition) fn new(
        name: &'static str,
        handle: JoinHandle<()>,
        panic_log: &'static str,
    ) -> Self {
        Self {
            name,
            handle,
            panic_log,
        }
    }

    /// 在 `deadline` 前等它退出。
    ///
    /// 返回 `true` = 已退出（含 panic）；`false` = 超期被 detach。
    /// 两条路径都不会让调用方无限阻塞：先 `is_finished()` 再 `join()`，
    /// 因此 `join()` 永不阻塞（Rust std 保证 `is_finished() == true` 后 `join()` 立即返回）。
    fn join_bounded(self, deadline: Instant, report: &mut ShutdownReport) -> bool {
        let WorkerThread {
            name,
            handle,
            panic_log,
        } = self;
        match join_within(handle, deadline) {
            Ok(()) => true,
            Err(JoinFailure::Panicked(payload)) => {
                // panic 载荷是判断「为什么崩」的唯一线索，不能丢；
                // 统一转成可读文本，与 monitor 投影线程的写法保持一致。
                let text = panic_payload_text(payload.as_ref());
                log::error!("{panic_log}{text}");
                report.panicked.push((name, text));
                true
            }
            Err(JoinFailure::TimedOut) => {
                // 超期只记账 + 一条 warn，**不影响**其余服务的关停。
                log::warn!(
                    "线程「{name}」未在关停预算（{} ms）内退出（步骤：关停时等待线程退出），已脱离管理；本次关停继续关闭其余服务，进程退出时该线程会被系统回收",
                    report.budget().as_millis()
                );
                report.detached.push(name);
                false
            }
        }
    }
}

/// 有界 join：`deadline` 前返回结果，否则 `TimedOut`。
fn join_within(handle: JoinHandle<()>, deadline: Instant) -> Result<(), JoinFailure> {
    while !handle.is_finished() {
        let now = Instant::now();
        if now >= deadline {
            return Err(JoinFailure::TimedOut);
        }
        thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(JOIN_POLL_INTERVAL),
        );
    }
    handle
        .join()
        .map_err(|payload| JoinFailure::Panicked(payload))
}

/// 按顺序等一组线程退出，预算**均分**给还没轮到的线程。
///
/// 均分（而不是每个都给满预算）是为了让第一条挂住的线程吃不下第二条的时间：
/// 剩 `n` 条时每条只拿「剩余预算 / n」。任何一条超期都**不会** `break`——
/// 这正是「不得丢下其它服务不关停」的落点。
pub(in crate::composition) fn join_worker_threads(
    workers: Vec<WorkerThread>,
    budget: Duration,
    report: &mut ShutdownReport,
) {
    // 预算的起点是**这次等待**，不含关停序列前半段的耗时。
    let started_at = Instant::now();
    let mut pending = workers.len();
    for worker in workers {
        let remaining = budget.saturating_sub(started_at.elapsed());
        let deadline = Instant::now() + remaining / pending.max(1) as u32;
        worker.join_bounded(deadline, report);
        pending = pending.saturating_sub(1);
    }
}

// ---------------------------------------------------------------------------
// 关停报告
// ---------------------------------------------------------------------------

/// 一次 [`super::ApplicationRuntime::shutdown_all`] 的结果。
///
/// 只记「关停时发生了什么看不见的事」，不替代各组件自己的 `*ShutdownReport`
/// （那些仍然由各自分支处理并打日志）。W1 的 supervisor 会把两者合并成一个，
/// 眼下先给「超期未退出的线程」一个明确的落点。
#[derive(Debug)]
pub(in crate::composition) struct ShutdownReport {
    budget: Duration,
    started_at: Instant,
    finished_after: Option<Duration>,
    /// 已经执行过的关停步数（应当恒等于 [`SHUTDOWN_STEPS`] 的长度）。
    steps: usize,
    /// 预算内没有退出的线程名（按出现顺序）。
    detached: Vec<&'static str>,
    /// 崩溃的线程名 + panic 载荷文本。
    panicked: Vec<(&'static str, String)>,
}

impl ShutdownReport {
    pub(in crate::composition) fn new(budget: Duration) -> Self {
        Self {
            budget,
            started_at: Instant::now(),
            finished_after: None,
            steps: 0,
            detached: Vec::new(),
            panicked: Vec::new(),
        }
    }

    /// 打点一步。顺序 golden 测试以源码里这些调用的先后为准（见 `lifecycle_tests.rs`）。
    pub(in crate::composition) fn enter(&mut self, _step: ShutdownStep) {
        self.steps += 1;
    }

    /// 关停序列结束时收口：此后 `elapsed()` 返回**整段关停**的耗时（冻住）。
    pub(in crate::composition) fn finish(&mut self) {
        self.finished_after = Some(self.started_at.elapsed());
    }

    pub(in crate::composition) const fn budget(&self) -> Duration {
        self.budget
    }

    pub(in crate::composition) fn elapsed(&self) -> Duration {
        self.finished_after
            .unwrap_or_else(|| self.started_at.elapsed())
    }

    pub(in crate::composition) const fn steps(&self) -> usize {
        self.steps
    }

    /// 预算内没有退出的线程名。空 = 所有线程都干净退出（默认路径）。
    pub(in crate::composition) fn detached(&self) -> &[&'static str] {
        &self.detached
    }

    pub(in crate::composition) const fn timed_out(&self) -> bool {
        !self.detached.is_empty()
    }

    /// 崩溃线程的名字与载荷文本。
    pub(in crate::composition) fn panicked(&self) -> &[(&'static str, String)] {
        &self.panicked
    }
}

/// `shutdown_all` 需要被等待的两条裸线程句柄。
///
/// 单独成结构而不是塞进 `OwnedRuntimes`：这两个句柄今天由 `run()` 的局部变量持有
/// （启动在序列前段、等待在序列中段），放进结构只为了让 `shutdown_all` 能拿到它们，
/// 不改变所有权归属，也不改 spawn 侧（`workers.rs`）的签名。
#[derive(Default)]
pub(in crate::composition::application) struct ShutdownHandles {
    /// `shared-chat-observer` 的 `Drop` 会 join 它自己的线程（原第 1 步）。
    pub shared_chat_observer: Option<super::shared_observer::SharedChatObserverGuard>,
    /// 延迟聊天发送线程（原第 8 步）。
    pub deferred_chat_sender: Option<WorkerThread>,
    /// 播放监控线程（原第 9 步）。
    pub playback_monitor: Option<WorkerThread>,
}
