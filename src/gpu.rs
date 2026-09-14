//! Hardware Bridge — GPU Telemetry & Voltage Rail Monitoring
//!
//! Raw NVML acquisition lives here. Validation, normalization, freshness, and
//! provenance live in [`crate::telemetry`]. Safety policy consumes the typed
//! frame and never infers simulation from magic numeric values.

use crate::telemetry::{
    RawTelemetry, SampleValidity, TelemetryFrame, TelemetrySource, assess, unix_now_ms,
};
use lazy_static::lazy_static;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};

lazy_static! {
    static ref NVML: Option<Nvml> = Nvml::init().ok();
}

// ── Safety Status ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SafetyStatus {
    Ok,
    Warn(String),
    Critical(String),
}

// ── Hardware Bridge ─────────────────────────────────────────────────

pub struct HardwareBridge;

impl HardwareBridge {
    /// Acquire raw telemetry, then validate/normalize into a [`TelemetryFrame`].
    pub fn read_telemetry() -> TelemetryFrame {
        Self::read_telemetry_force(false)
    }

    /// Read telemetry, but if `force_software` is true, always use the simulated
    /// fallback (never attempt real NVML/nvidia-smi). This implements the
    /// `--force-software-only` CLI flag for #11.
    pub fn read_telemetry_force(force_software: bool) -> TelemetryFrame {
        let raw = Self::acquire_raw(force_software);
        assess(&raw, unix_now_ms())
    }

    /// Raw acquisition only — no validation, no silent zeros for missing sensors.
    pub fn acquire_raw(force_software: bool) -> RawTelemetry {
        if !force_software && let Some(raw) = Self::read_nvml() {
            return raw;
        }
        RawTelemetry::software_fallback(unix_now_ms())
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
                println!("[hardware_bridge] nvidia-smi hung. Bypassing NVML until it recovers.");
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

    /// Check GPU safety thresholds against a validated frame.
    ///
    /// Simulation is [`TelemetrySource::SoftwareFallback`], never inferred from
    /// values such as `temperature <= 0 && power <= 25`. Missing, invalid, or
    /// stale safety-critical signals fail closed.
    ///
    /// Returns `(SafetyStatus, is_simulated)`.
    pub fn check_safety(frame: &TelemetryFrame) -> (SafetyStatus, bool) {
        if frame.source == TelemetrySource::SoftwareFallback {
            return (SafetyStatus::Ok, true);
        }

        if let Some(status) = critical_from_frame(frame) {
            return (status, false);
        }
        if let Some(status) = warn_from_frame(frame) {
            return (status, false);
        }
        (SafetyStatus::Ok, false)
    }

    /// CLOSED LOOP CONTROL: The Emergency Brake.
    /// Throttles GPU power to the given fraction of the device's *default* power limit
    /// (not the current limit) to avoid compounding throttle across restarts.
    /// Fails closed if NVML cannot report a real limit — never invents a hardcoded wattage.
    pub fn apply_emergency_brake(pct: f32) -> Result<(), String> {
        // Prefer default PL as base so restarts cannot stack 50% on an already-braked limit.
        let base_limit = Self::query_default_power_limit_w()
            .or_else(Self::query_power_limit_w)
            .ok_or_else(|| {
                "Cannot query GPU power limit via NVML; refusing arbitrary fallback".to_string()
            })?;
        let pct = pct.clamp(0.1, 1.0);
        let target_pl = (base_limit as f32 * pct) as u32;

        // Already at or below target (e.g. leftover brake from a previous process).
        if let Some(current) = Self::query_power_limit_w().filter(|&c| c <= target_pl) {
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

        Self::set_power_limit_w(target_pl)
    }

    /// Release the emergency brake — restore GPU power limit to its default.
    /// Fails closed if the default cannot be queried (never restores a fabricated wattage).
    pub fn release_emergency_brake() -> Result<(), String> {
        let default_limit = Self::query_default_power_limit_w().ok_or_else(|| {
            "Cannot query default power limit via NVML; refusing to restore an arbitrary value"
                .to_string()
        })?;
        println!(
            "[hardware_bridge] RELEASING BRAKE: Restoring PL to {}W (device default)",
            default_limit
        );

        Self::set_power_limit_w(default_limit)
    }

    /// Returns `(current_w, default_w, expected_brake_w)` only when both limits are known
    /// and the current limit matches this relay's emergency-brake target. This avoids
    /// treating an operator-configured sub-default cap as an app-owned brake to auto-release.
    pub fn power_limit_matches_emergency_brake(pct: f32) -> Option<(u32, u32, u32)> {
        let current = Self::query_power_limit_w()?;
        let default = Self::query_default_power_limit_w()?;
        let expected = (default as f32 * pct.clamp(0.1, 1.0)) as u32;
        let tolerance_w = 2;
        if current.abs_diff(expected) <= tolerance_w {
            Some((current, default, expected))
        } else {
            None
        }
    }

    /// Set GPU power limit via `timeout` + non-interactive `sudo -n` so a password
    /// prompt or wedged nvidia-smi cannot stall the relay loop indefinitely.
    fn set_power_limit_w(limit_w: u32) -> Result<(), String> {
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
            .map_err(|e| format!("Failed to exec nvidia-smi: {e}"))?;

        if !status.success() {
            return Err(format!(
                "nvidia-smi -pl {limit_w} failed (timeout, missing passwordless sudo, or command error)"
            ));
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

fn safety_value(
    name: &str,
    sample: &crate::telemetry::TelemetrySample<f32>,
) -> Result<f32, SafetyStatus> {
    match sample.validity {
        SampleValidity::Missing => Err(SafetyStatus::Critical(format!(
            "Invalid telemetry: {name} missing"
        ))),
        SampleValidity::Invalid => Err(SafetyStatus::Critical(format!(
            "Invalid telemetry: {name} invalid"
        ))),
        SampleValidity::Stale => Err(SafetyStatus::Critical(format!(
            "Invalid telemetry: {name} stale"
        ))),
        SampleValidity::Valid => sample
            .value
            .ok_or_else(|| SafetyStatus::Critical(format!("Invalid telemetry: {name} missing"))),
    }
}

/// Require both safety-critical samples to be valid and present.
/// Missing, invalid, and stale fail closed — never a silent `None`.
fn safety_readings(frame: &TelemetryFrame) -> Result<(f32, f32), SafetyStatus> {
    Ok((
        safety_value("gpu_temp_c", &frame.gpu_temp_c)?,
        safety_value("power_w", &frame.power_w)?,
    ))
}

fn critical_from_frame(frame: &TelemetryFrame) -> Option<SafetyStatus> {
    let (gpu_temp_c, power_w) = match safety_readings(frame) {
        Ok(v) => v,
        Err(status) => return Some(status),
    };

    if gpu_temp_c > 85.0 {
        return Some(SafetyStatus::Critical(format!(
            "GPU thermal: {gpu_temp_c:.0}°C exceeds 85°C"
        )));
    }
    if power_w > 350.0 {
        return Some(SafetyStatus::Critical(format!(
            "GPU power: {power_w:.0}W exceeds 350W safety limit"
        )));
    }
    None
}

fn warn_from_frame(frame: &TelemetryFrame) -> Option<SafetyStatus> {
    let (gpu_temp_c, power_w) = match safety_readings(frame) {
        Ok(v) => v,
        Err(status) => return Some(status),
    };
    if gpu_temp_c > 75.0 {
        return Some(SafetyStatus::Warn(format!(
            "GPU thermal: {gpu_temp_c:.0}°C approaching 85°C limit"
        )));
    }
    if power_w > 300.0 {
        return Some(SafetyStatus::Warn(format!(
            "GPU power: {power_w:.0}W approaching safety limit"
        )));
    }
    None
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
    fn test_warn_from_frame_fail_closes_on_missing_temp_or_power() {
        // Direct call: `?` on Option would return None and skip warn logic.
        // Missing safety-critical samples must fail closed as Critical, same as
        // critical_from_frame / check_safety.
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = None;
        let frame = assess(&raw, fixtures::NOW);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(ref msg) if msg.contains("gpu_temp_c")));
        assert!(!is_sim);

        let mut raw = fixtures::healthy_real();
        raw.power_w = None;
        let frame = assess(&raw, fixtures::NOW);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("power_w"))
        );

        // Warn-band temperature with missing power must not become Warn or Ok.
        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.power_w.value = None;
        frame.power_w.validity = SampleValidity::Missing;
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("power_w"))
        );
        let (status, is_sim) = HardwareBridge::check_safety(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!matches!(status, SafetyStatus::Warn(_)));
        assert!(!is_sim);

        // Valid stamp with None value (invariant break) must not skip via `value?`.
        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.gpu_temp_c.value = None;
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );
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
}
