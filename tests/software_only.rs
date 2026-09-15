//! Software-only integration coverage (RM-1217 / GH#27).
//!
//! These tests require no NVIDIA GPU, `sudo`, or `nvidia-smi`. They lock the
//! post-GH#39 regression surface: telemetry acquisition, deterministic safety
//! transitions, CLI/process lifecycle, and IPC isolation. Typed `corpus-ipc`
//! round-trips remain GH#40 / RM-1144.

use std::process::Command;
use thalamic_relay::gpu::HardwareBridge;
use thalamic_relay::publish::{
    AbsentPublisher, FailingPublisher, IsolatedPublishQueue, PublishError,
    evaluate_then_try_publish,
};
use thalamic_relay::safety::{
    ActuatorOutcome, BRAKE_FRACTION, BrakeIntent, FakeActuator, SafetyActuator, SafetyMachine,
    SafetyState,
};
use thalamic_relay::telemetry::{
    SampleValidity, TelemetrySource, assess, fixtures, software_fallback,
};

#[test]
fn binary_help_and_version_exit_zero_without_gpu() {
    let bin = env!("CARGO_BIN_EXE_thalamic-relay");
    for arg in ["--help", "-V"] {
        let output = Command::new(bin)
            .arg(arg)
            .output()
            .unwrap_or_else(|e| panic!("spawn {arg}: {e}"));
        assert!(
            output.status.success(),
            "{arg} failed ({:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
    }
    let help = Command::new(bin).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--force-software-only"));
    assert!(text.contains("--metrics-ip"));
    assert!(text.contains("--step-interval-ms"));
}

#[test]
fn binary_software_only_starts_without_nvidia() {
    let bin = env!("CARGO_BIN_EXE_thalamic-relay");
    let lock_path = "/tmp/thalamic_relay.lock";
    let _ = std::fs::remove_file(lock_path);

    let output = Command::new("timeout")
        .args([
            "2s",
            bin,
            "--force-software-only",
            "--step-interval-ms",
            "50",
        ])
        .output()
        .expect("run thalamic-relay under timeout");

    let _ = std::fs::remove_file(lock_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("software-only"),
        "expected software-only startup, exit={:?} stdout={stdout:?} stderr={stderr:?}",
        output.status.code()
    );
}

#[test]
fn software_only_pipeline_evaluates_safety_then_best_effort_publish() {
    let frame = HardwareBridge::read_telemetry_force(true);
    assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
    assert_eq!(frame.gpu_temp_c.value, Some(software_fallback::GPU_TEMP_C));
    assert_eq!(frame.vram_temp_c.value, None);
    assert_eq!(frame.vram_temp_c.validity, SampleValidity::Missing);

    let mut machine = SafetyMachine::new();
    let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &frame, &AbsentPublisher);
    assert_eq!(pub_res, Err(PublishError::Absent));
    assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
    assert_eq!(snap.intent, BrakeIntent::None);

    let mapping = frame.to_sensory_mapping();
    assert_eq!(
        mapping.acquisition_source,
        TelemetrySource::SoftwareFallback
    );
    let names: Vec<&str> = mapping.stimuli.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["gpu_temp_c", "power_w", "gpu_clock_mhz", "mem_util_pct"]
    );
}

#[test]
fn ipc_failure_cannot_stall_safety_loop_progress() {
    let mut machine = SafetyMachine::new();
    let publisher = FailingPublisher::send_failed();
    let fake = FakeActuator::new();

    let mut critical = fixtures::healthy_real();
    critical.gpu_temp_c = Some(95.0);
    let critical = assess(&critical, fixtures::NOW);

    let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &critical, &publisher);
    assert!(pub_res.is_err());
    assert_eq!(snap.state, SafetyState::CriticalBraked);
    assert_eq!(snap.intent, BrakeIntent::Apply);

    fake.apply_emergency_brake(BRAKE_FRACTION).unwrap();
    let snap = machine.record_actuator(ActuatorOutcome::Applied);
    assert!(snap.brake_engaged);

    let ok = assess(&fixtures::healthy_real(), fixtures::NOW);
    let _ = evaluate_then_try_publish(&mut machine, &ok, &publisher);
    let _ = evaluate_then_try_publish(&mut machine, &ok, &publisher);
    let (third, pub_res) = evaluate_then_try_publish(&mut machine, &ok, &publisher);
    assert_eq!(
        pub_res,
        Err(PublishError::SendFailed("ipc send failed".into()))
    );
    assert_eq!(third.state, SafetyState::Recovering);
    assert_eq!(third.intent, BrakeIntent::Release);

    fake.release_emergency_brake().unwrap();
    let snap = machine.record_actuator(ActuatorOutcome::Released);
    assert!(!snap.brake_engaged);
    assert_eq!(fake.apply_calls(), 1);
    assert_eq!(fake.release_calls(), 1);

    let (healthy, pub_res) = evaluate_then_try_publish(&mut machine, &ok, &publisher);
    assert!(pub_res.is_err());
    assert_eq!(healthy.state, SafetyState::HealthyReal);
    assert_eq!(healthy.intent, BrakeIntent::None);
}

#[test]
fn missing_invalid_stale_and_simulated_frames_are_named_states() {
    let mut machine = SafetyMachine::new();
    let missing = machine.evaluate(&assess(&fixtures::sensor_dropout(), fixtures::NOW));
    assert_eq!(missing.state, SafetyState::TelemetryMissing);
    assert_eq!(missing.intent, BrakeIntent::Apply);

    let invalid = machine.evaluate(&assess(&fixtures::out_of_range(), fixtures::NOW));
    assert_eq!(invalid.state, SafetyState::TelemetryInvalid);

    let stale = machine.evaluate(&assess(&fixtures::stale(), fixtures::NOW));
    assert_eq!(stale.state, SafetyState::TelemetryStale);

    let sim = machine.evaluate(&assess(&fixtures::software_fallback(), fixtures::NOW));
    assert_eq!(sim.state, SafetyState::SimulatedSoftwareOnly);
    assert_eq!(sim.intent, BrakeIntent::None);
}

#[test]
fn slow_and_disconnected_publish_queues_do_not_block_critical_brake() {
    let (queue, rx) = IsolatedPublishQueue::bounded(1);
    drop(rx);
    let mut machine = SafetyMachine::new();
    let mut critical = fixtures::healthy_real();
    critical.gpu_temp_c = Some(90.0);
    let frame = assess(&critical, fixtures::NOW);
    let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &frame, &queue);
    assert_eq!(pub_res, Err(PublishError::Disconnected));
    assert_eq!(snap.state, SafetyState::CriticalBraked);
    assert_eq!(snap.intent, BrakeIntent::Apply);
}

#[test]
fn acquire_without_force_is_not_simulated_on_ci() {
    let raw = HardwareBridge::acquire_raw(false);
    assert_ne!(raw.source, TelemetrySource::SoftwareFallback);
    assert!(matches!(
        raw.source,
        TelemetrySource::Nvml | TelemetrySource::NvmlUnavailable
    ));
}
