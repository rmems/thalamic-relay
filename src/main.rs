use clap::Parser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thalamic_relay::cpu::{self, RelayMetrics};
use thalamic_relay::gpu::{GpuTelemetry, HardwareBridge, SafetyStatus};
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
    let mut brake_applied = false;
    let mut ok_count_after_brake: u32 = 0;
    let mut warned_brake_held_sim = false;
    let mut brake_task: Option<JoinHandle<Result<(), String>>> = None;
    let mut release_task: Option<JoinHandle<Result<(), String>>> = None;

    // Detect leftover throttle from a prior crash (hardware PL persists across process restarts).
    // Only seed brake_applied when the current limit matches this relay's expected 50% brake
    // target, so deliberate operator-set sub-default caps are not auto-restored to default.
    if let Some((current_w, default_w, expected_w)) =
        HardwareBridge::power_limit_matches_emergency_brake(0.5)
    {
        eprintln!(
            "[relay] WARNING: GPU power limit {current_w}W matches expected emergency brake \
             target {expected_w}W (default {default_w}W); will auto-release after Ok streak"
        );
        brake_applied = true;
    }

    loop {
        step_count += 1;
        let loop_start = Instant::now();
        let telemetry = HardwareBridge::read_telemetry_force(cli.force_software_only);

        let mut ok_count_updated_this_iter = false;
        if brake_task.as_ref().is_some_and(|task| task.is_finished()) {
            let task = brake_task.take().expect("finished brake task exists");
            match task.await {
                Ok(Ok(())) => {
                    brake_applied = true;
                    // The GPU may have already recovered while the brake was being applied.
                    // Re-check current telemetry so a stale brake doesn't linger unnoticed
                    // until the next periodic safety check.
                    let force_software_only = cli.force_software_only;
                    let post_telemetry = tokio::task::spawn_blocking(move || {
                        HardwareBridge::read_telemetry_force(force_software_only)
                    })
                    .await
                    .expect("post-brake telemetry read task panicked");
                    let (post_safety, is_sim) = HardwareBridge::check_safety(&post_telemetry);
                    if matches!(post_safety, SafetyStatus::Ok) && !is_sim {
                        ok_count_after_brake = ok_count_after_brake.saturating_add(1);
                        if ok_count_after_brake >= 3 && release_task.is_none() {
                            release_task = Some(tokio::task::spawn_blocking(|| {
                                HardwareBridge::release_emergency_brake()
                            }));
                        }
                    } else {
                        ok_count_after_brake = 0;
                    }
                    ok_count_updated_this_iter = true;
                }
                Ok(Err(e)) => eprintln!("[relay] Emergency brake failed: {e}"),
                Err(e) => eprintln!("[relay] Brake task panicked: {e}"),
            }
        }
        if release_task.as_ref().is_some_and(|task| task.is_finished()) {
            let task = release_task.take().expect("finished release task exists");
            match task.await {
                Ok(Ok(())) => {
                    ok_count_after_brake = 0;
                    let force_software_only = cli.force_software_only;
                    let post_telemetry = tokio::task::spawn_blocking(move || {
                        HardwareBridge::read_telemetry_force(force_software_only)
                    })
                    .await
                    .expect("post-release telemetry read task panicked");
                    let (post_safety, is_sim) = HardwareBridge::check_safety(&post_telemetry);
                    match post_safety {
                        SafetyStatus::Critical(_) => {
                            // Release already restored the default power limit; clear the
                            // brake flag before re-applying so a failed re-apply can be retried.
                            eprintln!(
                                "[relay] Safety critical after brake release, re-applying brake"
                            );
                            brake_applied = false;
                            if brake_task.is_none() {
                                brake_task = Some(tokio::task::spawn_blocking(|| {
                                    HardwareBridge::apply_emergency_brake(0.5)
                                }));
                            }
                        }
                        SafetyStatus::Ok if is_sim => {
                            // Physical brake was released; can't confirm safe state
                            // with simulated telemetry. Mark released but log the gap.
                            brake_applied = false;
                            eprintln!(
                                "[relay] SAFETY: brake released but post-release telemetry is simulated"
                            );
                        }
                        SafetyStatus::Warn(_) => {
                            // Release already restored the default power limit; clear the
                            // brake flag before re-applying so a failed re-apply can be retried.
                            eprintln!(
                                "[relay] Safety warning after brake release, re-applying brake"
                            );
                            brake_applied = false;
                            if brake_task.is_none() {
                                brake_task = Some(tokio::task::spawn_blocking(|| {
                                    HardwareBridge::apply_emergency_brake(0.5)
                                }));
                            }
                        }
                        SafetyStatus::Ok => {
                            // Post-release telemetry confirms safe state — clear brake flag.
                            brake_applied = false;
                        }
                    }
                }
                Ok(Err(e)) => {
                    ok_count_after_brake = 0;
                    eprintln!("[relay] Brake release failed: {e}");
                }
                Err(e) => {
                    ok_count_after_brake = 0;
                    eprintln!("[relay] Brake release task panicked: {e}");
                }
            }
        }

        // Safety check every 10 steps (rate scales with step_interval_ms)
        if step_count.is_multiple_of(10) {
            let (safety, is_sim) = HardwareBridge::check_safety(&telemetry);
            match safety {
                SafetyStatus::Critical(msg) => {
                    eprintln!("[relay] SAFETY CRITICAL: {msg}");
                    ok_count_after_brake = 0;
                    if !brake_applied && brake_task.is_none() {
                        brake_task = Some(tokio::task::spawn_blocking(|| {
                            HardwareBridge::apply_emergency_brake(0.5)
                        }));
                    }
                }
                SafetyStatus::Warn(msg) => {
                    eprintln!("[relay] SAFETY WARN: {msg}");
                    ok_count_after_brake = 0;
                }
                SafetyStatus::Ok => {
                    if brake_applied {
                        if is_sim {
                            // Hold brake while real telemetry is unavailable; reset hysteresis
                            // so release requires 3 consecutive *real* Ok readings after recovery.
                            ok_count_after_brake = 0;
                            if !warned_brake_held_sim {
                                eprintln!(
                                    "[relay] SAFETY: brake held — telemetry is simulated (no real GPU readings to confirm safe release)"
                                );
                                warned_brake_held_sim = true;
                            }
                        } else if !ok_count_updated_this_iter {
                            warned_brake_held_sim = false;
                            // Require 3 consecutive real Ok safety-check readings before release
                            // (at default 100ms tick × every 10 ticks ≈ 3s hysteresis).
                            ok_count_after_brake += 1;
                            if ok_count_after_brake >= 3
                                && release_task.is_none()
                                && brake_task.is_none()
                            {
                                release_task = Some(tokio::task::spawn_blocking(|| {
                                    HardwareBridge::release_emergency_brake()
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Update shared metrics
        {
            let mut metrics = relay_metrics.lock().unwrap();
            metrics.telemetry_freshness_s = loop_start.elapsed().as_secs_f64();
        }

        print_dashboard(&telemetry, step_count);

        sleep(Duration::from_millis(cli.step_interval_ms)).await;
    }
}

fn print_dashboard(telemetry: &GpuTelemetry, step: u64) {
    print!(
        "\r[Step {step}] Pwr: {:5.1}W | Vcore: {:.3}V   ",
        telemetry.power_w, telemetry.vddcr_gfx_v
    );
    let _ = io::stdout().flush();
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
}
