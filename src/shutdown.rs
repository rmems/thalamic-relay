//! Fail-closed orderly shutdown and restart-recovery policy.
//!
//! SIGINT/SIGTERM must not restore the device default power limit just because
//! the process is exiting. A persistent brake is released only through the same
//! hysteresis used during normal operation ([`crate::safety::SafetyMachine`]).
//!
//! This module is pure decision + bounded joins: no NVML, no signal handlers.

use crate::safety::SafetySnapshot;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Matches `nvidia-smi` `timeout -k 2 5s` plus a small grace period.
pub const SHUTDOWN_ACTUATOR_TIMEOUT: Duration = Duration::from_secs(8);
/// Metrics collector is interrupted via watch; this is the max wait before abort.
pub const SHUTDOWN_METRICS_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the supervisor is leaving the run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Sigint,
    Sigterm,
}

impl ShutdownReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sigint => "sigint",
            Self::Sigterm => "sigterm",
        }
    }
}

/// In-flight privileged actuation, if any, when the signal arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightActuation {
    None,
    Apply,
    Release,
}

impl InFlightActuation {
    #[must_use]
    pub const fn from_tasks(apply: bool, release: bool) -> Self {
        match (apply, release) {
            (true, _) => Self::Apply,
            (false, true) => Self::Release,
            (false, false) => Self::None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Apply => "apply",
            Self::Release => "release",
        }
    }
}

/// What to do with already-dispatched actuation. Never includes "dispatch new".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownActuation {
    /// No in-flight work. Do not start apply or release on the way out.
    Idle,
    /// Wait (bounded) for an in-flight apply. Completing it is fail-closed-friendly.
    AwaitApply,
    /// Wait (bounded) for a release that was already policy-authorized in-loop.
    AwaitAuthorizedRelease,
}

impl ShutdownActuation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AwaitApply => "await_apply",
            Self::AwaitAuthorizedRelease => "await_authorized_release",
        }
    }
}

/// Observable shutdown decision. The type system has no "restore default PL" action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownPlan {
    pub reason: ShutdownReason,
    pub actuation: ShutdownActuation,
    pub leave_brake_engaged: bool,
    pub unresolved_brake: bool,
    pub unresolved_actuator: bool,
    pub summary: &'static str,
}

/// Decide shutdown behavior from the current snapshot and in-flight tasks.
///
/// Invariants:
/// - Never dispatches a new apply or release.
/// - An undispatched [`crate::safety::BrakeIntent::Release`] is abandoned;
///   the brake stays on and restart re-runs hysteresis.
/// - Simulated/missing/stale/critical snapshots cannot authorize a new release.
#[must_use]
pub fn plan_shutdown(
    reason: ShutdownReason,
    snap: &SafetySnapshot,
    in_flight: InFlightActuation,
) -> ShutdownPlan {
    let actuation = match in_flight {
        InFlightActuation::Apply => ShutdownActuation::AwaitApply,
        InFlightActuation::Release => ShutdownActuation::AwaitAuthorizedRelease,
        InFlightActuation::None => ShutdownActuation::Idle,
    };
    let leave_brake_engaged = snap.brake_engaged || matches!(in_flight, InFlightActuation::Apply);
    let unresolved_brake =
        snap.brake_engaged || snap.desired_brake || matches!(in_flight, InFlightActuation::Apply);
    let unresolved_actuator = snap.last_actuator_error.is_some() || snap.actuator_failed;
    ShutdownPlan {
        reason,
        actuation,
        leave_brake_engaged,
        unresolved_brake,
        unresolved_actuator,
        summary: shutdown_summary(snap, in_flight),
    }
}

fn shutdown_summary(snap: &SafetySnapshot, in_flight: InFlightActuation) -> &'static str {
    use crate::safety::SafetyState;
    if matches!(in_flight, InFlightActuation::Apply) {
        return "await in-flight apply; do not restore default power limit";
    }
    if matches!(in_flight, InFlightActuation::Release) {
        return "await already-authorized in-flight release; no new actuation";
    }
    if snap.last_actuator_error.is_some() || snap.actuator_failed {
        return "actuator failed; do not retry on the way out; leave unresolved for restart";
    }
    match snap.state {
        SafetyState::HealthyReal if !snap.brake_engaged => {
            "healthy; leave hardware unchanged; release process lock"
        }
        SafetyState::Warning if !snap.brake_engaged => {
            "warned, brake not engaged; do not apply or restore on exit"
        }
        SafetyState::Warning => "warned with brake held; leave brake engaged",
        SafetyState::CriticalBraked if snap.brake_engaged => {
            "critical, brake active; leave brake engaged"
        }
        SafetyState::CriticalBraked => {
            "critical, brake requested but not yet applied; do not restore; next start will apply"
        }
        SafetyState::Recovering => {
            "recovering; abandon undispatched release; leave brake engaged for hysteresis on restart"
        }
        SafetyState::TelemetryMissing
        | SafetyState::TelemetryStale
        | SafetyState::TelemetryInvalid => {
            "telemetry unverified; leave hardware unchanged (fail closed)"
        }
        SafetyState::SimulatedSoftwareOnly if snap.brake_engaged => {
            "simulated telemetry cannot authorize release of a real brake; leave engaged"
        }
        SafetyState::SimulatedSoftwareOnly => "software-only; leave hardware unchanged",
        SafetyState::ActuatorFailure => {
            "actuator failed; do not retry on the way out; leave unresolved for restart"
        }
        SafetyState::HealthyReal => "brake still claimed; leave engaged; no restore on exit",
    }
}

/// Result of waiting on an in-flight apply/release during shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InFlightJoin {
    Succeeded,
    Failed(String),
    Panicked(String),
    TimedOut,
}

/// Await a blocking actuation task, or give up after `timeout`.
///
/// On timeout the task is aborted (async work) / detached (`spawn_blocking`
/// cannot be cancelled). Shutdown does not hang indefinitely.
pub async fn join_in_flight<E: std::fmt::Display>(
    task: JoinHandle<Result<(), E>>,
    timeout: Duration,
) -> InFlightJoin {
    let abort = task.abort_handle();
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(Ok(()))) => InFlightJoin::Succeeded,
        Ok(Ok(Err(e))) => InFlightJoin::Failed(e.to_string()),
        Ok(Err(e)) => InFlightJoin::Panicked(e.to_string()),
        Err(_) => {
            abort.abort();
            InFlightJoin::TimedOut
        }
    }
}

/// Signal the metrics collector and abort it if it does not exit within `timeout`.
pub async fn shutdown_metrics_collector(
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    metrics_task: JoinHandle<()>,
    timeout: Duration,
) {
    let _ = shutdown_tx.send(true);
    let abort_handle = metrics_task.abort_handle();
    match tokio::time::timeout(timeout, metrics_task).await {
        Ok(_) => {}
        Err(_) => abort_handle.abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{
        ActuatorOutcome, BRAKE_FRACTION, BrakeIntent, SafetyMachine, SafetyState,
        classify_power_limit,
    };
    use crate::telemetry::{assess, fixtures};

    fn nvml(temp_c: f32, power_w: f32) -> crate::telemetry::TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(temp_c);
        raw.power_w = Some(power_w);
        assess(&raw, fixtures::NOW)
    }

    fn snap_healthy() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&nvml(65.0, 200.0))
    }

    fn snap_warned() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&nvml(78.0, 200.0))
    }

    fn snap_brake_requested() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&nvml(90.0, 200.0))
    }

    fn snap_brake_active() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        let _ = m.evaluate(&nvml(90.0, 200.0));
        m.record_actuator(ActuatorOutcome::Applied)
    }

    fn snap_recovering() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        let _ = m.evaluate(&nvml(90.0, 200.0));
        let _ = m.record_actuator(ActuatorOutcome::Applied);
        m.evaluate(&nvml(65.0, 200.0))
    }

    fn snap_release_pending() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        let _ = m.evaluate(&nvml(90.0, 200.0));
        let _ = m.record_actuator(ActuatorOutcome::Applied);
        let ok = nvml(65.0, 200.0);
        let _ = m.evaluate(&ok);
        let _ = m.evaluate(&ok);
        m.evaluate(&ok)
    }

    fn snap_actuator_failed() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        let _ = m.evaluate(&nvml(90.0, 200.0));
        m.record_actuator(ActuatorOutcome::ApplyFailed("nvidia-smi -pl failed".into()))
    }

    fn snap_missing() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&assess(&fixtures::sensor_dropout(), fixtures::NOW))
    }

    fn snap_stale() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&assess(&fixtures::stale(), fixtures::NOW))
    }

    fn snap_invalid() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&assess(&fixtures::out_of_range(), fixtures::NOW))
    }

    fn snap_sim() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.evaluate(&assess(&fixtures::software_fallback(), fixtures::NOW))
    }

    fn snap_sim_braked() -> SafetySnapshot {
        let mut m = SafetyMachine::new();
        m.seed_brake_applied();
        m.evaluate(&assess(&fixtures::software_fallback(), fixtures::NOW))
    }

    fn assert_no_new_dispatch(plan: &ShutdownPlan) {
        assert!(
            matches!(
                plan.actuation,
                ShutdownActuation::Idle
                    | ShutdownActuation::AwaitApply
                    | ShutdownActuation::AwaitAuthorizedRelease
            ),
            "shutdown must not dispatch a new command, got {:?}",
            plan.actuation
        );
        assert_ne!(plan.actuation.as_str(), "dispatch_release");
        assert_ne!(plan.actuation.as_str(), "dispatch_apply");
    }

    #[test]
    fn healthy_idle_is_clean() {
        let plan = plan_shutdown(
            ShutdownReason::Sigint,
            &snap_healthy(),
            InFlightActuation::None,
        );
        assert_eq!(plan.reason.as_str(), "sigint");
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(!plan.leave_brake_engaged);
        assert!(!plan.unresolved_brake);
        assert!(!plan.unresolved_actuator);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn warned_without_brake_does_not_apply_on_exit() {
        let snap = snap_warned();
        assert_eq!(snap.state, SafetyState::Warning);
        assert_eq!(snap.intent, BrakeIntent::None);
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.reason.as_str(), "sigterm");
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(!plan.leave_brake_engaged);
        assert!(!plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn brake_requested_does_not_restore_and_does_not_dispatch() {
        let snap = snap_brake_requested();
        assert_eq!(snap.intent, BrakeIntent::Apply);
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(!plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn in_flight_apply_is_awaited_not_released() {
        let snap = snap_brake_requested();
        let plan = plan_shutdown(ShutdownReason::Sigint, &snap, InFlightActuation::Apply);
        assert_eq!(plan.actuation, ShutdownActuation::AwaitApply);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn brake_active_leaves_hardware_throttled() {
        let snap = snap_brake_active();
        assert!(snap.brake_engaged);
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn recovering_abandons_undispatched_release() {
        let snap = snap_recovering();
        assert_eq!(snap.state, SafetyState::Recovering);
        assert_eq!(snap.intent, BrakeIntent::None);
        let plan = plan_shutdown(ShutdownReason::Sigint, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn release_pending_not_in_flight_does_not_restore() {
        let snap = snap_release_pending();
        assert_eq!(snap.intent, BrakeIntent::Release);
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn already_authorized_in_flight_release_may_complete() {
        let snap = snap_release_pending();
        assert_eq!(snap.intent, BrakeIntent::Release);
        let plan = plan_shutdown(ShutdownReason::Sigint, &snap, InFlightActuation::Release);
        assert_eq!(plan.actuation, ShutdownActuation::AwaitAuthorizedRelease);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn actuator_failure_does_not_retry_on_exit() {
        let snap = snap_actuator_failed();
        assert_eq!(snap.state, SafetyState::ActuatorFailure);
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.unresolved_actuator);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn missing_stale_invalid_fail_closed() {
        for snap in [snap_missing(), snap_stale(), snap_invalid()] {
            let plan = plan_shutdown(ShutdownReason::Sigint, &snap, InFlightActuation::None);
            assert_eq!(plan.actuation, ShutdownActuation::Idle);
            assert!(plan.unresolved_brake);
            assert_no_new_dispatch(&plan);
        }
    }

    #[test]
    fn simulated_without_brake_leaves_hardware() {
        let snap = snap_sim();
        let plan = plan_shutdown(ShutdownReason::Sigterm, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(!plan.leave_brake_engaged);
        assert!(!plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn simulated_cannot_authorize_release_of_real_brake() {
        let snap = snap_sim_braked();
        assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
        assert_eq!(snap.intent, BrakeIntent::None);
        assert!(snap.brake_engaged);
        let plan = plan_shutdown(ShutdownReason::Sigint, &snap, InFlightActuation::None);
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert_no_new_dispatch(&plan);
    }

    #[test]
    fn both_apply_and_release_flags_prefer_apply() {
        assert_eq!(
            InFlightActuation::from_tasks(true, true),
            InFlightActuation::Apply
        );
        assert_eq!(
            InFlightActuation::from_tasks(false, false),
            InFlightActuation::None
        );
    }

    #[test]
    fn foreign_cap_is_not_seeded() {
        let obs = classify_power_limit(Some(220), Some(300), BRAKE_FRACTION);
        assert!(!obs.should_seed_leftover_brake());
        assert_eq!(obs.as_str(), "foreign_sub_default_cap");
    }

    #[tokio::test]
    async fn join_in_flight_times_out_instead_of_hanging() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<(), String>(())
        });
        let result = join_in_flight(handle, Duration::from_millis(25)).await;
        assert_eq!(result, InFlightJoin::TimedOut);
    }

    #[tokio::test]
    async fn join_in_flight_records_success() {
        let handle = tokio::spawn(async { Ok::<(), String>(()) });
        let result = join_in_flight(handle, Duration::from_secs(1)).await;
        assert_eq!(result, InFlightJoin::Succeeded);
    }

    #[tokio::test]
    async fn join_in_flight_records_failure() {
        let handle = tokio::spawn(async { Err::<(), _>("boom".to_string()) });
        let result = join_in_flight(handle, Duration::from_secs(1)).await;
        assert_eq!(result, InFlightJoin::Failed("boom".into()));
    }

    #[tokio::test]
    async fn metrics_collector_shutdown_does_not_hang() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                if *rx.borrow() {
                    break;
                }
                tokio::select! {
                    result = rx.changed() => {
                        if result.is_err() || *rx.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
            }
        });
        shutdown_metrics_collector(&tx, task, Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn metrics_collector_timeout_aborts() {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        shutdown_metrics_collector(&tx, task, Duration::from_millis(20)).await;
    }
}
