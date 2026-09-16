//! Isolated hardware-safety failure domain.
//!
//! Classification, hysteresis, and brake *intent* are pure functions of a
//! [`TelemetryFrame`] plus machine state. This module has no corpus-ipc,
//! Brainstem, NVML, subprocess, or async-runtime dependency. Privileged
//! power-limit actuation is performed by the `thalamic-relay` executable
//! through [`SafetyActuator`]; sensory publication lives in [`crate::publish`]
//! and must never be awaited on this path.
//!
//! Transition rules: [`docs/safety.md`](../../docs/safety.md).

use crate::telemetry::{SampleValidity, TelemetryFrame, TelemetrySample, TelemetrySource};
use std::sync::Mutex;

/// Consecutive real [`AssessmentKind::Ok`] evaluations required before a release intent.
pub const RELEASE_OK_STREAK: u32 = 3;
/// Emergency-brake fraction of the device default power limit.
pub const BRAKE_FRACTION: f32 = 0.5;
/// Thermal warning band (°C), inclusive of this threshold.
pub const TEMP_WARN_C: f32 = 75.0;
/// Thermal critical threshold (°C).
pub const TEMP_CRITICAL_C: f32 = 85.0;
/// Power warning band (W), inclusive of this threshold.
pub const POWER_WARN_W: f32 = 300.0;
/// Power critical threshold (W).
pub const POWER_CRITICAL_W: f32 = 350.0;

/// Instantaneous Ok / Warn / Critical used by logs and [`instant_status`].
///
/// Missing, invalid, and stale safety samples are [`Self::Critical`] (fail
/// closed). Simulated software-only frames are [`Self::Ok`] — there is no
/// real GPU to protect. Stateful relay names live in [`SafetyState`].
#[derive(Debug, Clone, PartialEq)]
pub enum SafetyStatus {
    /// Safety samples present and below warn thresholds (or software-fallback).
    Ok,
    /// Thermal or power warn band; reason is human-readable.
    Warn(String),
    /// Fail-closed or critical thermal/power; reason is human-readable.
    Critical(String),
}

/// Named relay/safety state. Observable without querying neural runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SafetyState {
    /// Real NVML, valid, below warn, brake not engaged.
    HealthyReal,
    /// Real, valid, warn band. Brake is not newly applied (held if already on).
    Warning,
    /// Thermal/power critical; brake desired (and engaged once actuation succeeds).
    CriticalBraked,
    /// Brake still engaged; consecutive real Ok streak toward release.
    Recovering,
    /// Safety-critical sample missing (`NvmlUnavailable` or sensor dropout).
    TelemetryMissing,
    /// Safety-critical sample older than its stale threshold.
    TelemetryStale,
    /// Safety-critical sample non-finite or out of engineering range.
    TelemetryInvalid,
    /// `--force-software-only` (`TelemetrySource::SoftwareFallback`).
    SimulatedSoftwareOnly,
    /// Last apply/release command failed. Policy still evaluates underneath.
    ActuatorFailure,
}

impl SafetyState {
    /// All named states, in stable metric-id order.
    pub const ALL: [Self; 9] = [
        Self::HealthyReal,
        Self::Warning,
        Self::CriticalBraked,
        Self::Recovering,
        Self::TelemetryMissing,
        Self::TelemetryStale,
        Self::TelemetryInvalid,
        Self::SimulatedSoftwareOnly,
        Self::ActuatorFailure,
    ];

    /// Stable Prometheus numeric id (0…8). Do not reorder [`Self::ALL`].
    #[must_use]
    pub const fn as_id(self) -> u8 {
        match self {
            Self::HealthyReal => 0,
            Self::Warning => 1,
            Self::CriticalBraked => 2,
            Self::Recovering => 3,
            Self::TelemetryMissing => 4,
            Self::TelemetryStale => 5,
            Self::TelemetryInvalid => 6,
            Self::SimulatedSoftwareOnly => 7,
            Self::ActuatorFailure => 8,
        }
    }

    /// Prometheus `state` label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HealthyReal => "healthy_real",
            Self::Warning => "warning",
            Self::CriticalBraked => "critical_braked",
            Self::Recovering => "recovering",
            Self::TelemetryMissing => "telemetry_missing",
            Self::TelemetryStale => "telemetry_stale",
            Self::TelemetryInvalid => "telemetry_invalid",
            Self::SimulatedSoftwareOnly => "simulated_software_only",
            Self::ActuatorFailure => "actuator_failure",
        }
    }
}

/// Brake command the supervisor may dispatch to a [`SafetyActuator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrakeIntent {
    /// No apply/release this step.
    None,
    /// Supervisor should apply the emergency brake.
    Apply,
    /// Supervisor should release the emergency brake.
    Release,
}

/// Result of a privileged apply/release attempt. Fed back into the machine;
/// the machine never executes the command itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActuatorOutcome {
    /// Brake apply succeeded.
    Applied,
    /// Brake apply failed; the string is a log-oriented reason.
    ApplyFailed(String),
    /// Brake release succeeded.
    Released,
    /// Brake release failed; the string is a log-oriented reason.
    ReleaseFailed(String),
}

/// Stateless classification of one frame (no hysteresis, no actuator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssessmentKind {
    /// [`crate::telemetry::TelemetrySource::SoftwareFallback`]; skips thresholds.
    Simulated,
    /// Safety-critical sample missing.
    Missing,
    /// Safety-critical sample non-finite or out of engineering range.
    Invalid,
    /// Safety-critical sample older than its stale threshold.
    Stale,
    /// Valid samples above critical thermal/power limits.
    Critical,
    /// Valid samples in the warn band.
    Warn,
    /// Valid samples below warn thresholds.
    Ok,
}

/// Frame assessment with a human-readable reason for logs/metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameAssessment {
    /// Stateless kind (no hysteresis).
    pub kind: AssessmentKind,
    /// Human-readable reason suitable for logs and metrics.
    pub reason: String,
}

/// Observable snapshot after [`SafetyMachine::evaluate`] or actuator feedback.
#[derive(Debug, Clone, PartialEq)]
pub struct SafetySnapshot {
    /// Reported state (actuator-failure overlays policy when the last command failed).
    pub state: SafetyState,
    /// Policy classification ignoring actuator overlay.
    pub policy_state: SafetyState,
    /// Whether the last successful apply is still claimed.
    pub brake_engaged: bool,
    /// Whether policy currently wants the brake on.
    pub desired_brake: bool,
    /// Apply / release / none derived from desired vs engaged.
    pub intent: BrakeIntent,
    /// Consecutive real Ok evaluations while braked (release hysteresis).
    pub hysteresis_ok_count: u32,
    /// Reason from the latest classification or actuator feedback.
    pub last_reason: String,
    /// Last apply/release error, if the overlay is active.
    pub last_actuator_error: Option<String>,
    /// Named-state change this step, if any.
    pub transition: Option<(SafetyState, SafetyState)>,
    /// True when this step recorded a new actuator failure.
    pub actuator_failed: bool,
}

impl SafetySnapshot {
    /// Derive [`BrakeIntent`] from desired vs currently engaged brake.
    #[must_use]
    pub fn intent_from(desired_brake: bool, brake_engaged: bool) -> BrakeIntent {
        match (desired_brake, brake_engaged) {
            (true, false) => BrakeIntent::Apply,
            (false, true) => BrakeIntent::Release,
            _ => BrakeIntent::None,
        }
    }
}

/// Deterministic safety state machine. Not coupled to IPC or GPU actuation.
///
/// ```
/// use thalamic_relay::safety::{SafetyMachine, SafetyState};
/// use thalamic_relay::telemetry::{assess, fixtures};
///
/// let mut machine = SafetyMachine::new();
/// let frame = assess(&fixtures::healthy_real(), fixtures::NOW);
/// let snap = machine.evaluate(&frame);
/// assert_eq!(snap.state, SafetyState::HealthyReal);
/// ```
#[derive(Debug, Clone)]
pub struct SafetyMachine {
    brake_engaged: bool,
    ok_count: u32,
    just_released: bool,
    last_actuator_error: Option<String>,
    last_policy_state: SafetyState,
    state: SafetyState,
    last_reason: String,
    transitions_total: u64,
    actuator_failures_total: u64,
}

impl Default for SafetyMachine {
    /// Same as [`Self::new`]: fail-closed (`TelemetryMissing`) until the first frame.
    fn default() -> Self {
        Self::new()
    }
}

impl SafetyMachine {
    /// Fail-closed until the first frame is evaluated: missing telemetry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            brake_engaged: false,
            ok_count: 0,
            just_released: false,
            last_actuator_error: None,
            last_policy_state: SafetyState::TelemetryMissing,
            state: SafetyState::TelemetryMissing,
            last_reason: "no telemetry evaluated yet".to_string(),
            transitions_total: 0,
            actuator_failures_total: 0,
        }
    }

    /// Consecutive named-state changes observed by this machine.
    #[must_use]
    pub fn transitions_total(&self) -> u64 {
        self.transitions_total
    }

    /// Actuator apply/release failures recorded by this machine.
    #[must_use]
    pub fn actuator_failures_total(&self) -> u64 {
        self.actuator_failures_total
    }

    /// Seed leftover hardware throttle detected at process start.
    pub fn seed_brake_applied(&mut self) {
        self.brake_engaged = true;
        self.ok_count = 0;
        self.just_released = false;
        self.last_policy_state = SafetyState::Recovering;
        self.state = SafetyState::Recovering;
        self.last_reason = "leftover emergency brake detected at startup".to_string();
    }

    /// Current snapshot without advancing hysteresis.
    #[must_use]
    pub fn snapshot(&self) -> SafetySnapshot {
        self.compose_snapshot(false, None)
    }

    /// Evaluate one telemetry frame. Does not publish, sleep, or actuate.
    #[must_use]
    pub fn evaluate(&mut self, frame: &TelemetryFrame) -> SafetySnapshot {
        let assessment = classify_frame(frame);
        let just_released = self.just_released;
        self.just_released = false;

        let (policy_state, desired_brake) = self.policy_for(&assessment, just_released);
        if policy_state == SafetyState::HealthyReal && !desired_brake {
            // Healthy Ok must not keep a stale ActuatorFailure overlay forever.
            self.last_actuator_error = None;
        }
        self.last_policy_state = policy_state;
        self.last_reason = assessment.reason;
        self.compose_and_store(desired_brake, false)
    }

    /// Record a completed actuator attempt. Does not re-read telemetry.
    #[must_use]
    pub fn record_actuator(&mut self, outcome: ActuatorOutcome) -> SafetySnapshot {
        let mut actuator_failed = false;
        match outcome {
            ActuatorOutcome::Applied => {
                self.brake_engaged = true;
                self.last_actuator_error = None;
                self.just_released = false;
            }
            ActuatorOutcome::ApplyFailed(err) => {
                self.brake_engaged = false;
                self.last_actuator_error = Some(err);
                self.actuator_failures_total = self.actuator_failures_total.saturating_add(1);
                actuator_failed = true;
            }
            ActuatorOutcome::Released => {
                self.brake_engaged = false;
                self.ok_count = 0;
                self.just_released = true;
                self.last_actuator_error = None;
                // Optimistic until the supervisor re-evaluates post-release
                // telemetry. Warn/Critical on that frame re-applies.
                self.last_policy_state = SafetyState::HealthyReal;
                self.last_reason = "brake released".to_string();
            }
            ActuatorOutcome::ReleaseFailed(err) => {
                self.last_actuator_error = Some(err);
                self.actuator_failures_total = self.actuator_failures_total.saturating_add(1);
                actuator_failed = true;
            }
        }
        self.compose_and_store(self.desired_from_last_policy(), actuator_failed)
    }

    fn policy_for(
        &mut self,
        assessment: &FrameAssessment,
        just_released: bool,
    ) -> (SafetyState, bool) {
        match assessment.kind {
            AssessmentKind::Simulated => {
                self.ok_count = 0;
                (SafetyState::SimulatedSoftwareOnly, self.brake_engaged)
            }
            AssessmentKind::Missing => self.fail_closed(SafetyState::TelemetryMissing),
            AssessmentKind::Invalid => self.fail_closed(SafetyState::TelemetryInvalid),
            AssessmentKind::Stale => self.fail_closed(SafetyState::TelemetryStale),
            AssessmentKind::Critical => self.fail_closed(SafetyState::CriticalBraked),
            AssessmentKind::Warn => {
                self.ok_count = 0;
                let desired = self.brake_engaged || just_released;
                (SafetyState::Warning, desired)
            }
            AssessmentKind::Ok if self.brake_engaged => {
                self.ok_count = self.ok_count.saturating_add(1);
                let desired = self.ok_count < RELEASE_OK_STREAK;
                (SafetyState::Recovering, desired)
            }
            AssessmentKind::Ok => {
                self.ok_count = 0;
                (SafetyState::HealthyReal, false)
            }
        }
    }

    fn fail_closed(&mut self, state: SafetyState) -> (SafetyState, bool) {
        self.ok_count = 0;
        (state, true)
    }

    fn desired_from_last_policy(&self) -> bool {
        match self.last_policy_state {
            SafetyState::HealthyReal => false,
            SafetyState::SimulatedSoftwareOnly => self.brake_engaged,
            SafetyState::Warning => self.brake_engaged || self.just_released,
            SafetyState::Recovering => self.ok_count < RELEASE_OK_STREAK,
            SafetyState::CriticalBraked
            | SafetyState::TelemetryMissing
            | SafetyState::TelemetryStale
            | SafetyState::TelemetryInvalid
            | SafetyState::ActuatorFailure => true,
        }
    }

    fn compose_and_store(&mut self, desired_brake: bool, actuator_failed: bool) -> SafetySnapshot {
        let prev = self.state;
        let reported = if self.last_actuator_error.is_some() {
            SafetyState::ActuatorFailure
        } else {
            self.last_policy_state
        };
        let transition = if prev != reported {
            self.transitions_total = self.transitions_total.saturating_add(1);
            Some((prev, reported))
        } else {
            None
        };
        self.state = reported;
        SafetySnapshot {
            state: reported,
            policy_state: self.last_policy_state,
            brake_engaged: self.brake_engaged,
            desired_brake,
            intent: SafetySnapshot::intent_from(desired_brake, self.brake_engaged),
            hysteresis_ok_count: self.ok_count,
            last_reason: self.last_reason.clone(),
            last_actuator_error: self.last_actuator_error.clone(),
            transition,
            actuator_failed,
        }
    }

    fn compose_snapshot(
        &self,
        actuator_failed: bool,
        transition: Option<(SafetyState, SafetyState)>,
    ) -> SafetySnapshot {
        let desired = self.desired_from_last_policy();
        SafetySnapshot {
            state: self.state,
            policy_state: self.last_policy_state,
            brake_engaged: self.brake_engaged,
            desired_brake: desired,
            intent: SafetySnapshot::intent_from(desired, self.brake_engaged),
            hysteresis_ok_count: self.ok_count,
            last_reason: self.last_reason.clone(),
            last_actuator_error: self.last_actuator_error.clone(),
            transition,
            actuator_failed,
        }
    }
}

/// Instantaneous status (no hysteresis). Same classification as [`classify_frame`].
#[must_use]
pub fn instant_status(frame: &TelemetryFrame) -> (SafetyStatus, bool) {
    let assessment = classify_frame(frame);
    match assessment.kind {
        AssessmentKind::Simulated => (SafetyStatus::Ok, true),
        AssessmentKind::Missing
        | AssessmentKind::Invalid
        | AssessmentKind::Stale
        | AssessmentKind::Critical => (SafetyStatus::Critical(assessment.reason), false),
        AssessmentKind::Warn => (SafetyStatus::Warn(assessment.reason), false),
        AssessmentKind::Ok => (SafetyStatus::Ok, false),
    }
}

/// Classify a frame without hysteresis. Simulation is provenance-only.
#[must_use]
pub fn classify_frame(frame: &TelemetryFrame) -> FrameAssessment {
    if frame.source == TelemetrySource::SoftwareFallback {
        return FrameAssessment {
            kind: AssessmentKind::Simulated,
            reason: "telemetry source is SoftwareFallback".to_string(),
        };
    }

    if let Some(fault) = worst_safety_fault(frame) {
        return fault;
    }

    let (gpu_temp_c, power_w) = match safety_readings(frame) {
        Ok(v) => v,
        Err(status) => {
            return FrameAssessment {
                kind: kind_from_critical_status(&status),
                reason: status_reason(status),
            };
        }
    };

    if gpu_temp_c > TEMP_CRITICAL_C {
        return FrameAssessment {
            kind: AssessmentKind::Critical,
            reason: format!("GPU thermal: {gpu_temp_c:.0}°C exceeds {TEMP_CRITICAL_C:.0}°C"),
        };
    }
    if power_w > POWER_CRITICAL_W {
        return FrameAssessment {
            kind: AssessmentKind::Critical,
            reason: format!("GPU power: {power_w:.0}W exceeds {POWER_CRITICAL_W:.0}W safety limit"),
        };
    }
    if gpu_temp_c >= TEMP_WARN_C {
        return FrameAssessment {
            kind: AssessmentKind::Warn,
            reason: format!(
                "GPU thermal: {gpu_temp_c:.0}°C approaching {TEMP_CRITICAL_C:.0}°C limit"
            ),
        };
    }
    if power_w >= POWER_WARN_W {
        return FrameAssessment {
            kind: AssessmentKind::Warn,
            reason: format!("GPU power: {power_w:.0}W approaching safety limit"),
        };
    }
    FrameAssessment {
        kind: AssessmentKind::Ok,
        reason: "healthy real telemetry".to_string(),
    }
}

/// Direct warn-path helper: missing/invalid/stale fail closed as Critical.
/// Kept so the fail-closed warn regression does not skip via `Option::?`.
#[cfg(test)]
#[must_use]
fn warn_from_frame(frame: &TelemetryFrame) -> Option<SafetyStatus> {
    match classify_frame(frame).kind {
        AssessmentKind::Missing
        | AssessmentKind::Invalid
        | AssessmentKind::Stale
        | AssessmentKind::Critical => {
            let (status, _) = instant_status(frame);
            Some(status)
        }
        AssessmentKind::Warn => {
            let (status, _) = instant_status(frame);
            Some(status)
        }
        AssessmentKind::Ok | AssessmentKind::Simulated => None,
    }
}

fn kind_from_critical_status(status: &SafetyStatus) -> AssessmentKind {
    match status {
        SafetyStatus::Critical(msg) if msg.contains("missing") => AssessmentKind::Missing,
        SafetyStatus::Critical(msg) if msg.contains("invalid") => AssessmentKind::Invalid,
        SafetyStatus::Critical(msg) if msg.contains("stale") => AssessmentKind::Stale,
        SafetyStatus::Critical(_) => AssessmentKind::Critical,
        SafetyStatus::Warn(_) => AssessmentKind::Warn,
        SafetyStatus::Ok => AssessmentKind::Ok,
    }
}

fn status_reason(status: SafetyStatus) -> String {
    match status {
        SafetyStatus::Ok => "ok".to_string(),
        SafetyStatus::Warn(msg) | SafetyStatus::Critical(msg) => msg,
    }
}

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

fn safety_readings(frame: &TelemetryFrame) -> Result<(f32, f32), SafetyStatus> {
    Ok((
        safety_value("gpu_temp_c", &frame.gpu_temp_c)?,
        safety_value("power_w", &frame.power_w)?,
    ))
}

fn sample_fault_rank(sample: &TelemetrySample<f32>, name: &str) -> Option<(u8, FrameAssessment)> {
    let (rank, kind, label) = match sample.validity {
        SampleValidity::Valid => {
            if sample.value.is_none() {
                (3, AssessmentKind::Missing, "missing")
            } else {
                return None;
            }
        }
        SampleValidity::Stale => (1, AssessmentKind::Stale, "stale"),
        SampleValidity::Invalid => (2, AssessmentKind::Invalid, "invalid"),
        SampleValidity::Missing => (3, AssessmentKind::Missing, "missing"),
    };
    Some((
        rank,
        FrameAssessment {
            kind,
            reason: format!("Invalid telemetry: {name} {label}"),
        },
    ))
}

fn worst_safety_fault(frame: &TelemetryFrame) -> Option<FrameAssessment> {
    let temp = sample_fault_rank(&frame.gpu_temp_c, "gpu_temp_c");
    let power = sample_fault_rank(&frame.power_w, "power_w");
    match (temp, power) {
        (None, None) => None,
        (Some((_, a)), None) | (None, Some((_, a))) => Some(a),
        (Some((rt, at)), Some((rp, ap))) => {
            if rt >= rp {
                Some(at)
            } else {
                Some(ap)
            }
        }
    }
}

// ── Actuation boundary (GH#46) ──────────────────────────────────────

/// Typed, observable failure of a privileged actuation attempt.
///
/// Represented explicitly (never a bare `String` at the actuator boundary) so
/// the supervisor can surface actuator failures — the `SafetyMachine` feeds
/// these into the [`SafetyState::ActuatorFailure`] observability path (GH#42).
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
    /// Current power limit in watts.
    pub current_w: u32,
    /// Device default power limit in watts.
    pub default_w: u32,
    /// Expected emergency-brake target in watts (`pct` of default).
    pub expected_w: u32,
}

/// The privileged hardware-safety actuation boundary.
///
/// Implementations apply/release a hardware power-limit brake and detect a
/// leftover brake at startup. The NVML/`nvidia-smi` backend is private to the
/// `thalamic-relay` executable; [`FakeActuator`] provides a deterministic test
/// double. This trait is the actuation half of GH#46: the [`SafetyMachine`]
/// decides *intent* ([`BrakeIntent`]) and never actuates, while implementations
/// here perform the privileged side effect and report typed [`ActuatorError`]s.
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
mod actuator_tests {
    use super::*;

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

    /// End-to-end: the pure `SafetyMachine` policy driving a `FakeActuator`,
    /// with no GPU and no subprocess.
    #[test]
    fn machine_drives_fake_actuator_critical_then_recovery() {
        use crate::telemetry::{assess, fixtures};

        fn frame(temp_c: f32, power_w: f32) -> TelemetryFrame {
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(temp_c);
            raw.power_w = Some(power_w);
            assess(&raw, fixtures::NOW)
        }

        let fake = FakeActuator::new();
        let mut machine = SafetyMachine::new();

        // Overheat → Apply intent → drive the fake actuator.
        let snap = machine.evaluate(&frame(95.0, 200.0));
        assert_eq!(snap.intent, BrakeIntent::Apply);
        let outcome = match fake.apply_emergency_brake(BRAKE_FRACTION) {
            Ok(()) => ActuatorOutcome::Applied,
            Err(e) => ActuatorOutcome::ApplyFailed(e.to_string()),
        };
        let snap = machine.record_actuator(outcome);
        assert!(snap.brake_engaged);
        assert!(fake.is_engaged());

        // Three real Ok readings → Release intent → drive the fake actuator.
        let cool = frame(60.0, 150.0);
        let _ = machine.evaluate(&cool);
        let _ = machine.evaluate(&cool);
        let snap = machine.evaluate(&cool);
        assert_eq!(snap.intent, BrakeIntent::Release);
        let outcome = match fake.release_emergency_brake() {
            Ok(()) => ActuatorOutcome::Released,
            Err(e) => ActuatorOutcome::ReleaseFailed(e.to_string()),
        };
        let snap = machine.record_actuator(outcome);
        assert!(!snap.brake_engaged);
        assert!(!fake.is_engaged());
        assert_eq!(fake.apply_calls(), 1);
        assert_eq!(fake.release_calls(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{SampleValidity, assess, fixtures};

    fn nvml_temp_power(temp_c: f32, power_w: f32) -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(temp_c);
        raw.power_w = Some(power_w);
        assess(&raw, fixtures::NOW)
    }

    fn eval_once(raw: crate::telemetry::RawTelemetry) -> SafetySnapshot {
        let mut machine = SafetyMachine::new();
        machine.evaluate(&assess(&raw, fixtures::NOW))
    }

    #[test]
    fn new_is_fail_closed_telemetry_missing() {
        let snap = SafetyMachine::new().snapshot();
        assert_eq!(snap.state, SafetyState::TelemetryMissing);
        assert_eq!(snap.policy_state, SafetyState::TelemetryMissing);
        assert!(snap.desired_brake);
        assert!(!snap.brake_engaged);
        assert_eq!(snap.intent, BrakeIntent::Apply);
        assert_eq!(snap.last_reason, "no telemetry evaluated yet");
    }

    #[test]
    fn healthy_real_is_named_state() {
        let snap = eval_once(fixtures::healthy_real());
        assert_eq!(snap.state, SafetyState::HealthyReal);
        assert_eq!(snap.policy_state, SafetyState::HealthyReal);
        assert!(!snap.brake_engaged);
        assert!(!snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::None);
    }

    #[test]
    fn warning_does_not_apply_brake_when_released() {
        let snap = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(78.0);
            raw
        });
        assert_eq!(snap.state, SafetyState::Warning);
        assert!(!snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::None);
    }

    #[test]
    fn warn_band_includes_exact_thresholds() {
        let at_temp = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(TEMP_WARN_C);
            raw
        });
        assert_eq!(at_temp.state, SafetyState::Warning);

        let at_power = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.power_w = Some(POWER_WARN_W);
            raw
        });
        assert_eq!(at_power.state, SafetyState::Warning);

        let just_below = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(74.9);
            raw.power_w = Some(299.9);
            raw
        });
        assert_eq!(just_below.state, SafetyState::HealthyReal);
    }

    #[test]
    fn critical_requests_brake() {
        let snap = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(90.0);
            raw
        });
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert!(snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn missing_stale_invalid_and_unavailable_fail_closed() {
        let missing = eval_once(fixtures::sensor_dropout());
        assert_eq!(snap_policy(&missing), SafetyState::TelemetryMissing);
        assert_eq!(missing.intent, BrakeIntent::Apply);

        let stale = eval_once(fixtures::stale());
        assert_eq!(snap_policy(&stale), SafetyState::TelemetryStale);
        assert_eq!(stale.intent, BrakeIntent::Apply);

        let invalid = eval_once(fixtures::out_of_range());
        assert_eq!(snap_policy(&invalid), SafetyState::TelemetryInvalid);
        assert_eq!(invalid.intent, BrakeIntent::Apply);

        let unavail = eval_once(fixtures::nvml_unavailable());
        assert_eq!(snap_policy(&unavail), SafetyState::TelemetryMissing);
        assert_eq!(unavail.intent, BrakeIntent::Apply);
        assert!(unavail.last_reason.contains("missing"));
    }

    fn snap_policy(snap: &SafetySnapshot) -> SafetyState {
        snap.policy_state
    }

    #[test]
    fn simulated_skips_thresholds_and_does_not_apply() {
        let mut raw = fixtures::software_fallback();
        raw.gpu_temp_c = Some(99.0);
        raw.power_w = Some(400.0);
        let snap = eval_once(raw);
        assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
        assert_eq!(snap.intent, BrakeIntent::None);
        let (status, is_sim) =
            instant_status(&assess(&fixtures::software_fallback(), fixtures::NOW));
        assert_eq!(status, SafetyStatus::Ok);
        assert!(is_sim);
    }

    #[test]
    fn simulated_holds_existing_brake_and_resets_hysteresis() {
        let mut machine = SafetyMachine::new();
        let critical = nvml_temp_power(90.0, 200.0);
        let snap = machine.evaluate(&critical);
        assert_eq!(snap.intent, BrakeIntent::Apply);
        let snap = machine.record_actuator(ActuatorOutcome::Applied);
        assert!(snap.brake_engaged);

        let sim = assess(&fixtures::software_fallback(), fixtures::NOW);
        let snap = machine.evaluate(&sim);
        assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
        assert!(snap.brake_engaged);
        assert!(snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::None);
        assert_eq!(snap.hysteresis_ok_count, 0);
    }

    #[test]
    fn hysteresis_requires_three_real_ok_before_release() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        let _ = machine.record_actuator(ActuatorOutcome::Applied);

        let ok = nvml_temp_power(65.0, 200.0);
        let first = machine.evaluate(&ok);
        assert_eq!(first.state, SafetyState::Recovering);
        assert_eq!(first.hysteresis_ok_count, 1);
        assert_eq!(first.intent, BrakeIntent::None);

        let second = machine.evaluate(&ok);
        assert_eq!(second.hysteresis_ok_count, 2);
        assert_eq!(second.intent, BrakeIntent::None);

        let third = machine.evaluate(&ok);
        assert_eq!(third.state, SafetyState::Recovering);
        assert_eq!(third.hysteresis_ok_count, 3);
        assert!(!third.desired_brake);
        assert_eq!(third.intent, BrakeIntent::Release);
    }

    #[test]
    fn warn_or_critical_resets_hysteresis_streak() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        let _ = machine.record_actuator(ActuatorOutcome::Applied);
        let ok = nvml_temp_power(65.0, 200.0);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        assert_eq!(machine.snapshot().hysteresis_ok_count, 2);

        let warn = nvml_temp_power(78.0, 200.0);
        let snap = machine.evaluate(&warn);
        assert_eq!(snap.state, SafetyState::Warning);
        assert_eq!(snap.hysteresis_ok_count, 0);
        assert!(snap.brake_engaged);
        assert_eq!(snap.intent, BrakeIntent::None);
    }

    #[test]
    fn warn_after_release_reapplies_brake() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        let _ = machine.record_actuator(ActuatorOutcome::Applied);
        let ok = nvml_temp_power(65.0, 200.0);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        let _ = machine.record_actuator(ActuatorOutcome::Released);
        assert!(!machine.snapshot().brake_engaged);

        let warn = nvml_temp_power(78.0, 200.0);
        let snap = machine.evaluate(&warn);
        assert_eq!(snap.state, SafetyState::Warning);
        assert!(snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn ok_after_release_is_healthy_real() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        let _ = machine.record_actuator(ActuatorOutcome::Applied);
        let ok = nvml_temp_power(65.0, 200.0);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        let _ = machine.record_actuator(ActuatorOutcome::Released);
        let snap = machine.evaluate(&ok);
        assert_eq!(snap.state, SafetyState::HealthyReal);
        assert_eq!(snap.intent, BrakeIntent::None);
    }

    #[test]
    fn actuator_failure_is_named_state_and_does_not_stop_evaluation() {
        let mut machine = SafetyMachine::new();
        let critical = nvml_temp_power(90.0, 200.0);
        let _ = machine.evaluate(&critical);
        let snap = machine.record_actuator(ActuatorOutcome::ApplyFailed(
            "nvidia-smi -pl failed".to_string(),
        ));
        assert_eq!(snap.state, SafetyState::ActuatorFailure);
        assert_eq!(snap.policy_state, SafetyState::CriticalBraked);
        assert!(snap.actuator_failed);
        assert_eq!(machine.actuator_failures_total(), 1);
        assert!(!snap.brake_engaged);
        assert_eq!(snap.intent, BrakeIntent::Apply);

        let still = machine.evaluate(&critical);
        assert_eq!(still.state, SafetyState::ActuatorFailure);
        assert_eq!(still.policy_state, SafetyState::CriticalBraked);
        assert_eq!(still.intent, BrakeIntent::Apply);

        let recovered = machine.record_actuator(ActuatorOutcome::Applied);
        assert!(!recovered.actuator_failed);
        let after = machine.evaluate(&critical);
        assert_eq!(after.state, SafetyState::CriticalBraked);
        assert!(after.brake_engaged);
        assert_eq!(after.intent, BrakeIntent::None);
    }

    #[test]
    fn healthy_evaluate_clears_stale_actuator_failure_overlay() {
        let mut machine = SafetyMachine::new();
        let critical = nvml_temp_power(90.0, 200.0);
        let _ = machine.evaluate(&critical);
        let _ = machine.record_actuator(ActuatorOutcome::ApplyFailed(
            "nvidia-smi -pl failed".to_string(),
        ));
        assert_eq!(machine.snapshot().state, SafetyState::ActuatorFailure);

        let healthy = machine.evaluate(&nvml_temp_power(65.0, 200.0));
        assert_eq!(healthy.policy_state, SafetyState::HealthyReal);
        assert!(!healthy.desired_brake);
        assert_eq!(healthy.state, SafetyState::HealthyReal);
        assert_eq!(healthy.last_actuator_error, None);
        assert_eq!(healthy.intent, BrakeIntent::None);
        assert_eq!(machine.actuator_failures_total(), 1);
    }

    #[test]
    fn release_failure_keeps_brake_and_retries() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        let _ = machine.record_actuator(ActuatorOutcome::Applied);
        let ok = nvml_temp_power(65.0, 200.0);
        let _ = machine.evaluate(&ok);
        let _ = machine.evaluate(&ok);
        let release = machine.evaluate(&ok);
        assert_eq!(release.intent, BrakeIntent::Release);

        let snap = machine.record_actuator(ActuatorOutcome::ReleaseFailed("sudo denied".into()));
        assert_eq!(snap.state, SafetyState::ActuatorFailure);
        assert!(snap.brake_engaged);
        assert_eq!(snap.intent, BrakeIntent::Release);
        assert_eq!(machine.actuator_failures_total(), 1);
    }

    #[test]
    fn leftover_brake_seed_enters_recovering() {
        let mut machine = SafetyMachine::new();
        machine.seed_brake_applied();
        let snap = machine.evaluate(&nvml_temp_power(65.0, 200.0));
        assert_eq!(snap.state, SafetyState::Recovering);
        assert!(snap.brake_engaged);
        assert_eq!(snap.hysteresis_ok_count, 1);
    }

    #[test]
    fn nvml_old_magic_is_healthy_real_not_simulated() {
        let snap = eval_once(fixtures::nvml_looks_like_old_magic());
        assert_eq!(snap.state, SafetyState::HealthyReal);
        let (status, is_sim) = instant_status(&assess(
            &fixtures::nvml_looks_like_old_magic(),
            fixtures::NOW,
        ));
        assert_eq!(status, SafetyStatus::Ok);
        assert!(!is_sim);
    }

    #[test]
    fn warn_from_frame_fail_closes_on_missing_temp_or_power() {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = None;
        let frame = assess(&raw, fixtures::NOW);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );

        let mut raw = fixtures::healthy_real();
        raw.power_w = None;
        let frame = assess(&raw, fixtures::NOW);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("power_w"))
        );

        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.power_w.value = None;
        frame.power_w.validity = SampleValidity::Missing;
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("power_w"))
        );
        let (status, is_sim) = instant_status(&frame);
        assert!(matches!(status, SafetyStatus::Critical(_)));
        assert!(!matches!(status, SafetyStatus::Warn(_)));
        assert!(!is_sim);

        let mut frame = nvml_temp_power(78.0, 200.0);
        frame.gpu_temp_c.value = None;
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert!(
            matches!(warn_from_frame(&frame), Some(SafetyStatus::Critical(ref msg)) if msg.contains("gpu_temp_c"))
        );
    }

    #[test]
    fn mixed_faults_prefer_missing_over_stale() {
        let mut frame = nvml_temp_power(65.0, 200.0);
        frame.gpu_temp_c.validity = SampleValidity::Stale;
        frame.power_w.value = None;
        frame.power_w.validity = SampleValidity::Missing;
        let assessment = classify_frame(&frame);
        assert_eq!(assessment.kind, AssessmentKind::Missing);
        assert!(assessment.reason.contains("power_w"));
    }

    #[test]
    fn transitions_are_counted() {
        let mut machine = SafetyMachine::new();
        let _ = machine.evaluate(&nvml_temp_power(65.0, 200.0));
        let before = machine.transitions_total();
        let snap = machine.evaluate(&nvml_temp_power(90.0, 200.0));
        assert_eq!(
            snap.transition,
            Some((SafetyState::HealthyReal, SafetyState::CriticalBraked))
        );
        assert_eq!(machine.transitions_total(), before + 1);
        let again = machine.evaluate(&nvml_temp_power(91.0, 200.0));
        assert_eq!(again.transition, None);
        assert_eq!(machine.transitions_total(), before + 1);
    }

    #[test]
    fn safety_state_ids_are_stable() {
        for (i, state) in SafetyState::ALL.iter().enumerate() {
            assert_eq!(state.as_id() as usize, i);
        }
        assert_eq!(SafetyState::ActuatorFailure.as_str(), "actuator_failure");
    }

    #[test]
    fn default_machine_matches_new() {
        let snap = SafetyMachine::default().snapshot();
        assert_eq!(snap.state, SafetyState::TelemetryMissing);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn exact_critical_thresholds_are_exclusive() {
        let at_temp = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(TEMP_CRITICAL_C);
            raw
        });
        assert_eq!(at_temp.state, SafetyState::Warning);

        let over_temp = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(TEMP_CRITICAL_C + 0.1);
            raw
        });
        assert_eq!(over_temp.state, SafetyState::CriticalBraked);
        assert_eq!(over_temp.intent, BrakeIntent::Apply);

        let at_power = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.power_w = Some(POWER_CRITICAL_W);
            raw
        });
        assert_eq!(at_power.state, SafetyState::Warning);

        let over_power = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.power_w = Some(POWER_CRITICAL_W + 0.1);
            raw
        });
        assert_eq!(over_power.state, SafetyState::CriticalBraked);
    }

    #[test]
    fn power_critical_wins_over_temp_warn() {
        let snap = eval_once({
            let mut raw = fixtures::healthy_real();
            raw.gpu_temp_c = Some(78.0);
            raw.power_w = Some(360.0);
            raw
        });
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert!(snap.last_reason.contains("power"));
    }

    #[test]
    fn non_finite_is_named_telemetry_invalid() {
        let snap = eval_once(fixtures::non_finite());
        assert_eq!(snap.state, SafetyState::TelemetryInvalid);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn mixed_faults_rank_missing_over_invalid_over_stale() {
        let mut invalid_over_stale = nvml_temp_power(65.0, 200.0);
        invalid_over_stale.gpu_temp_c.validity = SampleValidity::Stale;
        invalid_over_stale.power_w.validity = SampleValidity::Invalid;
        let assessment = classify_frame(&invalid_over_stale);
        assert_eq!(assessment.kind, AssessmentKind::Invalid);
        assert!(assessment.reason.contains("power_w"));

        let mut missing_over_invalid = nvml_temp_power(65.0, 200.0);
        missing_over_invalid.gpu_temp_c.validity = SampleValidity::Invalid;
        missing_over_invalid.power_w.value = None;
        missing_over_invalid.power_w.validity = SampleValidity::Missing;
        let assessment = classify_frame(&missing_over_invalid);
        assert_eq!(assessment.kind, AssessmentKind::Missing);
        assert!(assessment.reason.contains("power_w"));
    }

    #[test]
    fn equal_fault_rank_prefers_gpu_temp() {
        let mut both_stale = nvml_temp_power(65.0, 200.0);
        both_stale.gpu_temp_c.validity = SampleValidity::Stale;
        both_stale.power_w.validity = SampleValidity::Stale;
        let assessment = classify_frame(&both_stale);
        assert_eq!(assessment.kind, AssessmentKind::Stale);
        assert!(assessment.reason.contains("gpu_temp_c"));

        let mut both_invalid = nvml_temp_power(65.0, 200.0);
        both_invalid.gpu_temp_c.validity = SampleValidity::Invalid;
        both_invalid.power_w.validity = SampleValidity::Invalid;
        let assessment = classify_frame(&both_invalid);
        assert_eq!(assessment.kind, AssessmentKind::Invalid);
        assert!(assessment.reason.contains("gpu_temp_c"));

        let mut both_missing = nvml_temp_power(65.0, 200.0);
        both_missing.gpu_temp_c.value = None;
        both_missing.gpu_temp_c.validity = SampleValidity::Missing;
        both_missing.power_w.value = None;
        both_missing.power_w.validity = SampleValidity::Missing;
        let assessment = classify_frame(&both_missing);
        assert_eq!(assessment.kind, AssessmentKind::Missing);
        assert!(assessment.reason.contains("gpu_temp_c"));
    }

    #[test]
    fn warn_from_frame_is_none_for_ok_and_simulated() {
        assert_eq!(
            warn_from_frame(&assess(&fixtures::healthy_real(), fixtures::NOW)),
            None
        );
        assert_eq!(
            warn_from_frame(&assess(&fixtures::software_fallback(), fixtures::NOW)),
            None
        );
    }

    #[test]
    fn leftover_brake_plus_simulated_holds_and_does_not_apply() {
        let mut machine = SafetyMachine::new();
        machine.seed_brake_applied();
        let snap = machine.evaluate(&assess(&fixtures::software_fallback(), fixtures::NOW));
        assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
        assert!(snap.brake_engaged);
        assert!(snap.desired_brake);
        assert_eq!(snap.intent, BrakeIntent::None);
        assert_eq!(snap.hysteresis_ok_count, 0);
    }
}
