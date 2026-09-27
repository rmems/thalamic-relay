//! Validated, device-independent safety configuration.

use super::{BRAKE_FRACTION, RELEASE_OK_STREAK, TEMP_CRITICAL_C, TEMP_WARN_C};

/// Operator-selected limits. Resolve once before evaluating real telemetry.
///
/// Power overrides must be supplied together. Otherwise warning/critical are
/// 85%/100% of the reported device default power limit. No device default and
/// no overrides leaves power policy unavailable (real telemetry fails closed).
/// Thermal defaults are operator policy choices, not inferred vendor limits.
#[derive(Debug, Clone, PartialEq)]
pub struct SafetyPolicyConfig {
    /// Thermal warning threshold in Celsius (inclusive).
    pub temp_warn_c: f32,
    /// Thermal critical threshold in Celsius (exceeded strictly).
    pub temp_critical_c: f32,
    /// Explicit power warning threshold in watts (inclusive).
    pub power_warn_w: Option<f32>,
    /// Explicit power critical threshold in watts (exceeded strictly).
    pub power_critical_w: Option<f32>,
    /// Consecutive healthy real evaluations required to authorize release.
    pub release_ok_streak: u32,
    /// Maximum sample age in milliseconds (age at this limit is stale).
    /// Can tighten, but cannot relax the telemetry contract's 2000 ms limit.
    pub max_sample_age_ms: u64,
    /// Maximum declared acquisition interval in milliseconds (inclusive).
    pub max_acquisition_interval_ms: u64,
}

impl Default for SafetyPolicyConfig {
    fn default() -> Self {
        Self {
            temp_warn_c: TEMP_WARN_C,
            temp_critical_c: TEMP_CRITICAL_C,
            power_warn_w: None,
            power_critical_w: None,
            release_ok_streak: RELEASE_OK_STREAK,
            max_sample_age_ms: crate::telemetry::SAFETY_STALE_AFTER_MS,
            max_acquisition_interval_ms: crate::telemetry::DEFAULT_ACQUISITION_CADENCE_MS,
        }
    }
}

/// Effective immutable policy. Construct with [`SafetyPolicyConfig::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub struct SafetyPolicy {
    pub(super) config: SafetyPolicyConfig,
    pub(super) power_limits_w: Option<(f32, f32)>,
    power_limit_source: &'static str,
}

/// Invalid operator configuration or invalid reported device capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub &'static str);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for PolicyError {}

impl SafetyPolicyConfig {
    /// Validate settings and resolve power limits against an optional NVML
    /// default limit, in watts. Explicit critical power cannot exceed a known
    /// default. Unknown capabilities require explicit paired power limits for
    /// healthy real classification; simulated frames still work without them.
    pub fn resolve(&self, device_default_w: Option<f32>) -> Result<SafetyPolicy, PolicyError> {
        let ordered_positive = |a: f32, b: f32| a.is_finite() && b.is_finite() && a > 0.0 && a < b;
        if !ordered_positive(self.temp_warn_c, self.temp_critical_c) || self.temp_critical_c > 125.0
        {
            return Err(PolicyError(
                "temperature limits must be finite: 0 < warning < critical <= 125 C",
            ));
        }
        if self.release_ok_streak == 0 {
            return Err(PolicyError("release Ok streak must be positive"));
        }
        if self.max_sample_age_ms == 0
            || self.max_sample_age_ms > crate::telemetry::SAFETY_STALE_AFTER_MS
        {
            return Err(PolicyError("maximum sample age must be in 1..=2000 ms"));
        }
        if self.max_acquisition_interval_ms == 0
            || self.max_acquisition_interval_ms > self.max_sample_age_ms
        {
            return Err(PolicyError(
                "maximum acquisition interval must be positive and no greater than maximum sample age",
            ));
        }
        if device_default_w.is_some_and(|w| !w.is_finite() || w <= 0.0 || w > 2_000.0) {
            return Err(PolicyError(
                "device default power limit must be finite and in (0, 2000] W",
            ));
        }
        let (power_limits_w, power_limit_source) = match (self.power_warn_w, self.power_critical_w)
        {
            (Some(warn), Some(critical)) => {
                if !ordered_positive(warn, critical) || critical > 2_000.0 {
                    return Err(PolicyError(
                        "power limits must be finite: 0 < warning < critical <= 2000 W",
                    ));
                }
                if device_default_w.is_some_and(|cap| critical > cap) {
                    return Err(PolicyError(
                        "explicit critical power exceeds the device default power limit",
                    ));
                }
                (Some((warn, critical)), "operator")
            }
            (None, None) => match device_default_w {
                Some(w) if ordered_positive(w * 0.85, w) => (Some((w * 0.85, w)), "device_default"),
                Some(_) => {
                    return Err(PolicyError(
                        "device default is too small to derive power limits",
                    ));
                }
                None => (None, "unavailable"),
            },
            _ => {
                return Err(PolicyError(
                    "power warning and critical overrides must be supplied together",
                ));
            }
        };
        Ok(SafetyPolicy {
            config: self.clone(),
            power_limits_w,
            power_limit_source,
        })
    }
}

impl SafetyPolicy {
    /// Validated operator settings, including thermal and recovery limits.
    #[must_use]
    pub fn config(&self) -> &SafetyPolicyConfig {
        &self.config
    }
    /// Effective warning/critical watts, or `None` for unavailable power policy.
    #[must_use]
    pub fn power_limits_w(&self) -> Option<(f32, f32)> {
        self.power_limits_w
    }
    /// Stable provenance: `operator`, `device_default`, or `unavailable`.
    #[must_use]
    pub fn power_limit_source(&self) -> &'static str {
        self.power_limit_source
    }
    /// Fixed brake strategy for this release, shared with restart detection.
    #[must_use]
    pub fn brake_fraction(&self) -> f32 {
        BRAKE_FRACTION
    }
}
