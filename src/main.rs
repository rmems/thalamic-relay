use clap::Parser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thalamic_relay::cpu::{self, RelayMetrics};
use thalamic_relay::gpu::{HardwareBridge, NvmlActuator};
use thalamic_relay::publish::{AbsentPublisher, SensoryPublisher, evaluate_then_try_publish};
use thalamic_relay::safety::{
    ActuatorError, ActuatorOutcome, BRAKE_FRACTION, BrakeIntent, PowerLimitObservation,
    SafetyActuator, SafetyMachine, SafetySnapshot, SafetyState, classify_power_limit,
};
use thalamic_relay::shutdown::{
    InFlightActuation, InFlightJoin, SHUTDOWN_ACTUATOR_TIMEOUT, SHUTDOWN_METRICS_TIMEOUT,
    ShutdownActuation, ShutdownPlan, ShutdownReason, join_in_flight, plan_shutdown,
    shutdown_metrics_collector,
};
use thalamic_relay::telemetry::{SampleValidity, TelemetryFrame, TelemetrySample, TelemetrySource};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::sleep;

#[derive(Debug)]
struct LockGuard(String);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Acquire the single-instance lock file, reclaiming it only when the recorded PID is dead.
fn try_acquire_lock(lock_path: &str) -> Result<LockGuard, String> {
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{}", std::process::id()) {
                    return Err(format!("Failed to write PID to lock file {lock_path}: {e}"));
                }
                return Ok(LockGuard(lock_path.to_string()));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                // A lock exists. Reclaim it ONLY when we can positively confirm
                // the recorded PID is dead. Any ambiguity (unreadable or
                // unparseable lock file) fails closed to preserve the
                // single-instance guarantee.
                let Some(recorded_pid) = std::fs::read_to_string(lock_path)
                    .ok()
                    .and_then(|content| content.trim().parse::<u32>().ok())
                else {
                    return Err(format!(
                        "Lock file {lock_path} exists but is unreadable/unparseable; refusing to start."
                    ));
                };

                if std::path::Path::new(&format!("/proc/{recorded_pid}")).exists() {
                    return Err(format!(
                        "Another instance is already active (PID: {recorded_pid})."
                    ));
                }

                // Stale lock from a dead PID: remove it and retry. If removal
                // fails, abort instead of spinning in a tight retry loop.
                if let Err(remove_err) = std::fs::remove_file(lock_path) {
                    return Err(format!(
                        "Failed to clear stale lock {lock_path}: {remove_err}"
                    ));
                }
            }
            Err(err) => return Err(format!("Failed to create lock file {lock_path}: {err}")),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Acquire the single-instance lock atomically BEFORE binding any ports.
    // This ensures the clean "Another instance is already active" message
    // appears instead of a Prometheus bind panic when two instances race.
    let lock_path = "/tmp/thalamic_relay.lock";
    let _lock_guard = match try_acquire_lock(lock_path) {
        Ok(guard) => guard,
        Err(msg) => {
            eprintln!("[relay] FATAL: {msg}");
            std::process::exit(1);
        }
    };

    let metrics_addr = std::net::SocketAddr::new(cli.metrics_ip, 9000);
    cpu::init_telemetry(metrics_addr);

    let relay_metrics = Arc::new(Mutex::new(RelayMetrics::default()));
    let metrics_clone = Arc::clone(&relay_metrics);
    let (metrics_shutdown_tx, metrics_shutdown_rx) = tokio::sync::watch::channel(false);
    let metrics_task = tokio::spawn(async move {
        cpu::run_metrics_collector(metrics_clone, metrics_shutdown_rx).await;
    });

    println!("[relay] --- Thalamic Relay ---");
    if cli.force_software_only {
        println!("[relay] running in software-only mode (forced via --force-software-only)");
    } else {
        println!("[relay] running in software-only mode (no FPGA/silicon-bridge)");
    }

    let mut step_count: u64 = 0;
    let mut machine = SafetyMachine::new();
    let publisher = AbsentPublisher;
    let mut warned_brake_held_sim = false;
    let mut brake_task: Option<ActuationTask> = None;
    let mut release_task: Option<ActuationTask> = None;

    // Privileged NVML/nvidia-smi actuation backend. The supervisor only ever
    // reaches hardware through this `SafetyActuator`; all safety semantics live
    // in `thalamic_relay::safety`, never in the NVIDIA adapter (GH#46).
    let actuator: Arc<dyn SafetyActuator> = Arc::new(NvmlActuator::new());

    // Detect leftover throttle from a prior crash (hardware PL persists across process restarts).
    // Only seed brake_applied when the current limit matches this relay's expected 50% brake
    // target, so deliberate operator-set sub-default caps are not auto-restored to default.
    // This query uses the real actuator even under `--force-software-only`: simulated
    // telemetry must not hide or authorize release of a real persistent brake.
    let (current_w, default_w) = actuator.query_power_limits_w();
    match classify_power_limit(current_w, default_w, BRAKE_FRACTION) {
        PowerLimitObservation::RelayOwnedBrake(m) => {
            eprintln!(
                "[relay] WARNING: GPU power limit {}W matches expected emergency brake \
                 target {}W (default {}W); will auto-release after Ok streak",
                m.current_w, m.expected_w, m.default_w
            );
            machine.seed_brake_applied();
            let seed = machine.snapshot();
            {
                let mut metrics = relay_metrics.lock().unwrap();
                cpu::record_safety_snapshot(&mut metrics, &seed);
            }
        }
        PowerLimitObservation::ForeignSubDefaultCap {
            current_w,
            default_w,
            expected_brake_w,
        } => {
            eprintln!(
                "[relay] GPU power limit {current_w}W is below default {default_w}W but does not \
                 match relay brake target {expected_brake_w}W; leaving operator/device cap unchanged"
            );
        }
        PowerLimitObservation::Unreadable => {
            tracing::info!(
                "power limits unreadable at startup; first-frame eval will fail-closed if needed"
            );
        }
        PowerLimitObservation::AtOrAboveDefault { .. } => {}
    }

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut sigterm = signal(SignalKind::terminate())?;

    let shutdown_reason = loop {
        step_count += 1;
        let telemetry =
            HardwareBridge::read_telemetry_with(cli.force_software_only, cli.step_interval_ms);

        let mut evaluated_this_iter = false;

        if brake_task.as_ref().is_some_and(|task| task.is_finished()) {
            let task = brake_task.take().expect("finished brake task exists");
            match task.await {
                Ok(Ok(())) => {
                    let snap = machine.record_actuator(ActuatorOutcome::Applied);
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    let force_software_only = cli.force_software_only;
                    let cadence_ms = cli.step_interval_ms;
                    let post_telemetry = tokio::task::spawn_blocking(move || {
                        HardwareBridge::read_telemetry_with(force_software_only, cadence_ms)
                    })
                    .await
                    .expect("post-brake telemetry read task panicked");
                    let (snap, pub_res) =
                        evaluate_then_try_publish(&mut machine, &post_telemetry, &publisher);
                    let _ = pub_res;
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    spawn_intent(&snap, &actuator, &mut brake_task, &mut release_task);
                    evaluated_this_iter = true;
                }
                Ok(Err(e)) => {
                    eprintln!("[relay] Emergency brake failed: {e}");
                    let snap = machine.record_actuator(ActuatorOutcome::ApplyFailed(e.to_string()));
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    // Retry on the safety cadence / first-frame path, not every tick.
                }
                Err(e) => {
                    eprintln!("[relay] Brake task panicked: {e}");
                    let snap = machine.record_actuator(ActuatorOutcome::ApplyFailed(e.to_string()));
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                }
            }
        }
        if release_task.as_ref().is_some_and(|task| task.is_finished()) {
            let task = release_task.take().expect("finished release task exists");
            match task.await {
                Ok(Ok(())) => {
                    let snap = machine.record_actuator(ActuatorOutcome::Released);
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    let force_software_only = cli.force_software_only;
                    let cadence_ms = cli.step_interval_ms;
                    let post_telemetry = tokio::task::spawn_blocking(move || {
                        HardwareBridge::read_telemetry_with(force_software_only, cadence_ms)
                    })
                    .await
                    .expect("post-release telemetry read task panicked");
                    let (snap, pub_res) =
                        evaluate_then_try_publish(&mut machine, &post_telemetry, &publisher);
                    let _ = pub_res;
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    spawn_intent(&snap, &actuator, &mut brake_task, &mut release_task);
                    evaluated_this_iter = true;
                }
                Ok(Err(e)) => {
                    eprintln!("[relay] Brake release failed: {e}");
                    let snap =
                        machine.record_actuator(ActuatorOutcome::ReleaseFailed(e.to_string()));
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                    // Retry on the safety cadence / first-frame path, not every tick.
                }
                Err(e) => {
                    eprintln!("[relay] Brake release task panicked: {e}");
                    let snap =
                        machine.record_actuator(ActuatorOutcome::ReleaseFailed(e.to_string()));
                    store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
                }
            }
        }

        // First acquired frame is evaluated immediately (fail-closed startup).
        // Later evaluations keep the every-10-ticks cadence.
        // Do not spawn from the pre-telemetry snapshot: SoftwareFallback holds
        // rather than applies, which only classify_frame can decide.
        if !evaluated_this_iter && (step_count == 1 || step_count.is_multiple_of(10)) {
            let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &telemetry, &publisher);
            let _ = pub_res;
            store_safety(&relay_metrics, &snap, &mut warned_brake_held_sim);
            spawn_intent(&snap, &actuator, &mut brake_task, &mut release_task);
        } else {
            // Publication is outside the safety critical path and never awaited.
            let _ = publisher.try_publish(&telemetry.to_sensory_mapping());
        }

        {
            let mut metrics = relay_metrics.lock().unwrap();
            metrics.telemetry_acquired_at = Some(telemetry.acquired_at);
        }

        print_dashboard(&telemetry, step_count, machine.snapshot().state);

        tokio::select! {
            _ = &mut ctrl_c => {
                break ShutdownReason::Sigint;
            }
            _ = sigterm.recv() => {
                break ShutdownReason::Sigterm;
            }
            _ = sleep(Duration::from_millis(cli.step_interval_ms)) => {}
        }
    };

    let _plan = perform_orderly_shutdown(
        shutdown_reason,
        &mut machine,
        &mut brake_task,
        &mut release_task,
        &relay_metrics,
        &mut warned_brake_held_sim,
        &metrics_shutdown_tx,
        metrics_task,
    )
    .await;
    eprintln!("[relay] releasing process lock {lock_path}");

    Ok(())
}

fn store_safety(
    relay_metrics: &Arc<Mutex<RelayMetrics>>,
    snap: &SafetySnapshot,
    warned_brake_held_sim: &mut bool,
) {
    log_safety_snapshot(snap, warned_brake_held_sim);
    let mut metrics = relay_metrics.lock().unwrap();
    cpu::record_safety_snapshot(&mut metrics, snap);
}

#[allow(clippy::too_many_arguments)]
async fn perform_orderly_shutdown(
    reason: ShutdownReason,
    machine: &mut SafetyMachine,
    brake_task: &mut Option<ActuationTask>,
    release_task: &mut Option<ActuationTask>,
    relay_metrics: &Arc<Mutex<RelayMetrics>>,
    warned_brake_held_sim: &mut bool,
    metrics_shutdown: &tokio::sync::watch::Sender<bool>,
    metrics_task: JoinHandle<()>,
) -> ShutdownPlan {
    let snap = machine.snapshot();
    let in_flight = InFlightActuation::from_tasks(brake_task.is_some(), release_task.is_some());
    let plan = plan_shutdown(reason, &snap, in_flight);

    println!();
    eprintln!(
        "[relay] shutdown reason={} state={} policy_state={} brake_engaged={} desired_brake={} \
         in_flight={} unresolved_brake={} unresolved_actuator={} actuation={} ({})",
        plan.reason.as_str(),
        snap.state.as_str(),
        snap.policy_state.as_str(),
        snap.brake_engaged,
        snap.desired_brake,
        in_flight.as_str(),
        plan.unresolved_brake,
        plan.unresolved_actuator,
        plan.actuation.as_str(),
        plan.summary,
    );
    tracing::info!(
        reason = plan.reason.as_str(),
        state = snap.state.as_str(),
        policy_state = snap.policy_state.as_str(),
        brake_engaged = snap.brake_engaged,
        desired_brake = snap.desired_brake,
        in_flight = in_flight.as_str(),
        unresolved_brake = plan.unresolved_brake,
        unresolved_actuator = plan.unresolved_actuator,
        actuation = plan.actuation.as_str(),
        summary = plan.summary,
        "orderly shutdown (fail-closed: no new power-limit restore)"
    );

    {
        let mut metrics = relay_metrics.lock().unwrap();
        cpu::record_shutdown(&mut metrics, &plan);
    }

    match plan.actuation {
        ShutdownActuation::Idle => {
            if let Some(task) = brake_task.take() {
                task.abort();
            }
            if let Some(task) = release_task.take() {
                task.abort();
            }
        }
        ShutdownActuation::AwaitApply => {
            if let Some(task) = brake_task.take() {
                match join_in_flight(task, SHUTDOWN_ACTUATOR_TIMEOUT).await {
                    InFlightJoin::Succeeded => {
                        let snap = machine.record_actuator(ActuatorOutcome::Applied);
                        store_safety(relay_metrics, &snap, warned_brake_held_sim);
                    }
                    InFlightJoin::Failed(e) | InFlightJoin::Panicked(e) => {
                        eprintln!("[relay] shutdown: in-flight apply did not succeed: {e}");
                        let snap = machine.record_actuator(ActuatorOutcome::ApplyFailed(e));
                        store_safety(relay_metrics, &snap, warned_brake_held_sim);
                    }
                    InFlightJoin::TimedOut => {
                        eprintln!(
                            "[relay] shutdown: in-flight apply timed out; leaving hardware unchanged"
                        );
                    }
                }
            }
            if let Some(task) = release_task.take() {
                task.abort();
            }
        }
        ShutdownActuation::AwaitAuthorizedRelease => {
            if let Some(task) = release_task.take() {
                match join_in_flight(task, SHUTDOWN_ACTUATOR_TIMEOUT).await {
                    InFlightJoin::Succeeded => {
                        let snap = machine.record_actuator(ActuatorOutcome::Released);
                        store_safety(relay_metrics, &snap, warned_brake_held_sim);
                    }
                    InFlightJoin::Failed(e) | InFlightJoin::Panicked(e) => {
                        eprintln!("[relay] shutdown: in-flight release did not succeed: {e}");
                        let snap = machine.record_actuator(ActuatorOutcome::ReleaseFailed(e));
                        store_safety(relay_metrics, &snap, warned_brake_held_sim);
                    }
                    InFlightJoin::TimedOut => {
                        eprintln!(
                            "[relay] shutdown: in-flight release timed out; leaving brake engaged"
                        );
                    }
                }
            }
            if let Some(task) = brake_task.take() {
                task.abort();
            }
        }
    }

    shutdown_metrics_collector(metrics_shutdown, metrics_task, SHUTDOWN_METRICS_TIMEOUT).await;
    plan
}

fn log_safety_snapshot(snap: &SafetySnapshot, warned_brake_held_sim: &mut bool) {
    if let Some((from, to)) = snap.transition {
        eprintln!(
            "[relay] safety {} → {} ({})",
            from.as_str(),
            to.as_str(),
            snap.last_reason
        );
    }
    match snap.state {
        SafetyState::CriticalBraked
        | SafetyState::TelemetryMissing
        | SafetyState::TelemetryStale
        | SafetyState::TelemetryInvalid => {
            if snap.transition.is_some() {
                eprintln!("[relay] SAFETY CRITICAL: {}", snap.last_reason);
            }
        }
        SafetyState::Warning => {
            if snap.transition.is_some() {
                eprintln!("[relay] SAFETY WARN: {}", snap.last_reason);
            }
        }
        SafetyState::SimulatedSoftwareOnly if snap.brake_engaged => {
            if !*warned_brake_held_sim {
                eprintln!(
                    "[relay] SAFETY: brake held — telemetry is simulated (no real GPU readings to confirm safe release)"
                );
                *warned_brake_held_sim = true;
            }
        }
        SafetyState::ActuatorFailure => {
            if snap.actuator_failed
                && let Some(err) = &snap.last_actuator_error
            {
                eprintln!("[relay] SAFETY actuator failure: {err}");
            }
        }
        SafetyState::HealthyReal | SafetyState::Recovering | SafetyState::SimulatedSoftwareOnly => {
            *warned_brake_held_sim = false;
        }
    }
}

/// Handle to an in-flight privileged actuation attempt.
type ActuationTask = JoinHandle<Result<(), ActuatorError>>;

/// Dispatch the machine's [`BrakeIntent`] onto a blocking worker so the
/// telemetry loop is never stalled by `nvidia-smi`. Actuation always goes
/// through the [`SafetyActuator`] boundary; the outcome is fed back into the
/// pure [`SafetyMachine`] by the caller.
fn spawn_intent(
    snap: &SafetySnapshot,
    actuator: &Arc<dyn SafetyActuator>,
    brake_task: &mut Option<ActuationTask>,
    release_task: &mut Option<ActuationTask>,
) {
    match snap.intent {
        BrakeIntent::Apply if brake_task.is_none() && release_task.is_none() => {
            let actuator = Arc::clone(actuator);
            *brake_task = Some(tokio::task::spawn_blocking(move || {
                actuator.apply_emergency_brake(BRAKE_FRACTION)
            }));
        }
        BrakeIntent::Release if release_task.is_none() && brake_task.is_none() => {
            let actuator = Arc::clone(actuator);
            *release_task = Some(tokio::task::spawn_blocking(move || {
                actuator.release_emergency_brake()
            }));
        }
        _ => {}
    }
}

fn print_dashboard(frame: &TelemetryFrame, step: u64, safety: SafetyState) {
    let pwr = format_live_reading(&frame.power_w, |w| format!("{w:5.1}W"));
    let vddcr = format_live_reading(&frame.vddcr_gfx_v, |v| format!("{v:.3}V"));
    let tag = match frame.source {
        TelemetrySource::SoftwareFallback => " [sim]",
        TelemetrySource::NvmlUnavailable => " [unavail]",
        TelemetrySource::Nvml => "",
    };
    print!(
        "\r[Step {step}] Pwr: {pwr} | Vddcr: {vddcr}{tag} | safety: {}   ",
        safety.as_str()
    );
    let _ = io::stdout().flush();
}

/// Live hardware display: only [`SampleValidity::Valid`] + present values.
fn format_live_reading(sample: &TelemetrySample<f32>, fmt_val: impl Fn(f32) -> String) -> String {
    match (sample.validity, sample.value) {
        (SampleValidity::Valid, Some(v)) => fmt_val(v),
        (SampleValidity::Stale, _) => "stale".to_string(),
        (SampleValidity::Invalid, _) => "inv".to_string(),
        (SampleValidity::Missing, _) | (_, None) => "n/a".to_string(),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "thalamic-relay",
    version,
    about = "Thalamic Relay - sensory + hardware-safety relay for hardware telemetry (software-only)"
)]
struct Cli {
    /// Prometheus metrics listen IP (port is always 9000 per compliance)
    #[arg(long, default_value = "127.0.0.1", env = "THALAMIC_METRICS_IP", value_parser = clap::value_parser!(std::net::IpAddr))]
    metrics_ip: std::net::IpAddr,

    /// Relay loop tick interval (ms); minimum 1 to prevent busy-looping
    #[arg(long, default_value_t = 100, env = "THALAMIC_STEP_INTERVAL_MS", value_parser = clap::value_parser!(u64).range(1..))]
    step_interval_ms: u64,

    /// Force software-only mode (skip real GPU telemetry attempts, use sim).
    /// Usable as a bare flag (`--force-software-only`) or with an explicit
    /// value (`--force-software-only=false` / `THALAMIC_FORCE_SOFTWARE_ONLY=false`).
    #[arg(long, env = "THALAMIC_FORCE_SOFTWARE_ONLY", num_args = 0..=1, default_missing_value = "true", default_value_t = false, value_parser = clap::value_parser!(bool))]
    force_software_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_custom_args_and_env_equiv() {
        // direct args
        let cli = Cli::try_parse_from([
            "thalamic-relay",
            "--metrics-ip",
            "0.0.0.0",
            "--step-interval-ms",
            "50",
            "--force-software-only",
        ])
        .unwrap();
        assert_eq!(
            cli.metrics_ip,
            "0.0.0.0".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(cli.step_interval_ms, 50);
        assert!(cli.force_software_only);
    }

    #[test]
    fn parses_defaults() {
        let cli = Cli::try_parse_from(["thalamic-relay"]).unwrap();
        assert_eq!(
            cli.metrics_ip,
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(cli.step_interval_ms, 100);
        assert!(!cli.force_software_only);
    }

    #[test]
    fn parses_force_software_only_false() {
        let cli = Cli::try_parse_from(["thalamic-relay", "--force-software-only=false"]).unwrap();
        assert!(!cli.force_software_only);
    }

    #[test]
    fn lock_guard_created_and_removed() {
        let lock_path = "/tmp/thalamic_relay_test_created.lock";
        let _ = std::fs::remove_file(lock_path);
        let guard = try_acquire_lock(lock_path).unwrap();
        assert!(std::path::Path::new(lock_path).exists());
        drop(guard);
        assert!(!std::path::Path::new(lock_path).exists());
    }

    #[test]
    fn lock_guard_rejects_active_pid() {
        let lock_path = "/tmp/thalamic_relay_test_active.lock";
        let _ = std::fs::remove_file(lock_path);
        std::fs::write(lock_path, std::process::id().to_string()).unwrap();
        let err = try_acquire_lock(lock_path).unwrap_err();
        assert!(
            err.contains("already active"),
            "expected active-instance error, got: {err}"
        );
        let _ = std::fs::remove_file(lock_path);
    }

    #[test]
    fn lock_guard_reclaims_stale_lock() {
        let lock_path = "/tmp/thalamic_relay_test_stale.lock";
        let _ = std::fs::remove_file(lock_path);
        std::fs::write(lock_path, "0").unwrap();
        let guard = try_acquire_lock(lock_path).unwrap();
        let content = std::fs::read_to_string(lock_path).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
        drop(guard);
        assert!(!std::path::Path::new(lock_path).exists());
    }

    #[test]
    fn dashboard_shows_only_valid_present_as_live_hardware() {
        use thalamic_relay::telemetry::{assess, fixtures};

        let live = assess(&fixtures::healthy_real(), fixtures::NOW);
        assert_eq!(
            format_live_reading(&live.power_w, |w| format!("{w:.0}W")),
            "200W"
        );

        let stale = assess(&fixtures::stale(), fixtures::NOW);
        assert_eq!(
            format_live_reading(&stale.power_w, |w| format!("{w:.0}W")),
            "stale"
        );

        let invalid = assess(&fixtures::out_of_range(), fixtures::NOW);
        assert_eq!(
            format_live_reading(&invalid.gpu_temp_c, |t| format!("{t:.0}")),
            "inv"
        );

        let missing = assess(&fixtures::sensor_dropout(), fixtures::NOW);
        assert_eq!(
            format_live_reading(&missing.power_w, |w| format!("{w:.0}W")),
            "n/a"
        );
    }

    #[tokio::test]
    async fn orderly_shutdown_leaves_seeded_brake_and_stops_metrics() {
        let mut machine = SafetyMachine::new();
        machine.seed_brake_applied();
        let mut brake_task = None;
        let mut release_task = None;
        let relay_metrics = Arc::new(Mutex::new(RelayMetrics::default()));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let metrics_task = tokio::spawn(cpu::run_metrics_collector(Arc::clone(&relay_metrics), rx));
        let mut warned = false;
        let plan = perform_orderly_shutdown(
            ShutdownReason::Sigterm,
            &mut machine,
            &mut brake_task,
            &mut release_task,
            &relay_metrics,
            &mut warned,
            &tx,
            metrics_task,
        )
        .await;
        assert_eq!(plan.actuation, ShutdownActuation::Idle);
        assert!(plan.leave_brake_engaged);
        assert!(plan.unresolved_brake);
        assert!(machine.snapshot().brake_engaged);
        assert_eq!(
            relay_metrics.lock().unwrap().shutdown_reason,
            Some("sigterm")
        );
    }

    #[tokio::test]
    async fn orderly_shutdown_records_in_flight_apply_without_release() {
        use thalamic_relay::safety::FakeActuator;
        use thalamic_relay::telemetry::{assess, fixtures};

        let fake = Arc::new(FakeActuator::new());
        let fake_task = Arc::clone(&fake);
        let mut machine = SafetyMachine::new();
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(90.0);
        let snap = machine.evaluate(&assess(&raw, fixtures::NOW));
        assert_eq!(snap.intent, BrakeIntent::Apply);

        let mut brake_task = Some(tokio::task::spawn_blocking(move || {
            fake_task.apply_emergency_brake(BRAKE_FRACTION)
        }));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut release_task = None;
        let relay_metrics = Arc::new(Mutex::new(RelayMetrics::default()));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let metrics_task = tokio::spawn(cpu::run_metrics_collector(Arc::clone(&relay_metrics), rx));
        let mut warned = false;
        let plan = perform_orderly_shutdown(
            ShutdownReason::Sigint,
            &mut machine,
            &mut brake_task,
            &mut release_task,
            &relay_metrics,
            &mut warned,
            &tx,
            metrics_task,
        )
        .await;
        assert_eq!(plan.actuation, ShutdownActuation::AwaitApply);
        assert!(fake.is_engaged());
        assert!(machine.snapshot().brake_engaged);
        assert_eq!(fake.apply_calls(), 1);
        assert_eq!(fake.release_calls(), 0);
    }

    #[tokio::test]
    async fn orderly_shutdown_then_lock_guard_releases_file() {
        let lock_path = "/tmp/thalamic_relay_test_shutdown.lock";
        let _ = std::fs::remove_file(lock_path);
        let guard = try_acquire_lock(lock_path).unwrap();
        assert!(std::path::Path::new(lock_path).exists());

        let mut machine = SafetyMachine::new();
        let mut brake_task = None;
        let mut release_task = None;
        let relay_metrics = Arc::new(Mutex::new(RelayMetrics::default()));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let metrics_task = tokio::spawn(cpu::run_metrics_collector(Arc::clone(&relay_metrics), rx));
        let mut warned = false;
        let _ = perform_orderly_shutdown(
            ShutdownReason::Sigint,
            &mut machine,
            &mut brake_task,
            &mut release_task,
            &relay_metrics,
            &mut warned,
            &tx,
            metrics_task,
        )
        .await;
        assert!(std::path::Path::new(lock_path).exists());
        drop(guard);
        assert!(!std::path::Path::new(lock_path).exists());
    }
}
