//! Hardware Bridge — GPU telemetry acquisition and privileged NVML actuation.
//!
//! Raw NVML acquisition lives here. Validation, normalization, freshness, and
//! provenance live in [`crate::telemetry`]. Pure safety policy — instantaneous
//! classification, brake hysteresis, and the [`SafetyActuator`] boundary — lives
//! in [`crate::safety`].
//!
//! This module is a *backend*: [`NvmlActuator`] implements
//! [`SafetyActuator`] against NVML / `nvidia-smi`, but it defines none of the
//! generic safety semantics (thresholds, hysteresis, fail-closed rules). Those
//! belong to [`crate::safety`].

use crate::safety::{ActuatorError, BrakeMatch, SafetyActuator};
use crate::telemetry::{
    DEFAULT_ACQUISITION_CADENCE_MS, RawTelemetry, TelemetryFrame, TelemetrySource,
    assess_with_cadence, unix_now_ms,
};
use lazy_static::lazy_static;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};

lazy_static! {
    static ref NVML: Option<Nvml> = Nvml::init().ok();
}

// ── Hardware Bridge (telemetry acquisition) ─────────────────────────

pub struct HardwareBridge;

impl HardwareBridge {
    /// Acquire raw telemetry, then validate/normalize into a [`TelemetryFrame`].
    pub fn read_telemetry() -> TelemetryFrame {
        Self::read_telemetry_with(false, DEFAULT_ACQUISITION_CADENCE_MS)
    }

    /// Read telemetry, but if `force_software` is true, always use the simulated
    /// fallback (never attempt real NVML/nvidia-smi). This implements the
    /// `--force-software-only` CLI flag for #11.
    pub fn read_telemetry_force(force_software: bool) -> TelemetryFrame {
        Self::read_telemetry_with(force_software, DEFAULT_ACQUISITION_CADENCE_MS)
    }

    /// Acquire + assess with the configured supervisor tick interval.
    pub fn read_telemetry_with(force_software: bool, cadence_ms: u64) -> TelemetryFrame {
        let raw = Self::acquire_raw(force_software);
        assess_with_cadence(&raw, unix_now_ms(), cadence_ms)
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
            observed_at,
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
}

/// Derive an observability-only Vcore estimate from board power.
/// This is not an NVML voltage sensor; [`crate::telemetry::SignalOrigin::Derived`].
fn derive_vddcr_gfx_v(power_w: f32) -> f32 {
    let p_idle = 50.0_f32;
    let p_tdp = 300.0_f32;
    let v_idle = 0.70_f32;
    let v_tdp = 1.05_f32;
    let t = ((power_w - p_idle) / (p_tdp - p_idle)).clamp(0.0, 1.0);
    v_idle + t * (v_tdp - v_idle)
}

// ── NVML / nvidia-smi safety actuator (privileged backend) ──────────

/// [`SafetyActuator`] backed by NVML (for reads) and `nvidia-smi` (for the
/// privileged power-limit mutation).
///
/// This is a hardware adapter only: the emergency-brake *policy* (when to brake,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{SafetyStatus, classify};
    use crate::telemetry::{SampleValidity, TelemetrySource, software_fallback};

    #[test]
    fn test_raw_software_fallback_never_uses_silent_zero_for_missing_vram() {
        let raw = RawTelemetry::software_fallback(crate::telemetry::fixtures::NOW);
        assert_eq!(raw.source, TelemetrySource::SoftwareFallback);
        assert_eq!(raw.vram_temp_c, None);
        assert_eq!(raw.mem_util_pct, Some(0.0));
        assert_eq!(raw.gpu_temp_c, Some(software_fallback::GPU_TEMP_C));
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
        // Classification of a simulated frame belongs to `crate::safety`.
        let assessment = classify(&frame);
        assert_eq!(assessment.status, SafetyStatus::Ok);
        assert!(assessment.simulated);
    }

    #[test]
    fn test_is_gpu_healthy_does_not_panic() {
        let result = std::panic::catch_unwind(HardwareBridge::is_gpu_healthy);
        assert!(result.is_ok(), "is_gpu_healthy should not panic");
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
    }

    /// Without a real GPU (no NVML), actuation must fail closed with a typed
    /// [`ActuatorError`] rather than inventing a wattage or panicking.
    #[test]
    fn test_nvml_actuator_fails_closed_without_gpu() {
        // These tests run in CI without an NVIDIA device, so NVML queries return
        // None and every actuation path must surface a typed error / no match.
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
}
