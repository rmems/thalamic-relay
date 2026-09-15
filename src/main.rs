use clap::Parser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thalamic_relay::cpu::{self, RelayMetrics};
use thalamic_relay::gpu::{HardwareBridge, NvmlActuator};
use thalamic_relay::safety::{
    self, ActuatorError, BRAKE_FRACTION, BrakeCommand, SafetyActuator, SafetyStateMachine,
    SafetyStatus,
};
use thalamic_relay::telemetry::{SampleValidity, TelemetryFrame, TelemetrySample, TelemetrySource};
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// Handle to an in-flight privileged actuation attempt.
type ActuationTask = JoinHandle<Result<(), ActuatorError>>;

/// Dispatch a [`BrakeCommand`] onto a blocking worker so the telemetry loop is
/// never stalled by `nvidia-smi`. The [`SafetyStateMachine`] already tracks
/// in-flight actuation, so at most one apply and one release run concurrently.
fn dispatch(
    command: Option<BrakeCommand>,
    actuator: &Arc<dyn SafetyActuator>,
    brake_task: &mut Option<ActuationTask>,
    release_task: &mut Option<ActuationTask>,
) {
    match command {
        Some(BrakeCommand::Apply) if brake_task.is_none() => {
            let actuator = Arc::clone(actuator);
            *brake_task = Some(tokio::task::spawn_blocking(move || {
                actuator.apply_emergency_brake(BRAKE_FRACTION)
            }));
        }
        Some(BrakeCommand::Release) if release_task.is_none() => {
            let actuator = Arc::clone(actuator);
            *release_task = Some(tokio::task::spawn_blocking(move || {
                actuator.release_emergency_brake()
            }));
        }
        _ => {}
    }
}

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
    tokio::spawn(async move {
        cpu::run_metrics_collector(metrics_clone).await;
    });

    println!("[relay] --- Thalamic Relay ---");
    if cli.force_software_only {
        println!("[relay] running in software-only mode (forced via --force-software-only)");
    } else {
        println!("[relay] running in software-only mode (no FPGA/silicon-bridge)");
    }

    let mut step_count: u64 = 0;
    let mut warned_brake_held_sim = false;
    let mut brake_task: Option<ActuationTask> = None;
    let mut release_task: Option<ActuationTask> = None;

    // Privileged NVML/nvidia-smi actuation backend. The supervisor only ever
    // talks to it through the pure `SafetyStateMachine`; all safety semantics
    // live in `thalamic_relay::safety`, never in the NVIDIA adapter.
    let actuator: Arc<dyn SafetyActuator> = Arc::new(NvmlActuator::new());

    // Detect leftover throttle from a prior crash (hardware PL persists across process restarts).
    // Only seed the engaged state when the current limit matches this relay's expected brake
    // target, so deliberate operator-set sub-default caps are not auto-restored to default.
    let mut machine = match actuator.detect_engaged_brake(BRAKE_FRACTION) {
        Some(m) => {
            eprintln!(
                "[relay] WARNING: GPU power limit {}W matches expected emergency brake \
                 target {}W (default {}W); will auto-release after Ok streak",
                m.current_w, m.expected_w, m.default_w
            );
            SafetyStateMachine::with_brake_engaged()
        }
        None => SafetyStateMachine::new(),
    };

    loop {
        step_count += 1;
        let telemetry =
            HardwareBridge::read_telemetry_with(cli.force_software_only, cli.step_interval_ms);

        // Reap any finished actuation, feed the outcome back into the pure state
        // machine, then re-evaluate immediately so a completed apply/release does
        // not wait a full safety cadence to be reconciled.
        let mut reassessed_this_iter = false;
        if brake_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = brake_task.take().expect("finished brake task exists");
            match task.await {
                Ok(Ok(())) => machine.on_apply_result(true),
                Ok(Err(e)) => {
                    eprintln!("[relay] Emergency brake failed: {e}");
                    machine.on_apply_result(false);
                }
                Err(e) => {
                    eprintln!("[relay] Brake task panicked: {e}");
                    machine.on_apply_result(false);
                }
            }
            let assessment = safety::classify(&telemetry);
            dispatch(
                machine.observe(&assessment.status, assessment.simulated),
                &actuator,
                &mut brake_task,
                &mut release_task,
            );
            reassessed_this_iter = true;
        }
        if release_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = release_task.take().expect("finished release task exists");
            match task.await {
                Ok(Ok(())) => machine.on_release_result(true),
                Ok(Err(e)) => {
                    eprintln!("[relay] Brake release failed: {e}");
                    machine.on_release_result(false);
                }
                Err(e) => {
                    eprintln!("[relay] Brake release task panicked: {e}");
                    machine.on_release_result(false);
                }
            }
            let assessment = safety::classify(&telemetry);
            if matches!(
                assessment.status,
                SafetyStatus::Critical(_) | SafetyStatus::Warn(_)
            ) {
                eprintln!("[relay] Safety not clear after brake release, re-evaluating brake");
            }
            dispatch(
                machine.observe(&assessment.status, assessment.simulated),
                &actuator,
                &mut brake_task,
                &mut release_task,
            );
            reassessed_this_iter = true;
        }

        // Periodic safety check every 10 steps (rate scales with step_interval_ms).
        if step_count.is_multiple_of(10) && !reassessed_this_iter {
            let assessment = safety::classify(&telemetry);
            match &assessment.status {
                SafetyStatus::Critical(msg) => eprintln!("[relay] SAFETY CRITICAL: {msg}"),
                SafetyStatus::Warn(msg) => eprintln!("[relay] SAFETY WARN: {msg}"),
                SafetyStatus::Ok if assessment.simulated && machine.brake_engaged() => {
                    // Hold the brake: simulated telemetry cannot confirm a safe
                    // release. Log once per simulated episode to avoid flooding.
                    if !warned_brake_held_sim {
                        eprintln!(
                            "[relay] SAFETY: brake held — telemetry is simulated (no real GPU readings to confirm safe release)"
                        );
                        warned_brake_held_sim = true;
                    }
                }
                SafetyStatus::Ok => {}
            }
            if !assessment.simulated {
                warned_brake_held_sim = false;
            }
            dispatch(
                machine.observe(&assessment.status, assessment.simulated),
                &actuator,
                &mut brake_task,
                &mut release_task,
            );
        }

        // Store acquired_at; collector computes freshness at scrape/export time.
        {
            let mut metrics = relay_metrics.lock().unwrap();
            metrics.telemetry_acquired_at = Some(telemetry.acquired_at);
        }

        print_dashboard(&telemetry, step_count);

        sleep(Duration::from_millis(cli.step_interval_ms)).await;
    }
}

fn print_dashboard(frame: &TelemetryFrame, step: u64) {
    let pwr = format_live_reading(&frame.power_w, |w| format!("{w:5.1}W"));
    let vddcr = format_live_reading(&frame.vddcr_gfx_v, |v| format!("{v:.3}V"));
    let tag = match frame.source {
        TelemetrySource::SoftwareFallback => " [sim]",
        TelemetrySource::NvmlUnavailable => " [unavail]",
        TelemetrySource::Nvml => "",
    };
    print!("\r[Step {step}] Pwr: {pwr} | Vddcr: {vddcr}{tag}   ");
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
}
