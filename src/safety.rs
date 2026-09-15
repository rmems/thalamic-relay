//! Pure hardware-safety policy, hysteresis state machine, and actuation boundary.
//!
//! This module deliberately has **no** NVML, subprocess, global-state, or
//! async-runtime dependency. Everything here is a deterministic function of a
//! validated [`TelemetryFrame`] (from [`crate::telemetry`]) plus explicit
//! machine state, so classification and hysteresis can be unit-tested with no
//! GPU and no `nvidia-smi`.
//!
//! Responsibilities (per [GH#46](https://github.com/rmems/thalamic-relay/issues/46)):
//!
//! - [`classify`] — pure Ok/Warn/Critical evaluation of a frame. Missing,
//!   invalid, and stale safety-critical samples fail closed to `Critical`.
//! - [`SafetyStateMachine`] — pure brake/release hysteresis. It only decides
//!   *intent* ([`BrakeCommand`]); it never touches hardware.
//! - [`SafetyActuator`] — the privileged apply/release/detect boundary. The
//!   NVML/`nvidia-smi` backend lives in [`crate::gpu`]; [`FakeActuator`]
//!   provides a deterministic in-memory implementation for tests.
//! - [`ActuatorError`] — typed, observable actuation failures (no stringly
//!   coupling to the main loop).
//!
//! The NVIDIA adapter implements [`SafetyActuator`] but must not define any of
//! the safety semantics above.

use crate::telemetry::{SampleValidity, TelemetryFrame, TelemetrySample};
use std::sync::Mutex;

// ── Thresholds (single source of truth for safety policy) ───────────

/// Thermal warning band (°C), exclusive of the critical threshold.
pub const TEMP_WARN_C: f32 = 75.0;
/// Thermal critical threshold (°C).
pub const TEMP_CRITICAL_C: f32 = 85.0;
/// Power warning band (W), exclusive of the critical threshold.
pub const POWER_WARN_W: f32 = 300.0;
/// Power critical threshold (W).
pub const POWER_CRITICAL_W: f32 = 350.0;

/// Emergency-brake fraction of the device *default* power limit.
pub const BRAKE_FRACTION: f32 = 0.5;
/// Consecutive real (non-simulated) `Ok` evaluations required before releasing.
pub const RELEASE_OK_STREAK: u32 = 3;

// ── Instantaneous classification ────────────────────────────────────

/// Instantaneous Ok / Warn / Critical safety verdict for a single frame.
///
/// Missing, invalid, and stale safety-critical samples are represented as
/// [`Self::Critical`] (fail closed). Simulated software-only frames are
/// [`Self::Ok`] — there is no real GPU to protect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyStatus {
    Ok,
    Warn(String),
    Critical(String),
}

/// Result of classifying a frame: the [`SafetyStatus`] plus whether the frame
/// came from simulated software-only telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyAssessment {
    pub status: SafetyStatus,
    /// `true` when the verdict is based on simulated (`SoftwareFallback`) data.
    pub simulated: bool,
}

impl SafetyAssessment {
    /// Whether the instantaneous verdict is [`SafetyStatus::Ok`].
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.status, SafetyStatus::Ok)
    }
}

/// Classify a validated frame into a [`SafetyAssessment`].
///
/// Simulation is [`crate::telemetry::TelemetrySource::SoftwareFallback`] only.
/// `NvmlUnavailable` is **not** treated as simulated: its missing safety
/// samples fail closed to `Critical`.
#[must_use]
pub fn classify(frame: &TelemetryFrame) -> SafetyAssessment {
    use crate::telemetry::TelemetrySource;

    if frame.source == TelemetrySource::SoftwareFallback {
        return SafetyAssessment {
            status: SafetyStatus::Ok,
            simulated: true,
        };
    }

    if let Some(status) = critical_from_frame(frame) {
        return SafetyAssessment {
            status,
            simulated: false,
        };
    }
    if let Some(status) = warn_from_frame(frame) {
        return SafetyAssessment {
            status,
            simulated: false,
        };
    }
    SafetyAssessment {
        status: SafetyStatus::Ok,
        simulated: false,
    }
}

/// Resolve a single safety-critical sample, failing closed on anything that is
/// not present-and-valid.
fn safety_value(name: &str, sample: &TelemetrySample<f32>) -> Result<f32, SafetyStatus> {
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

    if gpu_temp_c > TEMP_CRITICAL_C {
        return Some(SafetyStatus::Critical(format!(
            "GPU thermal: {gpu_temp_c:.0}°C exceeds {TEMP_CRITICAL_C:.0}°C"
        )));
    }
    if power_w > POWER_CRITICAL_W {
        return Some(SafetyStatus::Critical(format!(
            "GPU power: {power_w:.0}W exceeds {POWER_CRITICAL_W:.0}W safety limit"
        )));
    }
    None
}

fn warn_from_frame(frame: &TelemetryFrame) -> Option<SafetyStatus> {
    let (gpu_temp_c, power_w) = match safety_readings(frame) {
        Ok(v) => v,
        Err(status) => return Some(status),
    };
    if gpu_temp_c > TEMP_WARN_C {
        return Some(SafetyStatus::Warn(format!(
            "GPU thermal: {gpu_temp_c:.0}°C approaching {TEMP_CRITICAL_C:.0}°C limit"
        )));
    }
    if power_w > POWER_WARN_W {
        return Some(SafetyStatus::Warn(format!(
            "GPU power: {power_w:.0}W approaching safety limit"
        )));
    }
    None
}

// ── Hysteresis state machine ────────────────────────────────────────

/// A brake command the supervisor should dispatch to a [`SafetyActuator`].
///
/// The state machine only decides intent; it never performs actuation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrakeCommand {
    /// Engage the emergency brake at [`BRAKE_FRACTION`] of the default limit.
    Apply,
    /// Release the emergency brake, restoring the default limit.
    Release,
}

/// Pure brake/release hysteresis.
///
/// Feed it classified [`SafetyAssessment`]s (via [`Self::observe`]) and the
/// results of dispatched commands (via [`Self::on_apply_result`] /
/// [`Self::on_release_result`]). It returns the next [`BrakeCommand`] to
/// dispatch, if any, tracking in-flight actuation so a command is never
/// dispatched twice.
///
/// Fail-closed behavior is preserved because [`classify`] maps missing/invalid/
/// stale/unavailable telemetry to `Critical`, which drives [`BrakeCommand::Apply`].
#[derive(Debug, Clone)]
pub struct SafetyStateMachine {
    brake_engaged: bool,
    ok_streak: u32,
    apply_in_flight: bool,
    release_in_flight: bool,
}

impl Default for SafetyStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl SafetyStateMachine {
    /// Start with no brake engaged.
    #[must_use]
    pub fn new() -> Self {
        Self {
            brake_engaged: false,
            ok_streak: 0,
            apply_in_flight: false,
            release_in_flight: false,
        }
    }

    /// Start with the brake treated as already engaged.
    ///
    /// Used when startup detection ([`SafetyActuator::detect_engaged_brake`])
    /// finds a leftover brake from a prior crash, so the relay auto-releases it
    /// after a real `Ok` streak instead of leaving the GPU throttled forever.
    #[must_use]
    pub fn with_brake_engaged() -> Self {
        Self {
            brake_engaged: true,
            ..Self::new()
        }
    }

    /// Whether the brake is currently believed to be engaged.
    #[must_use]
    pub fn brake_engaged(&self) -> bool {
        self.brake_engaged
    }

    /// Consecutive real `Ok` evaluations observed toward a release.
    #[must_use]
    pub fn ok_streak(&self) -> u32 {
        self.ok_streak
    }

    /// Whether an apply or release command is currently in flight.
    #[must_use]
    pub fn actuation_in_flight(&self) -> bool {
        self.apply_in_flight || self.release_in_flight
    }

    /// Observe a classified assessment and return the next command to dispatch.
    ///
    /// - `Critical` engages the brake (if not already engaged / in flight).
    /// - `Warn` holds and resets the release streak.
    /// - `Ok` on real telemetry advances the release streak while braked, and
    ///   releases once [`RELEASE_OK_STREAK`] consecutive real `Ok`s are seen.
    /// - `Ok` on simulated telemetry while braked *holds* the brake and resets
    ///   the streak: a simulated frame cannot confirm a safe physical release.
    pub fn observe(&mut self, status: &SafetyStatus, simulated: bool) -> Option<BrakeCommand> {
        match status {
            SafetyStatus::Critical(_) => {
                self.ok_streak = 0;
                if !self.brake_engaged && !self.apply_in_flight {
                    self.apply_in_flight = true;
                    return Some(BrakeCommand::Apply);
                }
                None
            }
            SafetyStatus::Warn(_) => {
                self.ok_streak = 0;
                None
            }
            SafetyStatus::Ok => {
                if !self.brake_engaged {
                    return None;
                }
                if simulated {
                    // Hold the physical brake: simulated telemetry cannot
                    // confirm a safe release.
                    self.ok_streak = 0;
                    return None;
                }
                self.ok_streak = self.ok_streak.saturating_add(1);
                if self.ok_streak >= RELEASE_OK_STREAK
                    && !self.release_in_flight
                    && !self.apply_in_flight
                {
                    self.release_in_flight = true;
                    return Some(BrakeCommand::Release);
                }
                None
            }
        }
    }

    /// Feed back the result of a dispatched [`BrakeCommand::Apply`].
    ///
    /// On success the brake is considered engaged. On failure the brake stays
    /// disengaged so the next `Critical` observation re-dispatches (fail closed).
    /// The release streak is reset either way.
    pub fn on_apply_result(&mut self, succeeded: bool) {
        self.apply_in_flight = false;
        self.ok_streak = 0;
        if succeeded {
            self.brake_engaged = true;
        }
    }

    /// Feed back the result of a dispatched [`BrakeCommand::Release`].
    ///
    /// On success the brake is considered disengaged. On failure the brake
    /// stays engaged and a later `Ok` streak retries the release. The release
    /// streak is reset either way.
    pub fn on_release_result(&mut self, succeeded: bool) {
        self.release_in_flight = false;
        self.ok_streak = 0;
        if succeeded {
            self.brake_engaged = false;
        }
    }
}

// ── Actuation boundary ──────────────────────────────────────────────

/// Typed, observable failure of a privileged actuation attempt.
///
/// Represented explicitly (never a bare `String` in the control loop) so the
/// supervisor and observability layer ([GH#42](https://github.com/rmems/thalamic-relay/issues/42))
/// can react to and export actuator failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActuatorError {
    /// A real power limit could not be queried; refusing an arbitrary fallback.
    PowerLimitUnavailable,
    /// The actuation command failed (subprocess spawn error, timeout, missing
    /// passwordless sudo, or a non-zero exit).
    CommandFailed(String),
}

impl std::fmt::Display for ActuatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PowerLimitUnavailable => write!(
                f,
                "power limit unavailable via backend; refusing arbitrary fallback"
            ),
            Self::CommandFailed(msg) => write!(f, "actuation command failed: {msg}"),
        }
    }
}

impl std::error::Error for ActuatorError {}

/// A detected engaged brake: the current, default, and expected-brake limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrakeMatch {
    pub current_w: u32,
    pub default_w: u32,
    pub expected_w: u32,
}

/// The privileged hardware-safety actuation boundary.
///
/// Implementations apply/release a hardware power-limit brake and detect a
/// leftover brake at startup. The NVML/`nvidia-smi` backend lives in
/// [`crate::gpu`]; [`FakeActuator`] provides a deterministic test double.
///
/// Implementations must be `Send + Sync` so the supervisor can drive them from
/// blocking worker tasks without stalling the telemetry loop.
pub trait SafetyActuator: Send + Sync {
    /// Engage the emergency brake, throttling to `pct` of the device *default*
    /// power limit. Must fail closed with [`ActuatorError`] rather than
    /// inventing a hardcoded wattage.
    fn apply_emergency_brake(&self, pct: f32) -> Result<(), ActuatorError>;

    /// Release the emergency brake, restoring the device default power limit.
    fn release_emergency_brake(&self) -> Result<(), ActuatorError>;

    /// Detect a leftover brake matching this relay's `pct` target (e.g. after a
    /// crash/restart), so the supervisor can adopt and later release it. Returns
    /// `None` when no matching brake is present or the limits cannot be queried.
    fn detect_engaged_brake(&self, pct: f32) -> Option<BrakeMatch>;
}

/// Deterministic in-memory [`SafetyActuator`] for tests.
///
/// Records apply/release call counts, tracks engaged state, and can be
/// configured to fail apply and/or release to exercise fail-closed paths.
#[derive(Debug, Default)]
pub struct FakeActuator {
    state: Mutex<FakeState>,
}

#[derive(Debug, Default, Clone)]
struct FakeState {
    engaged: bool,
    apply_calls: u32,
    release_calls: u32,
    fail_apply: Option<ActuatorError>,
    fail_release: Option<ActuatorError>,
    detected: Option<BrakeMatch>,
}

impl FakeActuator {
    /// A fake with no brake engaged and no injected failures.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure [`SafetyActuator::apply_emergency_brake`] to fail with `err`.
    pub fn set_apply_failure(&self, err: Option<ActuatorError>) {
        self.state.lock().unwrap().fail_apply = err;
    }

    /// Configure [`SafetyActuator::release_emergency_brake`] to fail with `err`.
    pub fn set_release_failure(&self, err: Option<ActuatorError>) {
        self.state.lock().unwrap().fail_release = err;
    }

    /// Configure the [`BrakeMatch`] returned by
    /// [`SafetyActuator::detect_engaged_brake`].
    pub fn set_detected_brake(&self, detected: Option<BrakeMatch>) {
        self.state.lock().unwrap().detected = detected;
    }

    /// Whether the fake brake is currently engaged.
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        self.state.lock().unwrap().engaged
    }

    /// Number of apply calls received.
    #[must_use]
    pub fn apply_calls(&self) -> u32 {
        self.state.lock().unwrap().apply_calls
    }

    /// Number of release calls received.
    #[must_use]
    pub fn release_calls(&self) -> u32 {
        self.state.lock().unwrap().release_calls
    }
}

impl SafetyActuator for FakeActuator {
    fn apply_emergency_brake(&self, _pct: f32) -> Result<(), ActuatorError> {
        let mut state = self.state.lock().unwrap();
        state.apply_calls += 1;
        if let Some(err) = state.fail_apply.clone() {
            return Err(err);
        }
        state.engaged = true;
        Ok(())
    }

    fn release_emergency_brake(&self) -> Result<(), ActuatorError> {
        let mut state = self.state.lock().unwrap();
        state.release_calls += 1;
        if let Some(err) = state.fail_release.clone() {
            return Err(err);
        }
        state.engaged = false;
        Ok(())
    }

    fn detect_engaged_brake(&self, _pct: f32) -> Option<BrakeMatch> {
        self.state.lock().unwrap().detected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{
        SampleValidity, TelemetryFrame, TelemetrySource, assess, fixtures, software_fallback,
    };

    fn nvml_temp_power(temp_c: f32, power_w: f32) -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(temp_c);
        raw.power_w = Some(power_w);
        assess(&raw, fixtures::NOW)
    }

    // ── classification (pure, no GPU / no subprocess) ───────────────

    #[test]
    fn simulated_software_fallback_is_ok_and_flagged() {
        let a = classify(&assess(&fixtures::software_fallback(), fixtures::NOW));
        assert_eq!(a.status, SafetyStatus::Ok);
        assert!(a.simulated);
    }

    #[test]
    fn does_not_infer_simulation_from_old_magic_values() {
        let frame = assess(&fixtures::nvml_looks_like_old_magic(), fixtures::NOW);
        let a = classify(&frame);
        assert_eq!(a.status, SafetyStatus::Ok);
        assert!(!a.simulated);
        assert_eq!(frame.source, TelemetrySource::Nvml);
        assert_eq!(frame.gpu_temp_c.value, Some(0.0));
        assert_eq!(frame.power_w.value, Some(25.0));
    }

    #[test]
    fn warn_on_elevated_temp() {
        let a = classify(&nvml_temp_power(78.0, 200.0));
        assert!(matches!(a.status, SafetyStatus::Warn(_)));
        assert!(!a.simulated);
    }

    #[test]
    fn warn_on_elevated_power() {
        let a = classify(&nvml_temp_power(70.0, 320.0));
        assert!(matches!(a.status, SafetyStatus::Warn(_)));
        assert!(!a.simulated);
    }

    #[test]
    fn critical_on_high_temp() {
        let a = classify(&nvml_temp_power(90.0, 200.0));
        assert!(matches!(a.status, SafetyStatus::Critical(_)));
        assert!(!a.simulated);
    }

    #[test]
    fn critical_on_high_power() {
        let a = classify(&nvml_temp_power(70.0, 360.0));
        assert!(matches!(a.status, SafetyStatus::Critical(_)));
        assert!(!a.simulated);
    }

    #[test]
    fn ok_on_normal_telemetry() {
        let a = classify(&nvml_temp_power(65.0, 200.0));
        assert_eq!(a.status, SafetyStatus::Ok);
        assert!(!a.simulated);
    }

    #[test]
    fn critical_on_unknown_power_with_real_temperature() {
        let frame = assess(&fixtures::sensor_dropout(), fixtures::NOW);
        let a = classify(&frame);
        assert!(matches!(a.status, SafetyStatus::Critical(ref msg) if msg.contains("power_w")));
        assert!(!a.simulated);
        assert_eq!(frame.power_w.validity, SampleValidity::Missing);
        assert_eq!(frame.power_w.value, None);
    }

    #[test]
    fn fail_closes_on_missing_temp_or_power() {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = None;
        let frame = assess(&raw, fixtures::NOW);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );
        let a = classify(&frame);
        assert!(matches!(a.status, SafetyStatus::Critical(ref msg) if msg.contains("gpu_temp_c")));
        assert!(!a.simulated);

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
        let a = classify(&frame);
        assert!(matches!(a.status, SafetyStatus::Critical(_)));
        assert!(!matches!(a.status, SafetyStatus::Warn(_)));

        // Valid stamp with None value (invariant break) must not skip via `value?`.
        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.gpu_temp_c.value = None;
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );
    }

    #[test]
    fn critical_on_non_finite_telemetry() {
        let a = classify(&assess(&fixtures::non_finite(), fixtures::NOW));
        assert!(matches!(a.status, SafetyStatus::Critical(_)));

        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(f32::NAN);
        let a = classify(&assess(&raw, fixtures::NOW));
        assert!(matches!(a.status, SafetyStatus::Critical(_)));

        let mut raw = fixtures::healthy_real();
        raw.power_w = Some(f32::INFINITY);
        let a = classify(&assess(&raw, fixtures::NOW));
        assert!(matches!(a.status, SafetyStatus::Critical(_)));
    }

    #[test]
    fn critical_on_stale_and_out_of_range() {
        let a = classify(&assess(&fixtures::stale(), fixtures::NOW));
        assert!(matches!(a.status, SafetyStatus::Critical(ref msg) if msg.contains("stale")));

        let a = classify(&assess(&fixtures::out_of_range(), fixtures::NOW));
        assert!(matches!(a.status, SafetyStatus::Critical(ref msg) if msg.contains("invalid")));
    }

    #[test]
    fn nvml_unavailable_fail_closes() {
        let frame = assess(&fixtures::nvml_unavailable(), fixtures::NOW);
        let a = classify(&frame);
        assert!(matches!(a.status, SafetyStatus::Critical(ref msg) if msg.contains("missing")));
        assert!(!a.simulated);
        assert_eq!(frame.source, TelemetrySource::NvmlUnavailable);
    }

    #[test]
    fn software_fallback_frame_ok_flagged_simulated() {
        let frame = assess(&fixtures::software_fallback(), fixtures::NOW);
        let a = classify(&frame);
        assert_eq!(a.status, SafetyStatus::Ok);
        assert!(a.simulated);
        assert_eq!(frame.power_w.value, Some(software_fallback::POWER_W));
    }

    // ── state machine (pure hysteresis) ─────────────────────────────

    fn critical() -> SafetyStatus {
        SafetyStatus::Critical("test".into())
    }
    fn warn() -> SafetyStatus {
        SafetyStatus::Warn("test".into())
    }

    #[test]
    fn machine_applies_brake_on_critical() {
        let mut sm = SafetyStateMachine::new();
        assert_eq!(sm.observe(&critical(), false), Some(BrakeCommand::Apply));
        assert!(sm.actuation_in_flight());
        // A second critical while apply is in flight must not re-dispatch.
        assert_eq!(sm.observe(&critical(), false), None);
        assert!(!sm.brake_engaged());
        sm.on_apply_result(true);
        assert!(sm.brake_engaged());
        assert!(!sm.actuation_in_flight());
    }

    #[test]
    fn machine_does_not_reapply_when_already_engaged() {
        let mut sm = SafetyStateMachine::with_brake_engaged();
        assert_eq!(sm.observe(&critical(), false), None);
    }

    #[test]
    fn machine_releases_after_ok_streak() {
        let mut sm = SafetyStateMachine::with_brake_engaged();
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None); // streak 1
        assert_eq!(sm.ok_streak(), 1);
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None); // streak 2
        assert_eq!(
            sm.observe(&SafetyStatus::Ok, false),
            Some(BrakeCommand::Release)
        ); // streak 3
        assert!(sm.actuation_in_flight());
        sm.on_release_result(true);
        assert!(!sm.brake_engaged());
        assert_eq!(sm.ok_streak(), 0);
    }

    #[test]
    fn machine_warn_resets_release_streak() {
        let mut sm = SafetyStateMachine::with_brake_engaged();
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None);
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None);
        assert_eq!(sm.ok_streak(), 2);
        assert_eq!(sm.observe(&warn(), false), None);
        assert_eq!(sm.ok_streak(), 0);
        // Must start the streak over.
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None);
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None);
        assert_eq!(
            sm.observe(&SafetyStatus::Ok, false),
            Some(BrakeCommand::Release)
        );
    }

    #[test]
    fn machine_holds_brake_on_simulated_ok() {
        let mut sm = SafetyStateMachine::with_brake_engaged();
        // Simulated Ok must never advance the streak toward release.
        for _ in 0..10 {
            assert_eq!(sm.observe(&SafetyStatus::Ok, true), None);
            assert_eq!(sm.ok_streak(), 0);
        }
        assert!(sm.brake_engaged());
    }

    #[test]
    fn machine_ok_while_unbraked_is_noop() {
        let mut sm = SafetyStateMachine::new();
        assert_eq!(sm.observe(&SafetyStatus::Ok, false), None);
        assert_eq!(sm.ok_streak(), 0);
        assert!(!sm.brake_engaged());
    }

    #[test]
    fn machine_reapplies_after_failed_release() {
        let mut sm = SafetyStateMachine::with_brake_engaged();
        for _ in 0..2 {
            sm.observe(&SafetyStatus::Ok, false);
        }
        assert_eq!(
            sm.observe(&SafetyStatus::Ok, false),
            Some(BrakeCommand::Release)
        );
        sm.on_release_result(false); // release failed → brake stays engaged
        assert!(sm.brake_engaged());
        assert_eq!(sm.ok_streak(), 0);
        // Streak restarts; a later streak retries the release.
        sm.observe(&SafetyStatus::Ok, false);
        sm.observe(&SafetyStatus::Ok, false);
        assert_eq!(
            sm.observe(&SafetyStatus::Ok, false),
            Some(BrakeCommand::Release)
        );
    }

    #[test]
    fn machine_failed_apply_stays_fail_closed() {
        let mut sm = SafetyStateMachine::new();
        assert_eq!(sm.observe(&critical(), false), Some(BrakeCommand::Apply));
        sm.on_apply_result(false); // apply failed
        assert!(!sm.brake_engaged());
        assert!(!sm.actuation_in_flight());
        // Next critical re-dispatches (fail closed).
        assert_eq!(sm.observe(&critical(), false), Some(BrakeCommand::Apply));
    }

    // ── fake actuator ───────────────────────────────────────────────

    #[test]
    fn fake_actuator_tracks_engage_release() {
        let fake = FakeActuator::new();
        assert!(!fake.is_engaged());
        fake.apply_emergency_brake(BRAKE_FRACTION).unwrap();
        assert!(fake.is_engaged());
        assert_eq!(fake.apply_calls(), 1);
        fake.release_emergency_brake().unwrap();
        assert!(!fake.is_engaged());
        assert_eq!(fake.release_calls(), 1);
    }

    #[test]
    fn fake_actuator_injects_failures() {
        let fake = FakeActuator::new();
        fake.set_apply_failure(Some(ActuatorError::PowerLimitUnavailable));
        assert_eq!(
            fake.apply_emergency_brake(BRAKE_FRACTION),
            Err(ActuatorError::PowerLimitUnavailable)
        );
        assert!(!fake.is_engaged());
        assert_eq!(fake.apply_calls(), 1);

        fake.set_apply_failure(None);
        fake.apply_emergency_brake(BRAKE_FRACTION).unwrap();
        assert!(fake.is_engaged());

        fake.set_release_failure(Some(ActuatorError::CommandFailed("boom".into())));
        assert!(fake.release_emergency_brake().is_err());
        assert!(fake.is_engaged());
    }

    #[test]
    fn fake_actuator_reports_detected_brake() {
        let fake = FakeActuator::new();
        assert_eq!(fake.detect_engaged_brake(BRAKE_FRACTION), None);
        let m = BrakeMatch {
            current_w: 150,
            default_w: 300,
            expected_w: 150,
        };
        fake.set_detected_brake(Some(m));
        assert_eq!(fake.detect_engaged_brake(BRAKE_FRACTION), Some(m));
    }

    #[test]
    fn actuator_error_display_is_stable() {
        assert!(
            ActuatorError::PowerLimitUnavailable
                .to_string()
                .contains("power limit unavailable")
        );
        assert!(
            ActuatorError::CommandFailed("x".into())
                .to_string()
                .contains("x")
        );
    }

    /// End-to-end: pure classify + machine + fake actuator, no GPU/subprocess.
    #[test]
    fn integration_critical_then_recovery_with_fake_actuator() {
        let fake = FakeActuator::new();
        let mut sm = SafetyStateMachine::new();

        // Overheat → apply brake.
        let hot = classify(&nvml_temp_power(95.0, 200.0));
        if let Some(BrakeCommand::Apply) = sm.observe(&hot.status, hot.simulated) {
            sm.on_apply_result(fake.apply_emergency_brake(BRAKE_FRACTION).is_ok());
        }
        assert!(fake.is_engaged());
        assert!(sm.brake_engaged());

        // Three real Ok readings → release.
        let cool = classify(&nvml_temp_power(60.0, 150.0));
        let mut released = false;
        for _ in 0..RELEASE_OK_STREAK {
            if let Some(BrakeCommand::Release) = sm.observe(&cool.status, cool.simulated) {
                sm.on_release_result(fake.release_emergency_brake().is_ok());
                released = true;
            }
        }
        assert!(released);
        assert!(!fake.is_engaged());
        assert!(!sm.brake_engaged());
        assert_eq!(fake.apply_calls(), 1);
        assert_eq!(fake.release_calls(), 1);
    }
}
