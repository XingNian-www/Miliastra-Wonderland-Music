//! W1a 前置的行为契约测试：关停顺序 golden、4 态收尾表、两处有界 join。
//!
//! 单独成文件的两个理由：
//!
//! - `lifecycle.rs` 有**行数预算棘轮**（`scripts/layer-baseline.json`）：把关停序列
//!   搬进去已经用掉预算，再塞 300 行测试会把预算顶穿。
//! - 以 `*_tests.rs` 命名的文件不进分层闸门的扫描范围（`scripts/check-layers.py`
//!   的 `is_excluded_source`）：测试代码不该产生生产依赖边。
//!
//! 由 `lifecycle.rs` 里的 `#[path] mod lifecycle_tests;` 挂在 `lifecycle` 模块下，
//! 因此 `super` 是关停模块本身，`super::super` 是 `composition::application`。

use super::super::ShutdownReason;
use super::super::shutdown::{
    SHUTDOWN_STEPS, ShutdownReport, ShutdownStep, WorkerThread, join_worker_threads,
    recorded_shutdown_steps,
};
use super::super::{ReloadStartupActions, ReloadStartupGate, ShutdownState};
use super::committed_run_result;
use super::should_run_startup_automation;
use crate::RunOutcome;
use crate::features::startup::StartupTaskKind;
use anyhow::anyhow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// golden 数据必须恰好 19 步，且没有重复项（重复项会让「顺序」失去意义）。
#[test]
fn shutdown_golden_data_is_a_total_order_over_nineteen_steps() {
    assert_eq!(SHUTDOWN_STEPS.len(), 19);
    let names = SHUTDOWN_STEPS
        .iter()
        .map(|step| step.as_str())
        .collect::<Vec<_>>();
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "关停步骤里有重复项：{names:?}");
}

/// **核心 golden test**：`shutdown_all` 的**真实执行顺序** == [`SHUTDOWN_STEPS`]。
///
/// 「真实执行顺序」来自 [`recorded_shutdown_steps`] 扫描 `shutdown_all` 源码里那些埋点
/// 调用（埋点不改变生产控制流）：每个埋点与它对应的关停动作**相邻**，因此调换任意两行
/// （等价于调换两步关停）都会让本测试变红。
///
/// 为什么扫源码而不是运行期记录：`shutdown_all` 需要真实构造 19 个运行时（UI 设备、
/// OCR 推理线程、HTTP 面板……）才能跑一遍，单测里起不来；而这条断言要防的恰恰是
/// 「有人手改顺序却忘了改 golden」——源码扫描对这类改动最敏感，也不会引入进程级
/// 全局状态（见 `listener/display.rs` 对 static Mutex 的教训）。
#[test]
fn shutdown_order_matches_the_golden_sequence() {
    let source = include_str!("lifecycle.rs");
    let recorded = recorded_shutdown_steps(source);
    assert_eq!(
        recorded,
        SHUTDOWN_STEPS.to_vec(),
        "shutdown_all 的执行顺序与 golden 数据不一致"
    );
    // golden 数据里每一步都必须真的被埋点，否则「顺序一致」是靠漏项对齐的。
    for step in SHUTDOWN_STEPS {
        assert!(
            recorded.contains(step),
            "golden 步骤 {step:?} 在 shutdown_all 里没有埋点"
        );
    }
}

/// 生产者先于消费者：`shared-chat-observer` 必须先于 `external-chat` 停，
/// 尽管它**先启动**（这是「先启动的先停」，LIFO）。
#[test]
fn shared_chat_observer_stops_before_external_chat() {
    let position = |step: ShutdownStep| {
        SHUTDOWN_STEPS
            .iter()
            .position(|candidate| *candidate == step)
            .expect("golden 数据缺少步骤")
    };
    assert!(
        position(ShutdownStep::SharedChatObserver) < position(ShutdownStep::ExternalChatWorkers),
        "观察线程必须先于外部源停：外部源线程退出时会 set_active(false)，\
既有的观察线程会把它误判成「回落 OCR」而在关停期多扫一次"
    );
}

/// 4 态的收尾动作必须两两可分：`ConfigReload` 与 `UserExit` 走**不同**路径。
///
/// 具体到每一步：`ConfigReload` 收尾错误被吞掉且保留 `Reload`（看门狗据此重启子进程），
/// `UserExit` 收尾错误被吞掉但保留 `Stopped`（看门狗**不得**重启刚被用户停掉的进程），
/// 而 `Running` 的收尾错误**上抛**（普通故障，该由看门狗做错误恢复）。
#[test]
fn shutdown_reason_distinguishes_config_reload_from_user_exit() {
    // 收尾出错时的结论：三者必须可区分。
    let reload = committed_run_result(Err(anyhow!("tail failed")), ShutdownReason::ConfigReload)
        .expect("ConfigReload 的收尾错误必须被吞掉");
    let user_exit = committed_run_result(Err(anyhow!("tail failed")), ShutdownReason::UserExit)
        .expect("UserExit 的收尾错误必须被吞掉");
    assert_eq!(reload, RunOutcome::Reload);
    assert_eq!(user_exit, RunOutcome::Stopped);
    assert_ne!(reload, user_exit);
    assert_ne!(reload.exit_code(), user_exit.exit_code());
    // `Running` 相反：错误照常上抛。
    assert!(committed_run_result(Err(anyhow!("tail failed")), ShutdownReason::Running).is_err());

    // 收尾成功时的结论与 exit code：4 态逐项。
    assert_eq!(
        ShutdownReason::Running.finale().outcome,
        RunOutcome::Stopped
    );
    assert_eq!(
        ShutdownReason::ConfigReload.finale().outcome,
        RunOutcome::Reload
    );
    assert_eq!(
        ShutdownReason::ConfigReloadWithStartup.finale().outcome,
        RunOutcome::ReloadWithStartup
    );
    assert_eq!(
        ShutdownReason::UserExit.finale().outcome,
        RunOutcome::Stopped
    );
    assert_eq!(
        ShutdownReason::ConfigReload.finale().outcome.exit_code(),
        75
    );
    assert_eq!(
        ShutdownReason::ConfigReloadWithStartup
            .finale()
            .outcome
            .exit_code(),
        77
    );

    // 面板文案：两种配置重载共用一个文案，运行期/用户退出共用一个。
    assert_eq!(
        ShutdownReason::ConfigReload.finale().status_label,
        ShutdownReason::ConfigReloadWithStartup
            .finale()
            .status_label
    );
    assert_eq!(
        ShutdownReason::Running.finale().status_label,
        ShutdownReason::UserExit.finale().status_label
    );
    assert_ne!(
        ShutdownReason::ConfigReload.finale().status_label,
        ShutdownReason::UserExit.finale().status_label
    );

    // `survives_tail_error` 是上面那条「吞/抛」判断的数据来源，必须与之同构。
    assert!(!ShutdownReason::Running.finale().survives_tail_error);
    for reason in [
        ShutdownReason::ConfigReload,
        ShutdownReason::ConfigReloadWithStartup,
        ShutdownReason::UserExit,
    ] {
        assert!(reason.finale().survives_tail_error, "{reason:?}");
        assert!(reason.finale().tail_error_prefix.is_some(), "{reason:?}");
    }
    assert!(ShutdownReason::Running.finale().tail_error_prefix.is_none());
}

// ---------------------------------------------------------------------------
// 两处无界 join 的有界化（W1a 唯一的行为变更）
// ---------------------------------------------------------------------------

/// 故意不返回的假服务：用 `park()` 卡住，直到进程结束。
///
/// 不能用 `sleep(很长)`：那会在慢机器上和预算抢时间，让测试变成睡眠竞速。
fn spawn_stuck(name: &'static str) -> WorkerThread {
    WorkerThread::new(
        name,
        std::thread::spawn(|| std::thread::park()),
        "假服务崩溃（步骤：关停时等待线程退出）：",
    )
}

/// 立刻返回的假服务，并在退出瞬间置位 `done`。
fn spawn_quick(name: &'static str, done: &Arc<AtomicBool>) -> WorkerThread {
    let done = Arc::clone(done);
    WorkerThread::new(
        name,
        std::thread::spawn(move || done.store(true, Ordering::SeqCst)),
        "假服务崩溃（步骤：关停时等待线程退出）：",
    )
}

/// 超期线程必须被记账进 `ShutdownReport`，且**不得**让等待无限阻塞。
#[test]
fn bounded_join_reports_a_thread_that_overstays_deadline() {
    let budget = Duration::from_millis(200);
    let mut report = ShutdownReport::new(budget);
    let started = Instant::now();
    join_worker_threads(vec![spawn_stuck("slow-service")], budget, &mut report);
    let elapsed = started.elapsed();

    assert!(report.timed_out(), "超期线程必须让 report 判定为 timed_out");
    assert_eq!(
        report.detached(),
        &["slow-service"],
        "超期线程名必须进 ShutdownReport（供 W1 的 supervisor 合并成一个 report）"
    );
    assert!(
        elapsed >= budget,
        "不能比预算更早判定超期（实际 {elapsed:?} < {budget:?}）"
    );
    assert!(
        elapsed < budget + Duration::from_secs(2),
        "有界 join 必须在预算附近返回，实际耗时 {elapsed:?}"
    );
    assert!(
        report.panicked().is_empty(),
        "超期不是崩溃，不能记进 panicked"
    );
}

/// 一条线程挂死**不得**让其余服务被跳过——这是「超期只 warn，继续关停」的落点。
#[test]
fn bounded_join_never_abandons_the_remaining_services() {
    let budget = Duration::from_millis(400);
    let mut report = ShutdownReport::new(budget);
    let second_done = Arc::new(AtomicBool::new(false));
    let third_done = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    join_worker_threads(
        vec![
            spawn_stuck("stuck-service"),
            spawn_quick("second-service", &second_done),
            spawn_quick("third-service", &third_done),
        ],
        budget,
        &mut report,
    );
    let elapsed = started.elapsed();

    // 只有挂死的那条被 detach；排在它后面的两条照常等到退出。
    assert_eq!(
        report.detached(),
        &["stuck-service"],
        "挂死线程之后的服务不得被放弃"
    );
    assert!(
        second_done.load(Ordering::SeqCst),
        "第二条服务必须已经被等到"
    );
    assert!(
        third_done.load(Ordering::SeqCst),
        "第三条服务必须已经被等到（挂死线程不能中断循环）"
    );
    assert!(
        elapsed < budget + Duration::from_secs(2),
        "整组等待必须受预算约束，实际耗时 {elapsed:?}"
    );
}

#[test]
fn config_reload_child_skips_cold_start_only_with_a_reusable_window() {
    assert!(should_run_startup_automation(false, false, false));
    assert!(should_run_startup_automation(false, false, true));
    assert!(should_run_startup_automation(true, false, false));
    assert!(!should_run_startup_automation(true, false, true));
    assert!(should_run_startup_automation(true, true, true));
}

#[test]
fn reload_startup_gate_requires_every_requested_action_to_succeed() {
    let gate = ReloadStartupGate::default();
    let required = ReloadStartupActions::from_flags(true, true, true);
    gate.require(required);

    assert_eq!(gate.required_actions(), required);
    assert_eq!(gate.missing_actions(), required);
    assert!(!gate.is_satisfied());

    gate.record_success(StartupTaskKind::StartGame);
    assert!(gate.missing_actions().includes_enter_wonderland());
    assert!(!gate.missing_actions().includes_start_game());
    assert!(!gate.is_satisfied());

    gate.record_success(StartupTaskKind::EnterWonderland);
    assert!(gate.is_satisfied());
}

#[test]
fn disabled_startup_has_no_reload_ready_gate() {
    let gate = ReloadStartupGate::default();
    let actions = ReloadStartupActions::from_flags(false, true, true);
    assert!(actions.is_empty());
    gate.require(actions);
    assert!(gate.is_satisfied());
}

#[test]
fn automatic_entry_is_not_unconditionally_queued_at_startup() {
    let mut config = crate::config::AppConfig::default();
    config.startup.enabled = true;
    config.startup.enter_game = true;
    config.startup.enter_wonderland = true;
    config.startup.launch_game = false;
    assert!(ReloadStartupActions::from_startup_config(&config).is_empty());
    config.startup.launch_game = true;
    let actions = ReloadStartupActions::from_startup_config(&config);
    assert!(actions.includes_start_game());
    assert!(!actions.includes_enter_wonderland());
}

#[test]
fn startup_success_before_a_reload_requirement_does_not_acknowledge_it() {
    let gate = ReloadStartupGate::default();
    gate.record_success(StartupTaskKind::StartGame);
    gate.require(ReloadStartupActions::from_flags(true, true, false));

    assert!(!gate.is_satisfied());
    assert!(gate.missing_actions().includes_start_game());
}

#[test]
fn user_exit_wins_before_or_after_a_reload_claim() {
    let exit_first = ShutdownState::new();
    exit_first.request_user_exit();
    assert!(!exit_first.try_claim_config_reload());
    assert_eq!(exit_first.reason(), ShutdownReason::UserExit);

    let reload_first = ShutdownState::new();
    assert!(reload_first.try_claim_config_reload());
    reload_first.request_user_exit();
    reload_first.release_config_reload_claim();
    assert_eq!(reload_first.reason(), ShutdownReason::UserExit);
}

#[test]
fn an_abandoned_reload_claim_can_be_retried() {
    let shutdown = ShutdownState::new();
    assert!(shutdown.try_claim_config_reload());
    shutdown.release_config_reload_claim();
    assert_eq!(shutdown.reason(), ShutdownReason::Running);
    assert!(shutdown.try_claim_config_reload());
    assert_eq!(shutdown.reason(), ShutdownReason::ConfigReload);
}

#[test]
fn committed_reload_can_require_startup_without_overriding_user_exit() {
    let shutdown = ShutdownState::new();
    assert!(shutdown.try_claim_config_reload());
    shutdown.require_startup_after_reload();
    assert_eq!(shutdown.reason(), ShutdownReason::ConfigReloadWithStartup);
    assert_eq!(
        committed_run_result(Ok(()), shutdown.reason()).unwrap(),
        RunOutcome::ReloadWithStartup
    );

    shutdown.request_user_exit();
    assert_eq!(shutdown.reason(), ShutdownReason::UserExit);
}

#[test]
fn committed_reload_keeps_its_exit_outcome_after_a_tail_error() {
    assert_eq!(
        committed_run_result(
            Err(anyhow!("final forward failed")),
            ShutdownReason::ConfigReload,
        )
        .unwrap(),
        RunOutcome::Reload
    );
    assert!(
        committed_run_result(Err(anyhow!("ordinary failure")), ShutdownReason::Running).is_err()
    );
}

#[test]
fn committed_user_exit_ignores_a_concurrent_tail_error() {
    assert_eq!(
        committed_run_result(
            Err(anyhow!("final forward failed")),
            ShutdownReason::UserExit,
        )
        .unwrap(),
        RunOutcome::Stopped
    );
}
