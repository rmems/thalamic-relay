//! Hardware Bridge — GPU telemetry acquisition & privileged NVML actuation.
//!
//! Crate-private: the `thalamic-relay` executable uses this adapter; it is not
//! public library API.
//!
//! Raw NVML acquisition lives on [`HardwareBridge`]; the `nvidia-smi`
//! brake/release backend lives on [`NvmlActuator`]. Validation, normalization,
//! freshness, and provenance live in [`crate::telemetry`]. Deterministic safety
//! classification, hysteresis, and the [`SafetyActuator`] boundary live in
//! [`crate::safety`].
//!
//! [`NvmlActuator`] is a hardware *adapter*: it implements [`SafetyActuator`]
//! against NVML / `nvidia-smi`, but defines none of the generic safety
//! semantics (thresholds, hysteresis, fail-closed policy) — those belong to
//! [`crate::safety`].

use crate::safety::{ActuatorError, BrakeMatch, SafetyActuator};
#[cfg(test)]
use crate::safety::{SafetyStatus, instant_status};
#[cfg(test)]
use crate::telemetry::DEFAULT_ACQUISITION_CADENCE_MS;
use crate::telemetry::{
    RawTelemetry, SampleClock, TelemetryFrame, TelemetrySource, TimestampOrigin,
    assess_with_clock, unix_now_ms,
};
use lazy_static::lazy_static;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};

lazy_static! {
    static ref NVML: Option<Nvml> = Nvml::init().ok();
}

// ── Hardware Bridge ─────────────────────────────────────────────────

pub struct HardwareBridge;

impl HardwareBridge {
    /// Read telemetry, but if `force_software` is true, always use the simulated
    /// fallback (never attempt real NVML/nvidia-smi). This implements the
    /// `--force-software-only` CLI flag for #11.
    #[cfg(test)]
    pub fn read_telemetry_force(force_software: bool) -> TelemetryFrame {
        Self::read_telemetry_with(force_software, DEFAULT_ACQUISITION_CADENCE_MS)
    }

    /// Acquire + assess with the configured supervisor tick interval.
    pub fn read_telemetry_with(force_software: bool, cadence_ms: u64) -> TelemetryFrame {
        Self::read_telemetry_with_clock(force_software, cadence_ms, &mut SampleClock::new())
    }

    /// Acquire + assess through a shared session [`SampleClock`].
    pub fn read_telemetry_with_clock(
        force_software: bool,
        cadence_ms: u64,
        clock: &mut SampleClock,
    ) -> TelemetryFrame {
        let raw = Self::acquire_raw(force_software);
        assess_with_clock(&raw, unix_now_ms(), cadence_ms, clock)
    }

    /// Raw acquisition only — no validation, no silent zeros for missing sensors.
    ///
    /// `SoftwareFallback` is used only when `force_software` is true.
    /// NVML/driver/device failure is [`TelemetrySource::NvmlUnavailable`].
    pub fn acquire_raw(force_software: bool) -> RawTelemetry {
        if force_software {
            return RawTelemetry::software_fallback(unix_now_ms());
        }
        Self::read_nvml().unwrap_or_else(|| RawTelemetry::nvml_unavailable(unix_now_ms()))
    }

    /// Returns true if the NVIDIA driver is responsive and the GPU is healthy.
    /// Uses a tight timeout to prevent blocking the supervisor if the driver is "wedged".
    pub fn is_gpu_healthy() -> bool {
        let output = std::process::Command::new("timeout")
            .args(["1s", "nvidia-smi", "-L"])
            .output();

        match output {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    fn read_nvml() -> Option<RawTelemetry> {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Only log on the healthy -> unhealthy transition; the telemetry loop
        // calls this ~10x/sec, so an unconditional print would flood stdout
        // while the driver stays wedged.
        static WAS_UNHEALTHY: AtomicBool = AtomicBool::new(false);

        if !Self::is_gpu_healthy() {
            if !WAS_UNHEALTHY.swap(true, Ordering::Relaxed) {
                println!(
                    "[hardware_bridge] nvidia-smi hung. Treating telemetry as unavailable (fail closed)."
                );
            }
            return None;
        }
        WAS_UNHEALTHY.store(false, Ordering::Relaxed);

        let nvml = NVML.as_ref()?;
        let device = nvml.device_by_index(0).ok()?;
        let observed_at = unix_now_ms();

        let gpu_temp_c = device
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as f32);
        let power_w = device.power_usage().ok().map(|mw| mw as f32 / 1000.0);
        let gpu_clock_mhz = device.clock_info(Clock::Graphics).ok().map(|c| c as f32);
        let mem_clock_mhz = device.clock_info(Clock::Memory).ok().map(|c| c as f32);
        let fan_speed_pct = device.fan_speed(0).ok().map(|s| s as f32);
        let mem_util_pct = device.utilization_rates().ok().map(|u| u.memory as f32);
        let vddcr_gfx_v = power_w.map(derive_vddcr_gfx_v);

        Some(RawTelemetry {
            source_unix_ms: Some(observed_at),
            timestamp_origin: TimestampOrigin::LiveAcquire,
            source: TelemetrySource::Nvml,
            gpu_temp_c,
            // nvml-wrapper 0.10 only exposes TemperatureSensor::Gpu. Do not
            // fabricate VRAM temp as gpu+8 — leave it missing.
            vram_temp_c: None,
            power_w,
            vddcr_gfx_v,
            gpu_clock_mhz,
            mem_clock_mhz,
            fan_speed_pct,
            mem_util_pct,
        })
    }

    /// Check GPU safety thresholds against a validated frame.
    ///
    /// Instantaneous Ok/Warn/Critical only — stateful hysteresis lives in
    /// [`crate::safety::SafetyMachine`]. Simulation is
    /// [`TelemetrySource::SoftwareFallback`] only (forced software-only).
    /// [`TelemetrySource::NvmlUnavailable`] is **not** simulated: missing
    /// safety samples fail closed.
    ///
    /// Returns `(SafetyStatus, is_simulated)`.
    #[cfg(test)]
    pub fn check_safety(frame: &TelemetryFrame) -> (SafetyStatus, bool) {
        instant_status(frame)
    }
}

// ── NVML / nvidia-smi safety actuator (privileged backend, GH#46) ───

/// [`SafetyActuator`] backed by NVML (for reads) and `nvidia-smi` (for the
/// privileged power-limit mutation).
///
/// A hardware adapter only: the emergency-brake *policy* (when to brake,
/// hysteresis, fail-closed rules) is owned by [`crate::safety`]. This type just
/// applies/releases/detects a power-limit brake and reports typed
/// [`ActuatorError`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmlActuator;

impl NvmlActuator {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl SafetyActuator for NvmlActuator {
    /// CLOSED LOOP CONTROL: The Emergency Brake.
    /// Throttles GPU power to `pct` of the device's *default* power limit
    /// (not the current limit) to avoid compounding throttle across restarts.
    /// Fails closed if NVML cannot report a real limit — never invents a hardcoded wattage.
    fn apply_emergency_brake(&self, pct: f32) -> Result<(), ActuatorError> {
        // Prefer default PL as base so restarts cannot stack 50% on an already-braked limit.
        let base_limit = query_default_power_limit_w()
            .or_else(query_power_limit_w)
            .ok_or(ActuatorError::PowerLimitUnavailable)?;
        let pct = pct.clamp(0.1, 1.0);
        let target_pl = (base_limit as f32 * pct) as u32;

        // Already at or below target (e.g. leftover brake from a previous process).
        if let Some(current) = query_power_limit_w().filter(|&c| c <= target_pl) {
            println!(
                "[hardware_bridge] EMERGENCY BRAKE: already at or below target {target_pl}W (current {current}W)"
            );
            return Ok(());
        }

        println!(
            "[hardware_bridge] EMERGENCY BRAKE: Setting PL to {}W ({}% of {}W default)",
            target_pl,
            (pct * 100.0) as u32,
            base_limit
        );

        set_power_limit_w(target_pl)
    }

    /// Release the emergency brake — restore GPU power limit to its default.
    /// Fails closed if the default cannot be queried (never restores a fabricated wattage).
    fn release_emergency_brake(&self) -> Result<(), ActuatorError> {
        let default_limit =
            query_default_power_limit_w().ok_or(ActuatorError::PowerLimitUnavailable)?;
        println!(
            "[hardware_bridge] RELEASING BRAKE: Restoring PL to {}W (device default)",
            default_limit
        );

        set_power_limit_w(default_limit)
    }

    /// Detect a leftover brake whose current limit matches this relay's
    /// emergency-brake target. This avoids treating an operator-configured
    /// sub-default cap as an app-owned brake to auto-release. Returns `None`
    /// when both limits cannot be queried or the current limit does not match.
    fn detect_engaged_brake(&self, pct: f32) -> Option<BrakeMatch> {
        let current_w = query_power_limit_w()?;
        let default_w = query_default_power_limit_w()?;
        let expected_w = (default_w as f32 * pct.clamp(0.1, 1.0)) as u32;
        let tolerance_w = 2;
        if current_w.abs_diff(expected_w) <= tolerance_w {
            Some(BrakeMatch {
                current_w,
                default_w,
                expected_w,
            })
        } else {
            None
        }
    }
}

/// Set GPU power limit via `timeout` + non-interactive `sudo -n` so a password
/// prompt or wedged nvidia-smi cannot stall the relay loop indefinitely.
fn set_power_limit_w(limit_w: u32) -> Result<(), ActuatorError> {
    // -k 2: escalate SIGTERM -> SIGKILL so a wedged nvidia-smi cannot stall forever.
    let status = std::process::Command::new("timeout")
        .args([
            "-k",
            "2",
            "5s",
            "sudo",
            "-n",
            "nvidia-smi",
            "-pl",
            &limit_w.to_string(),
        ])
        .status()
        .map_err(|e| ActuatorError::CommandFailed(format!("Failed to exec nvidia-smi: {e}")))?;

    if !status.success() {
        return Err(ActuatorError::CommandFailed(format!(
            "nvidia-smi -pl {limit_w} failed (timeout, missing passwordless sudo, or command error)"
        )));
    }
    Ok(())
}

/// Query the GPU's current power management limit in watts via NVML.
fn query_power_limit_w() -> Option<u32> {
    let nvml = NVML.as_ref()?;
    let device = nvml.device_by_index(0).ok()?;
    let limit_mw = device.power_management_limit().ok()?;
    Some(limit_mw / 1000)
}

/// Query the GPU's default (enforced) power management limit in watts via NVML.
fn query_default_power_limit_w() -> Option<u32> {
    let nvml = NVML.as_ref()?;
    let device = nvml.device_by_index(0).ok()?;
    let limit_mw = device.power_management_limit_default().ok()?;
    Some(limit_mw / 1000)
}

/// Derive an observability-only Vcore estimate from board power.
/// This is not an NVML voltage sensor; [`crate::telemetry::SignalOrigin::Derived`].
pub(crate) fn derive_vddcr_gfx_v(power_w: f32) -> f32 {
    let p_idle = 50.0_f32;
    let p_tdp = 300.0_f32;
    let v_idle = 0.70_f32;
    let v_tdp = 1.05_f32;
    let t = ((power_w - p_idle) / (p_tdp - p_idle)).clamp(0.0, 1.0);
    v_idle + t * (v_tdp - v_idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{SampleValidity, TelemetrySource, assess, fixtures, software_fallback};

    fn nvml_temp_power(temp_c: f32, power_w: f32) -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(temp_c);
        raw.power_w = Some(power_w);
        assess(&raw, fixtures::NOW)
    }

    #[test]
    fn test_raw_software_fallback_never_uses_silent_zero_for_missing_vram() {
        let raw = RawTelemetry::software_fallback(fixtures::NOW);
        assert_eq!(raw.source, TelemetrySource::SoftwareFallback);
        assert_eq!(raw.vram_temp_c, None);
        assert_eq!(raw.mem_util_pct, Some(0.0));
        assert_eq!(raw.gpu_temp_c, Some(software_fallback::GPU_TEMP_C));
    }

    #[test]
    fn test_safety_ok_on_simulated_values() {
        let frame = assess(&fixtures::software_fallback(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert_eq!(status, SafetyStatus::Ok);
        assert!(is_sim);
    }

    #[test]
    fn test_safety_does_not_infer_simulation_from_old_magic_values() {
        let frame = assess(&fixtures::nvml_looks_like_old_magic(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert_eq!(status, SafetyStatus::Ok);
        assert!(!is_sim);
        assert_eq!(frame.source, TelemetrySource::Nvml);
        assert_eq!(frame.gpu_temp_c.value, Some(0.0));
        assert_eq!(frame.power_w.value, Some(25.0));
    }

    #[test]
    fn test_safety_warn_on_elevated_temp() {
        let frame = nvml_temp_power(78.0, 200.0);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Warn(_)));
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_warn_on_elevated_power() {
        let frame = nvml_temp_power(70.0, 320.0);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Warn(_)));
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_critical_on_high_temp() {
        let frame = nvml_temp_power(90.0, 200.0);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_critical_on_high_power() {
        let frame = nvml_temp_power(70.0, 360.0);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_ok_on_normal_telemetry() {
        let frame = nvml_temp_power(65.0, 200.0);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert_eq!(status, SafetyStatus::Ok);
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_critical_on_unknown_power_with_real_temperature() {
        let frame = assess(&fixtures::sensor_dropout(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("power_w")));
        assert!(!is_sim);
        assert_eq!(frame.power_w.validity, SampleValidity::Missing);
        assert_eq!(frame.power_w.value, None);
    }

    #[test]
    fn test_check_safety_fail_closes_on_missing_temp_or_power() {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = None;
        let frame = assess(&raw, fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("gpu_temp_c")));
        assert!(!is_sim);

        let mut raw = fixtures::healthy_real();
        raw.power_w = None;
        let frame = assess(&raw, fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("power_w")));
        assert!(!is_sim);

        // Warn-band temperature with missing power must not become Warn or Ok.
        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.power_w.value = None;
        frame.power_w.validity = SampleValidity::Missing;
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!matches!(status, SafetyStatus::Warn(_)));
        assert!(!is_sim);

        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.gpu_temp_c.value = None;
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        let (status, _) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("gpu_temp_c")));
    }

    #[test]
    fn test_safety_critical_on_non_finite_telemetry() {
        let frame = assess(&fixtures::non_finite(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!is_sim);

        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(f32::NAN);
        let frame = assess(&raw, fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!is_sim);

        let mut raw = fixtures::healthy_real();
        raw.power_w = Some(f32::INFINITY);
        let frame = assess(&raw, fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!is_sim);
    }

    #[test]
    fn test_safety_critical_on_stale_and_out_of_range() {
        let stale = assess(&fixtures::stale(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&stale);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("stale")));
        assert!(!is_sim);

        let oor = assess(&fixtures::out_of_range(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&oor);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("invalid")));
        assert!(!is_sim);
    }

    #[test]
    fn test_acquire_raw_force_software_is_fallback_not_unavailable() {
        let raw = HardwareBridge::acquire_raw(true);
        assert_eq!(raw.source, TelemetrySource::SoftwareFallback);
        let unavail = HardwareBridge::acquire_raw(false);
        assert!(
            matches!(
                unavail.source,
                TelemetrySource::Nvml | TelemetrySource::NvmlUnavailable
            ),
            "acquire_raw(false) must not be SoftwareFallback, got {:?}",
            unavail.source
        );
        assert_ne!(unavail.source, TelemetrySource::SoftwareFallback);
    }

    #[test]
    fn test_nvml_unavailable_fail_closes_safety() {
        let frame = assess(&fixtures::nvml_unavailable(), fixtures::NOW);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("missing")));
        assert!(!is_sim);
        assert_eq!(frame.source, TelemetrySource::NvmlUnavailable);
        assert_eq!(frame.gpu_temp_c.value, None);
        assert_eq!(frame.power_w.value, None);
    }

    #[test]
    fn test_read_telemetry_with_records_configured_cadence() {
        let frame = HardwareBridge::read_telemetry_with(true, 50);
        assert_eq!(frame.acquisition_cadence_ms, 50);
        assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
    }

    #[test]
    fn test_read_telemetry_force_software_only() {
        let frame = HardwareBridge::read_telemetry_force(true);
        assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
        assert_eq!(frame.power_w.value, Some(software_fallback::POWER_W));
        assert_eq!(frame.gpu_temp_c.value, Some(software_fallback::GPU_TEMP_C));
        assert_eq!(
            frame.gpu_clock_mhz.value,
            Some(software_fallback::GPU_CLOCK_MHZ)
        );
        assert_eq!(
            frame.mem_clock_mhz.value,
            Some(software_fallback::MEM_CLOCK_MHZ)
        );
        assert_eq!(
            frame.fan_speed_pct.value,
            Some(software_fallback::FAN_SPEED_PCT)
        );
        assert_eq!(
            frame.mem_util_pct.value,
            Some(software_fallback::MEM_UTIL_PCT)
        );
        assert_eq!(frame.vram_temp_c.value, None);
        assert_eq!(frame.vram_temp_c.validity, SampleValidity::Missing);
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert_eq!(status, SafetyStatus::Ok);
        assert!(is_sim);
    }

    #[test]
    fn test_is_gpu_healthy_does_not_panic() {
        let result = std::panic::catch_unwind(HardwareBridge::is_gpu_healthy);
        assert!(result.is_ok(), "is_gpu_healthy should not panic");
    }

    /// Without a real GPU (no NVML), actuation must fail closed with a typed
    /// [`ActuatorError`] rather than inventing a wattage or panicking.
    #[test]
    fn test_nvml_actuator_fails_closed_without_gpu() {
        // In CI there is no NVIDIA device, so NVML queries return None and every
        // actuation path must surface a typed error / no match.
        if query_default_power_limit_w().is_some() || query_power_limit_w().is_some() {
            // A real GPU is present (unusual in CI) — skip the fail-closed asserts.
            return;
        }
        let actuator = NvmlActuator::new();
        assert_eq!(
            actuator.apply_emergency_brake(0.5),
            Err(ActuatorError::PowerLimitUnavailable)
        );
        assert_eq!(
            actuator.release_emergency_brake(),
            Err(ActuatorError::PowerLimitUnavailable)
        );
        assert_eq!(actuator.detect_engaged_brake(0.5), None);
    }

    #[test]
    fn test_sensory_mapping_from_software_fallback_carries_provenance() {
        let frame = HardwareBridge::read_telemetry_force(true);
        let mapping = frame.to_sensory_mapping();
        assert_eq!(
            mapping.acquisition_source,
            TelemetrySource::SoftwareFallback
        );
        assert!(
            mapping
                .stimuli
                .iter()
                .all(|s| s.source == TelemetrySource::SoftwareFallback)
        );
        let util = mapping
            .stimuli
            .iter()
            .find(|s| s.name == "mem_util_pct")
            .unwrap();
        assert_eq!(util.raw, Some(0.0));
        assert_eq!(util.normalized, Some(0.0));
        assert_eq!(util.validity, SampleValidity::Valid);
        assert_eq!(mapping.timestamp_origin, TimestampOrigin::Simulated);
        assert_eq!(mapping.batch_id, 0);
        assert!(!mapping.session_id.is_empty());
        assert!(mapping.source_unix_ms.is_some());
        assert!(
            mapping.received_at_unix_ms >= mapping.source_unix_ms.unwrap(),
            "receive time is at or after source time on the live simulated path"
        );
    }

    #[test]
    fn test_software_fallback_frames_share_sample_clock_sequence() {
        let mut clock = SampleClock::with_session_id("hw-session");
        let a = HardwareBridge::read_telemetry_with_clock(true, 50, &mut clock);
        let b = HardwareBridge::read_telemetry_with_clock(true, 50, &mut clock);
        assert_eq!(a.session_id, "hw-session");
        assert_eq!(a.session_id, b.session_id);
        assert_eq!(a.batch_id, 0);
        assert_eq!(b.batch_id, 1);
        assert_eq!(a.timestamp_origin, TimestampOrigin::Simulated);
        assert_eq!(b.timestamp_origin, TimestampOrigin::Simulated);
        assert!(b.received_at_unix_ms >= a.received_at_unix_ms);
    }

    #[test]
    fn test_derive_vddcr_gfx_v_is_deterministic() {
        assert!((derive_vddcr_gfx_v(50.0) - 0.70).abs() < 1e-6);
        assert!((derive_vddcr_gfx_v(0.0) - 0.70).abs() < 1e-6);
        assert!((derive_vddcr_gfx_v(300.0) - 1.05).abs() < 1e-6);
        assert!((derive_vddcr_gfx_v(400.0) - 1.05).abs() < 1e-6);
        let mid = derive_vddcr_gfx_v(175.0);
        assert!((mid - 0.875).abs() < 1e-6);
    }

    #[test]
    fn test_read_telemetry_without_force_is_never_software_fallback() {
        let frame = HardwareBridge::read_telemetry_force(false);
        assert_ne!(frame.source, TelemetrySource::SoftwareFallback);
        assert!(matches!(
            frame.source,
            TelemetrySource::Nvml | TelemetrySource::NvmlUnavailable
        ));
    }
}
