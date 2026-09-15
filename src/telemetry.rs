//! Explicit telemetry validity, freshness, normalization, and provenance contract.
//!
//! Invalid, missing, stale, and simulated data is represented by
//! [`SampleValidity`] and [`TelemetrySource`], never inferred from magic numeric
//! values. Missing or invalid readings stay [`None`] and are never silently
//! converted into a legitimate numeric zero.
//!
//! [`SensoryMapping`] / [`MappedStimulus`] are a typed mapping hook toward a
//! future publisher ([#40](https://github.com/rmems/thalamic-relay/issues/40)).
//! This module does **not** implement `corpus-ipc` transport and does not feed
//! a local neuromorphic inference core.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix-epoch timestamp in milliseconds (UTC). Serializable and suitable for
/// the sensory mapping surface.
pub type UnixMillis = u64;

/// Provenance of a telemetry acquisition path.
///
/// Simulated / software-fallback data is tagged here explicitly. It is never
/// inferred from numeric conventions such as `temperature <= 0 && power <= 25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TelemetrySource {
    /// NVIDIA Management Library (NVML) on a live device.
    Nvml,
    /// Documented software-only idle estimates (`--force-software-only`).
    SoftwareFallback,
    /// NVML/driver/device lookup failed. Not confirmed software-only.
    /// Safety must fail closed (missing safety samples), not skip as simulated.
    NvmlUnavailable,
}

/// Validity of a single sample. Orthogonal to [`TelemetrySource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SampleValidity {
    /// Finite, in-range, and fresh enough to use.
    Valid,
    /// Sensor was not read (dropout, unsupported, or not applicable).
    Missing,
    /// Non-finite or outside the documented engineering range.
    Invalid,
    /// Value may be real but is older than the signal's stale threshold.
    Stale,
}

/// Engineering unit of a sample's `value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Unit {
    /// Degrees Celsius.
    Celsius,
    /// Watts.
    Watt,
    /// Volts.
    Volt,
    /// Megahertz.
    Megahertz,
    /// Percent (0–100 engineering units).
    Percent,
}

/// How a signal may be consumed. See the repository file `docs/telemetry.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignalClass {
    /// Candidate sensory input for a downstream runtime (via a future publisher).
    ///
    /// Thalamic itself does not consume this as neural input.
    RuntimeInput,
    /// Consumed only by hardware-safety policy.
    SafetyOnly,
    /// Dashboards / Prometheus / logs; not a sensory or safety input.
    ObservabilityOnly,
    /// Both a safety signal and a runtime-input candidate.
    Both,
}

impl SignalClass {
    /// Whether this signal may appear in the sensory mapping toward a future publisher.
    #[must_use]
    pub const fn includes_runtime_input(self) -> bool {
        matches!(self, Self::RuntimeInput | Self::Both)
    }

    /// Whether safety policy may consume this signal.
    #[must_use]
    pub const fn includes_safety(self) -> bool {
        matches!(self, Self::SafetyOnly | Self::Both)
    }
}

/// Whether the engineering value is measured or computed from another signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignalOrigin {
    /// Read from a device sensor (NVML) or a documented software-only estimate.
    Measured,
    /// Computed from another signal (currently `vddcr_gfx_v` from power).
    Derived,
}

/// Deterministic `[0, 1]` normalization for the sensory mapping.
/// Applied only to [`SampleValidity::Valid`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Normalization {
    /// Pass-through of the engineering value (not used for current GPU signals).
    Identity,
    /// `(value - min) / (max - min)` clamped to `[0, 1]`.
    Linear {
        /// Lower bound of the engineering range used for scaling.
        min: f32,
        /// Upper bound of the engineering range used for scaling.
        max: f32,
    },
}

impl Normalization {
    /// Normalize a finite engineering value. The caller must have already
    /// established [`SampleValidity::Valid`]; this never maps missing data.
    #[must_use]
    pub fn apply(self, value: f32) -> f32 {
        match self {
            Self::Identity => value,
            Self::Linear { min, max } => {
                let span = max - min;
                if !span.is_finite() || span.abs() < f32::EPSILON {
                    0.0
                } else {
                    ((value - min) / span).clamp(0.0, 1.0)
                }
            }
        }
    }
}

/// Stable signal identifiers. Names are the sensory-mapping keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignalId {
    /// GPU die temperature (`gpu_temp_c`).
    GpuTempC,
    /// VRAM temperature when the adapter can read it (`vram_temp_c`).
    VramTempC,
    /// Board power (`power_w`).
    PowerW,
    /// Derived GFX-rail voltage *estimate* (`vddcr_gfx_v`), not an NVML sensor.
    ///
    /// The identifier is historical (AMD `VDDCR_GFX` naming). It is
    /// observability-only and is omitted from [`SensoryMapping`].
    VddcrGfxV,
    /// Graphics clock (`gpu_clock_mhz`).
    GpuClockMhz,
    /// Memory clock (`mem_clock_mhz`).
    MemClockMhz,
    /// Fan speed (`fan_speed_pct`).
    FanSpeedPct,
    /// Memory utilization (`mem_util_pct`).
    MemUtilPct,
}

impl SignalId {
    /// Canonical snake_case name for docs and the sensory mapping keys.
    #[must_use]
    pub const fn name(self) -> &'static str {
        signal_spec(self).name
    }
}

/// Inventory of every GPU reading currently acquired by this crate.
pub const ALL_SIGNALS: [SignalId; 8] = [
    SignalId::GpuTempC,
    SignalId::VramTempC,
    SignalId::PowerW,
    SignalId::VddcrGfxV,
    SignalId::GpuClockMhz,
    SignalId::MemClockMhz,
    SignalId::FanSpeedPct,
    SignalId::MemUtilPct,
];

/// Default supervisor acquisition cadence (matches `--step-interval-ms` default).
pub const DEFAULT_ACQUISITION_CADENCE_MS: u64 = 100;
/// Safety evaluation cadence in the supervisor (every 10 ticks at default interval).
pub const SAFETY_EVAL_CADENCE_MS: u64 = 1_000;
/// Stale threshold for safety-critical signals (2× safety cadence).
pub const SAFETY_STALE_AFTER_MS: u64 = 2_000;
/// Stale threshold for non-safety signals.
pub const OBSERVABILITY_STALE_AFTER_MS: u64 = 5_000;

/// Per-signal contract: unit, range, origin, class, normalization, cadence, stale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalSpec {
    /// Stable identifier for this channel.
    pub id: SignalId,
    /// Canonical snake_case name (mapping key).
    pub name: &'static str,
    /// Engineering unit of `value`.
    pub unit: Unit,
    /// Inclusive minimum of the documented engineering range.
    pub min: f32,
    /// Inclusive maximum of the documented engineering range.
    pub max: f32,
    /// Measured versus derived.
    pub origin: SignalOrigin,
    /// Whether safety, sensory mapping, and/or observability may consume this.
    pub class: SignalClass,
    /// `[0, 1]` scaling applied only to [`SampleValidity::Valid`] samples.
    pub normalization: Normalization,
    /// Documented default acquisition interval (ms).
    pub cadence_ms: u64,
    /// Age (ms) at or beyond which a sample is [`SampleValidity::Stale`].
    pub stale_after_ms: u64,
}

/// Look up the static contract for `id`.
#[must_use]
pub const fn signal_spec(id: SignalId) -> SignalSpec {
    match id {
        SignalId::GpuTempC => SignalSpec {
            id,
            name: "gpu_temp_c",
            unit: Unit::Celsius,
            min: 0.0,
            max: 125.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::Both,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 100.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: SAFETY_STALE_AFTER_MS,
        },
        SignalId::VramTempC => SignalSpec {
            id,
            name: "vram_temp_c",
            unit: Unit::Celsius,
            min: 0.0,
            max: 125.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::ObservabilityOnly,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 100.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
        SignalId::PowerW => SignalSpec {
            id,
            name: "power_w",
            unit: Unit::Watt,
            min: 0.0,
            max: 500.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::Both,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 350.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: SAFETY_STALE_AFTER_MS,
        },
        SignalId::VddcrGfxV => SignalSpec {
            id,
            name: "vddcr_gfx_v",
            unit: Unit::Volt,
            min: 0.4,
            max: 1.5,
            origin: SignalOrigin::Derived,
            class: SignalClass::ObservabilityOnly,
            normalization: Normalization::Linear { min: 0.5, max: 1.2 },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
        SignalId::GpuClockMhz => SignalSpec {
            id,
            name: "gpu_clock_mhz",
            unit: Unit::Megahertz,
            min: 0.0,
            max: 3_000.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::RuntimeInput,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 2_500.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
        SignalId::MemClockMhz => SignalSpec {
            id,
            name: "mem_clock_mhz",
            unit: Unit::Megahertz,
            min: 0.0,
            max: 12_000.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::ObservabilityOnly,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 10_000.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
        SignalId::FanSpeedPct => SignalSpec {
            id,
            name: "fan_speed_pct",
            unit: Unit::Percent,
            min: 0.0,
            max: 100.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::ObservabilityOnly,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 100.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
        SignalId::MemUtilPct => SignalSpec {
            id,
            name: "mem_util_pct",
            unit: Unit::Percent,
            min: 0.0,
            max: 100.0,
            origin: SignalOrigin::Measured,
            class: SignalClass::RuntimeInput,
            normalization: Normalization::Linear {
                min: 0.0,
                max: 100.0,
            },
            cadence_ms: DEFAULT_ACQUISITION_CADENCE_MS,
            stale_after_ms: OBSERVABILITY_STALE_AFTER_MS,
        },
    }
}

/// Documented software-only idle estimates. These are **not** real sensors.
/// Provenance must be [`TelemetrySource::SoftwareFallback`].
pub mod software_fallback {
    /// Typical idle die temperature estimate (°C). Not a magic "no GPU" flag.
    pub const GPU_TEMP_C: f32 = 35.0;
    /// Typical idle board power estimate (W).
    pub const POWER_W: f32 = 25.0;
    /// Derived GFX-rail voltage estimate (V). Not a measured sensor.
    pub const VDDCR_GFX_V: f32 = 0.7;
    /// Typical idle graphics clock estimate (MHz).
    pub const GPU_CLOCK_MHZ: f32 = 210.0;
    /// Typical idle memory clock estimate (MHz).
    pub const MEM_CLOCK_MHZ: f32 = 405.0;
    /// Typical idle fan speed estimate (%).
    pub const FAN_SPEED_PCT: f32 = 30.0;
    /// Legitimate idle utilization of 0%, distinguishable from [`None`] missing.
    pub const MEM_UTIL_PCT: f32 = 0.0;
}

/// Wall-clock now as unix milliseconds. Returns 0 if the clock is before epoch.
#[must_use]
pub fn unix_now_ms() -> UnixMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One typed sample. `value` is engineering units when present.
///
/// `value` is `None` for missing and non-finite readings. Out-of-range and
/// stale readings keep the raw number for observability but are not [`SampleValidity::Valid`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySample<T> {
    /// Which inventory channel this sample belongs to.
    pub signal: SignalId,
    /// Engineering-unit value. `None` if missing or non-finite.
    pub value: Option<T>,
    /// Acquisition timestamp (unix ms).
    pub observed_at: UnixMillis,
    /// How this reading was acquired.
    pub source: TelemetrySource,
    /// Valid / missing / invalid / stale.
    pub validity: SampleValidity,
    /// Engineering unit of `value`.
    pub unit: Unit,
}

impl TelemetrySample<f32> {
    /// Validate a raw optional reading against the signal contract.
    ///
    /// Non-finite values become `value: None` + [`SampleValidity::Invalid`].
    /// Out-of-range values keep the raw number with [`SampleValidity::Invalid`].
    /// Missing stays `None` — never a silent `0.0`.
    #[must_use]
    pub fn from_raw(
        signal: SignalId,
        raw: Option<f32>,
        observed_at: UnixMillis,
        source: TelemetrySource,
        now: UnixMillis,
    ) -> Self {
        let spec = signal_spec(signal);
        let (value, validity) = if observed_at > now {
            match raw {
                None => (None, SampleValidity::Missing),
                Some(v) if !v.is_finite() => (None, SampleValidity::Invalid),
                Some(v) => (Some(v), SampleValidity::Invalid),
            }
        } else {
            let age_ms = now.saturating_sub(observed_at);
            match raw {
                None => (None, SampleValidity::Missing),
                Some(v) if !v.is_finite() => (None, SampleValidity::Invalid),
                Some(v) if v < spec.min || v > spec.max => (Some(v), SampleValidity::Invalid),
                Some(v) if age_ms >= spec.stale_after_ms => (Some(v), SampleValidity::Stale),
                Some(v) => (Some(v), SampleValidity::Valid),
            }
        };
        Self {
            signal,
            value,
            observed_at,
            source,
            validity,
            unit: spec.unit,
        }
    }

    /// Normalized sensory-mapping value in `[0, 1]`. `None` unless this sample
    /// is [`SampleValidity::Valid`]. Thalamic does not feed this into a local
    /// inference core.
    #[must_use]
    pub fn normalized(&self) -> Option<f32> {
        if self.validity != SampleValidity::Valid {
            return None;
        }
        self.value
            .map(|v| signal_spec(self.signal).normalization.apply(v))
    }

    /// Re-evaluate future/stale validity at `now` without re-acquiring.
    ///
    /// Missing stays missing. Samples with no engineering value keep their
    /// validity (except a future timestamp on a non-missing empty sample is
    /// [`SampleValidity::Invalid`]). Finite values are re-run through
    /// [`Self::from_raw`].
    #[must_use]
    pub fn at_time(&self, now: UnixMillis) -> Self {
        if self.value.is_none() {
            return self.clone();
        }
        Self::from_raw(self.signal, self.value, self.observed_at, self.source, now)
    }
}

/// Raw acquired readings **before** validation / normalization.
///
/// Every field is `Option`: absence means the sensor was not read. Acquisition
/// must not write `0.0` or `NaN` as a stand-in for "no data".
#[derive(Debug, Clone, PartialEq)]
pub struct RawTelemetry {
    /// Acquisition timestamp (unix ms).
    pub observed_at: UnixMillis,
    /// Provenance of this acquisition path.
    pub source: TelemetrySource,
    /// GPU die temperature (°C), if read.
    pub gpu_temp_c: Option<f32>,
    /// VRAM temperature (°C), if read.
    pub vram_temp_c: Option<f32>,
    /// Board power (W), if read.
    pub power_w: Option<f32>,
    /// Derived GFX-rail voltage estimate (V), if power was present.
    pub vddcr_gfx_v: Option<f32>,
    /// Graphics clock (MHz), if read.
    pub gpu_clock_mhz: Option<f32>,
    /// Memory clock (MHz), if read.
    pub mem_clock_mhz: Option<f32>,
    /// Fan speed (%), if read.
    pub fan_speed_pct: Option<f32>,
    /// Memory utilization (%), if read. `Some(0.0)` is a legitimate idle.
    pub mem_util_pct: Option<f32>,
}

impl RawTelemetry {
    /// Documented software-only idle estimates with explicit fallback provenance.
    #[must_use]
    pub fn software_fallback(observed_at: UnixMillis) -> Self {
        Self {
            observed_at,
            source: TelemetrySource::SoftwareFallback,
            gpu_temp_c: Some(software_fallback::GPU_TEMP_C),
            vram_temp_c: None,
            power_w: Some(software_fallback::POWER_W),
            vddcr_gfx_v: Some(software_fallback::VDDCR_GFX_V),
            gpu_clock_mhz: Some(software_fallback::GPU_CLOCK_MHZ),
            mem_clock_mhz: Some(software_fallback::MEM_CLOCK_MHZ),
            fan_speed_pct: Some(software_fallback::FAN_SPEED_PCT),
            mem_util_pct: Some(software_fallback::MEM_UTIL_PCT),
        }
    }

    /// NVML/driver unavailable: every channel missing, not software-idle estimates.
    #[must_use]
    pub fn nvml_unavailable(observed_at: UnixMillis) -> Self {
        Self {
            observed_at,
            source: TelemetrySource::NvmlUnavailable,
            gpu_temp_c: None,
            vram_temp_c: None,
            power_w: None,
            vddcr_gfx_v: None,
            gpu_clock_mhz: None,
            mem_clock_mhz: None,
            fan_speed_pct: None,
            mem_util_pct: None,
        }
    }

    fn value(&self, id: SignalId) -> Option<f32> {
        match id {
            SignalId::GpuTempC => self.gpu_temp_c,
            SignalId::VramTempC => self.vram_temp_c,
            SignalId::PowerW => self.power_w,
            SignalId::VddcrGfxV => self.vddcr_gfx_v,
            SignalId::GpuClockMhz => self.gpu_clock_mhz,
            SignalId::MemClockMhz => self.mem_clock_mhz,
            SignalId::FanSpeedPct => self.fan_speed_pct,
            SignalId::MemUtilPct => self.mem_util_pct,
        }
    }
}

/// Validated frame: per-signal [`TelemetrySample`] plus frame-level provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryFrame {
    /// Acquisition timestamp copied from the raw bag.
    pub acquired_at: UnixMillis,
    /// Frame-level provenance.
    pub source: TelemetrySource,
    /// Actual supervisor acquisition interval (`--step-interval-ms`), not the
    /// documented default in [`signal_spec`].
    pub acquisition_cadence_ms: u64,
    /// GPU die temperature sample.
    pub gpu_temp_c: TelemetrySample<f32>,
    /// VRAM temperature sample (often missing on current NVML adapters).
    pub vram_temp_c: TelemetrySample<f32>,
    /// Board power sample.
    pub power_w: TelemetrySample<f32>,
    /// Derived GFX-rail voltage estimate (observability-only).
    pub vddcr_gfx_v: TelemetrySample<f32>,
    /// Graphics clock sample.
    pub gpu_clock_mhz: TelemetrySample<f32>,
    /// Memory clock sample.
    pub mem_clock_mhz: TelemetrySample<f32>,
    /// Fan speed sample.
    pub fan_speed_pct: TelemetrySample<f32>,
    /// Memory utilization sample.
    pub mem_util_pct: TelemetrySample<f32>,
}

impl TelemetryFrame {
    /// Validate and stamp every raw channel. Does not mutate the raw bag.
    /// Uses [`DEFAULT_ACQUISITION_CADENCE_MS`] as the actual cadence.
    #[must_use]
    pub fn from_raw(raw: &RawTelemetry, now: UnixMillis) -> Self {
        Self::from_raw_with_cadence(raw, now, DEFAULT_ACQUISITION_CADENCE_MS)
    }

    /// Like [`Self::from_raw`] but records the configured acquisition interval.
    #[must_use]
    pub fn from_raw_with_cadence(raw: &RawTelemetry, now: UnixMillis, cadence_ms: u64) -> Self {
        let sample = |id: SignalId| {
            TelemetrySample::from_raw(id, raw.value(id), raw.observed_at, raw.source, now)
        };
        Self {
            acquired_at: raw.observed_at,
            source: raw.source,
            acquisition_cadence_ms: cadence_ms.max(1),
            gpu_temp_c: sample(SignalId::GpuTempC),
            vram_temp_c: sample(SignalId::VramTempC),
            power_w: sample(SignalId::PowerW),
            vddcr_gfx_v: sample(SignalId::VddcrGfxV),
            gpu_clock_mhz: sample(SignalId::GpuClockMhz),
            mem_clock_mhz: sample(SignalId::MemClockMhz),
            fan_speed_pct: sample(SignalId::FanSpeedPct),
            mem_util_pct: sample(SignalId::MemUtilPct),
        }
    }

    /// All samples in inventory order (includes observability-only).
    #[must_use]
    pub fn samples(&self) -> [&TelemetrySample<f32>; 8] {
        [
            &self.gpu_temp_c,
            &self.vram_temp_c,
            &self.power_w,
            &self.vddcr_gfx_v,
            &self.gpu_clock_mhz,
            &self.mem_clock_mhz,
            &self.fan_speed_pct,
            &self.mem_util_pct,
        ]
    }

    /// Deterministic mapping toward a future publisher using wall-clock now.
    ///
    /// This is **not** transport and is **not** a local inference input.
    #[must_use]
    pub fn to_sensory_mapping(&self) -> SensoryMapping {
        self.to_sensory_mapping_at(unix_now_ms())
    }

    /// Mapping at an explicit instant so held frames can go stale.
    ///
    /// Includes only runtime-input candidates ([`SignalClass::RuntimeInput`] or
    /// [`SignalClass::Both`]). Observability-only channels are omitted (no
    /// fabricated extra signals). Normalized values are `None` unless the sample
    /// is [`SampleValidity::Valid`] *at `now`*. Each mapped channel carries
    /// `stale_after_ms` and the actual `cadence_ms`.
    #[must_use]
    pub fn to_sensory_mapping_at(&self, now: UnixMillis) -> SensoryMapping {
        let stimuli = self
            .samples()
            .into_iter()
            .filter(|s| signal_spec(s.signal).class.includes_runtime_input())
            .map(|s| MappedStimulus::from_sample(&s.at_time(now), self.acquisition_cadence_ms))
            .collect();
        SensoryMapping {
            observed_at_unix_ms: self.acquired_at,
            acquisition_source: self.source,
            acquisition_cadence_ms: self.acquisition_cadence_ms,
            stimuli,
        }
    }

    /// Full raw snapshot for observability, independent of sensory normalization.
    #[must_use]
    pub fn to_observability_snapshot(&self) -> ObservabilitySnapshot {
        self.to_observability_snapshot_at(unix_now_ms())
    }

    /// Observability snapshot re-evaluated at `now`.
    #[must_use]
    pub fn to_observability_snapshot_at(&self, now: UnixMillis) -> ObservabilitySnapshot {
        ObservabilitySnapshot {
            observed_at_unix_ms: self.acquired_at,
            acquisition_source: self.source,
            acquisition_cadence_ms: self.acquisition_cadence_ms,
            samples: self
                .samples()
                .into_iter()
                .map(|s| MappedStimulus::from_sample(&s.at_time(now), self.acquisition_cadence_ms))
                .collect(),
        }
    }
}

/// Validate raw acquisition into a [`TelemetryFrame`].
#[must_use]
pub fn assess(raw: &RawTelemetry, now: UnixMillis) -> TelemetryFrame {
    TelemetryFrame::from_raw(raw, now)
}

/// Validate raw acquisition and record the configured acquisition cadence.
#[must_use]
pub fn assess_with_cadence(raw: &RawTelemetry, now: UnixMillis, cadence_ms: u64) -> TelemetryFrame {
    TelemetryFrame::from_raw_with_cadence(raw, now, cadence_ms)
}

/// One channel in the sensory mapping surface (not a wire type).
///
/// The type name is historical (the retired UDP `Stimuli` protocol). This is
/// **not** a neural stimulus: Thalamic does not run an SNN. Transport of this
/// bag remains unimplemented ([#40](https://github.com/rmems/thalamic-relay/issues/40)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappedStimulus {
    /// Inventory identifier.
    pub signal: SignalId,
    /// Canonical snake_case name.
    pub name: String,
    /// Safety / sensory / observability class.
    pub classification: SignalClass,
    /// Engineering unit of `raw`.
    pub unit: Unit,
    /// Acquisition provenance.
    pub source: TelemetrySource,
    /// Validity re-evaluated at mapping time.
    pub validity: SampleValidity,
    /// Engineering-unit raw value. `None` if missing or non-finite.
    pub raw: Option<f32>,
    /// `[0, 1]` normalized value for a downstream consumer. `None` unless
    /// [`SampleValidity::Valid`]. Not consumed by a local inference core.
    pub normalized: Option<f32>,
    /// Per-signal stale threshold (ms). Consumers need not copy the inventory.
    pub stale_after_ms: u64,
    /// Actual acquisition cadence for this frame (`--step-interval-ms`).
    pub cadence_ms: u64,
}

impl MappedStimulus {
    fn from_sample(sample: &TelemetrySample<f32>, cadence_ms: u64) -> Self {
        let spec = signal_spec(sample.signal);
        Self {
            signal: sample.signal,
            name: spec.name.to_string(),
            classification: spec.class,
            unit: sample.unit,
            source: sample.source,
            validity: sample.validity,
            raw: sample.value,
            normalized: sample.normalized(),
            stale_after_ms: spec.stale_after_ms,
            cadence_ms,
        }
    }
}

/// Typed mapping hook toward a future publisher. Not a `corpus-ipc` schema
/// duplicate and not a local inference input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensoryMapping {
    /// Frame acquisition timestamp (unix ms).
    pub observed_at_unix_ms: UnixMillis,
    /// Provenance of the underlying acquisition.
    pub acquisition_source: TelemetrySource,
    /// Actual acquisition interval for this frame (ms).
    pub acquisition_cadence_ms: u64,
    /// Runtime-input / Both channels only (observability-only omitted).
    pub stimuli: Vec<MappedStimulus>,
}

/// Raw-preserving observability view of an entire frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservabilitySnapshot {
    /// Frame acquisition timestamp (unix ms).
    pub observed_at_unix_ms: UnixMillis,
    /// Provenance of the underlying acquisition.
    pub acquisition_source: TelemetrySource,
    /// Actual acquisition interval for this frame (ms).
    pub acquisition_cadence_ms: u64,
    /// Every inventory channel, including observability-only.
    pub samples: Vec<MappedStimulus>,
}

/// Deterministic fixtures for the contract (healthy, fallback, stale, dropout, …).
pub mod fixtures {
    use super::{RawTelemetry, TelemetrySource, UnixMillis};

    /// Fixed timestamp so tests do not depend on wall clock.
    pub const NOW: UnixMillis = 1_700_000_000_000;

    /// Healthy NVML-like readings, including a legitimate `mem_util_pct = 0.0`.
    #[must_use]
    pub fn healthy_real() -> RawTelemetry {
        RawTelemetry {
            observed_at: NOW,
            source: TelemetrySource::Nvml,
            gpu_temp_c: Some(65.0),
            vram_temp_c: Some(72.0),
            power_w: Some(200.0),
            vddcr_gfx_v: Some(0.91),
            gpu_clock_mhz: Some(1_500.0),
            mem_clock_mhz: Some(6_000.0),
            fan_speed_pct: Some(40.0),
            mem_util_pct: Some(0.0),
        }
    }

    /// Software-only idle estimates (explicit fallback provenance).
    #[must_use]
    pub fn software_fallback() -> RawTelemetry {
        RawTelemetry::software_fallback(NOW)
    }

    /// NVML unavailable: all channels missing, fail-closed safety path.
    #[must_use]
    pub fn nvml_unavailable() -> RawTelemetry {
        RawTelemetry::nvml_unavailable(NOW)
    }

    /// Same engineering values as [`healthy_real`] but observed 10s ago.
    #[must_use]
    pub fn stale() -> RawTelemetry {
        RawTelemetry {
            observed_at: NOW.saturating_sub(10_000),
            ..healthy_real()
        }
    }

    /// NVML path with power sensor dropout (`None`, not `0.0` or `NaN`).
    #[must_use]
    pub fn sensor_dropout() -> RawTelemetry {
        RawTelemetry {
            power_w: None,
            vddcr_gfx_v: None,
            fan_speed_pct: None,
            ..healthy_real()
        }
    }

    /// Non-finite power; temperature still present.
    #[must_use]
    pub fn non_finite() -> RawTelemetry {
        RawTelemetry {
            power_w: Some(f32::NAN),
            ..healthy_real()
        }
    }

    /// GPU temperature far outside the documented engineering range.
    #[must_use]
    pub fn out_of_range() -> RawTelemetry {
        RawTelemetry {
            gpu_temp_c: Some(200.0),
            ..healthy_real()
        }
    }

    /// NVML readings that match the *old* magic-value heuristic (`temp <= 0 && power <= 25`).
    /// These must **not** be classified as simulated.
    #[must_use]
    pub fn nvml_looks_like_old_magic() -> RawTelemetry {
        RawTelemetry {
            gpu_temp_c: Some(0.0),
            power_w: Some(25.0),
            vddcr_gfx_v: Some(0.7),
            gpu_clock_mhz: Some(210.0),
            mem_clock_mhz: Some(405.0),
            fan_speed_pct: Some(30.0),
            mem_util_pct: Some(0.0),
            vram_temp_c: None,
            observed_at: NOW,
            source: TelemetrySource::Nvml,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::NOW;

    fn assess_now(raw: &RawTelemetry) -> TelemetryFrame {
        assess(raw, NOW)
    }

    #[test]
    fn healthy_real_is_valid_nvml_with_legitimate_zero_util() {
        let frame = assess_now(&fixtures::healthy_real());
        assert_eq!(frame.source, TelemetrySource::Nvml);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert_eq!(frame.gpu_temp_c.value, Some(65.0));
        assert_eq!(frame.power_w.validity, SampleValidity::Valid);
        assert_eq!(frame.mem_util_pct.validity, SampleValidity::Valid);
        assert_eq!(frame.mem_util_pct.value, Some(0.0));
        assert_eq!(frame.mem_util_pct.normalized(), Some(0.0));
        assert_ne!(frame.mem_util_pct.validity, SampleValidity::Missing);
    }

    #[test]
    fn software_fallback_is_explicit_source_not_magic_values() {
        let frame = assess_now(&fixtures::software_fallback());
        assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
        assert_eq!(frame.gpu_temp_c.value, Some(software_fallback::GPU_TEMP_C));
        assert_eq!(frame.power_w.value, Some(software_fallback::POWER_W));
        assert_eq!(frame.vram_temp_c.value, None);
        assert_eq!(frame.vram_temp_c.validity, SampleValidity::Missing);
        assert_eq!(frame.gpu_temp_c.source, TelemetrySource::SoftwareFallback);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        // Fallback idle is 35°C / 25W — the old heuristic would have treated
        // only temp<=0 && power<=25 as simulated. Provenance is the contract.
        assert!(frame.gpu_temp_c.value.unwrap() > 0.0);
    }

    #[test]
    fn nvml_matching_old_magic_is_not_simulated() {
        let frame = assess_now(&fixtures::nvml_looks_like_old_magic());
        assert_eq!(frame.source, TelemetrySource::Nvml);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert_eq!(frame.gpu_temp_c.value, Some(0.0));
        assert_eq!(frame.power_w.value, Some(25.0));
        assert_eq!(frame.gpu_temp_c.normalized(), Some(0.0));
    }

    #[test]
    fn stale_samples_keep_raw_but_are_not_valid() {
        let frame = assess(&fixtures::stale(), NOW);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Stale);
        assert_eq!(frame.gpu_temp_c.value, Some(65.0));
        assert_eq!(frame.gpu_temp_c.normalized(), None);
        assert_eq!(frame.power_w.validity, SampleValidity::Stale);
        assert_eq!(frame.mem_util_pct.validity, SampleValidity::Stale);
    }

    #[test]
    fn sensor_dropout_is_missing_not_zero() {
        let frame = assess_now(&fixtures::sensor_dropout());
        assert_eq!(frame.source, TelemetrySource::Nvml);
        assert_eq!(frame.power_w.value, None);
        assert_eq!(frame.power_w.validity, SampleValidity::Missing);
        assert_eq!(frame.power_w.normalized(), None);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
        assert_eq!(frame.vddcr_gfx_v.validity, SampleValidity::Missing);
        assert_eq!(frame.fan_speed_pct.validity, SampleValidity::Missing);
    }

    #[test]
    fn non_finite_is_invalid_with_none_value() {
        let frame = assess_now(&fixtures::non_finite());
        assert_eq!(frame.power_w.validity, SampleValidity::Invalid);
        assert_eq!(frame.power_w.value, None);
        assert_eq!(frame.power_w.normalized(), None);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);
    }

    #[test]
    fn out_of_range_keeps_raw_and_is_invalid() {
        let frame = assess_now(&fixtures::out_of_range());
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Invalid);
        assert_eq!(frame.gpu_temp_c.value, Some(200.0));
        assert_eq!(frame.gpu_temp_c.normalized(), None);
    }

    #[test]
    fn normalization_is_deterministic() {
        let frame = assess_now(&fixtures::healthy_real());
        assert_eq!(frame.gpu_temp_c.normalized(), Some(0.65));
        let expected_power = 200.0 / 350.0;
        assert!((frame.power_w.normalized().unwrap() - expected_power).abs() < 1e-6);
        assert_eq!(frame.mem_util_pct.normalized(), Some(0.0));
    }

    #[test]
    fn sensory_mapping_omits_observability_only_without_filler() {
        let frame = assess_now(&fixtures::healthy_real());
        let mapping = frame.to_sensory_mapping_at(NOW);
        assert_eq!(mapping.acquisition_source, TelemetrySource::Nvml);
        let names: Vec<&str> = mapping.stimuli.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["gpu_temp_c", "power_w", "gpu_clock_mhz", "mem_util_pct"]
        );
        assert!(!names.contains(&"vddcr_gfx_v"));
        assert!(!names.contains(&"fan_speed_pct"));
        let util = mapping
            .stimuli
            .iter()
            .find(|s| s.signal == SignalId::MemUtilPct)
            .unwrap();
        assert_eq!(util.raw, Some(0.0));
        assert_eq!(util.normalized, Some(0.0));
        assert_eq!(util.validity, SampleValidity::Valid);
    }

    #[test]
    fn observability_snapshot_preserves_raw_independently_of_normalization() {
        let frame = assess_now(&fixtures::out_of_range());
        let snap = frame.to_observability_snapshot_at(NOW);
        assert_eq!(snap.samples.len(), ALL_SIGNALS.len());
        let temp = snap
            .samples
            .iter()
            .find(|s| s.signal == SignalId::GpuTempC)
            .unwrap();
        assert_eq!(temp.raw, Some(200.0));
        assert_eq!(temp.normalized, None);
        assert_eq!(temp.validity, SampleValidity::Invalid);
    }

    #[test]
    fn missing_runtime_input_is_not_normalized_to_zero() {
        let frame = assess_now(&fixtures::sensor_dropout());
        let mapping = frame.to_sensory_mapping_at(NOW);
        let power = mapping
            .stimuli
            .iter()
            .find(|s| s.signal == SignalId::PowerW)
            .unwrap();
        assert_eq!(power.raw, None);
        assert_eq!(power.normalized, None);
        assert_eq!(power.validity, SampleValidity::Missing);
    }

    #[test]
    fn inventory_classifies_every_gpu_signal() {
        for id in ALL_SIGNALS {
            let spec = signal_spec(id);
            assert_eq!(spec.id, id);
            assert!(!spec.name.is_empty());
            assert!(spec.max > spec.min);
            assert!(spec.cadence_ms > 0);
            assert!(spec.stale_after_ms > 0);
        }
        assert_eq!(signal_spec(SignalId::GpuTempC).class, SignalClass::Both);
        assert_eq!(signal_spec(SignalId::PowerW).class, SignalClass::Both);
        assert_eq!(
            signal_spec(SignalId::GpuClockMhz).class,
            SignalClass::RuntimeInput
        );
        assert_eq!(
            signal_spec(SignalId::MemUtilPct).class,
            SignalClass::RuntimeInput
        );
        assert_eq!(
            signal_spec(SignalId::VddcrGfxV).class,
            SignalClass::ObservabilityOnly
        );
        assert_eq!(
            signal_spec(SignalId::VddcrGfxV).origin,
            SignalOrigin::Derived
        );
        assert_eq!(
            signal_spec(SignalId::VramTempC).class,
            SignalClass::ObservabilityOnly
        );
        assert_eq!(
            signal_spec(SignalId::MemClockMhz).class,
            SignalClass::ObservabilityOnly
        );
        assert_eq!(
            signal_spec(SignalId::FanSpeedPct).class,
            SignalClass::ObservabilityOnly
        );
    }

    #[test]
    fn future_observed_at_is_invalid_not_valid() {
        let mut raw = fixtures::healthy_real();
        raw.observed_at = NOW + 5_000;
        let frame = assess(&raw, NOW);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Invalid);
        assert_eq!(frame.gpu_temp_c.value, Some(65.0));
        assert_eq!(frame.power_w.validity, SampleValidity::Invalid);
        assert_eq!(frame.gpu_temp_c.normalized(), None);
    }

    #[test]
    fn mapping_re_evaluates_stale_and_carries_thresholds() {
        let frame = assess_now(&fixtures::healthy_real());
        let fresh = frame.to_sensory_mapping_at(NOW);
        let temp = fresh
            .stimuli
            .iter()
            .find(|s| s.signal == SignalId::GpuTempC)
            .unwrap();
        assert_eq!(temp.validity, SampleValidity::Valid);
        assert_eq!(temp.normalized, Some(0.65));
        assert_eq!(temp.stale_after_ms, SAFETY_STALE_AFTER_MS);
        assert_eq!(temp.cadence_ms, DEFAULT_ACQUISITION_CADENCE_MS);
        assert_eq!(fresh.acquisition_cadence_ms, DEFAULT_ACQUISITION_CADENCE_MS);

        let later = NOW + SAFETY_STALE_AFTER_MS;
        let mapping = frame.to_sensory_mapping_at(later);
        let temp = mapping
            .stimuli
            .iter()
            .find(|s| s.signal == SignalId::GpuTempC)
            .unwrap();
        assert_eq!(temp.validity, SampleValidity::Stale);
        assert_eq!(temp.raw, Some(65.0));
        assert_eq!(temp.normalized, None);
    }

    #[test]
    fn mapping_reports_configured_acquisition_cadence() {
        let frame = assess_with_cadence(&fixtures::healthy_real(), NOW, 50);
        assert_eq!(frame.acquisition_cadence_ms, 50);
        assert_eq!(
            signal_spec(SignalId::GpuTempC).cadence_ms,
            DEFAULT_ACQUISITION_CADENCE_MS
        );
        let mapping = frame.to_sensory_mapping_at(NOW);
        assert_eq!(mapping.acquisition_cadence_ms, 50);
        assert!(mapping.stimuli.iter().all(|s| s.cadence_ms == 50));
    }

    #[test]
    fn nvml_unavailable_is_missing_not_software_fallback() {
        let frame = assess_now(&fixtures::nvml_unavailable());
        assert_eq!(frame.source, TelemetrySource::NvmlUnavailable);
        assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Missing);
        assert_eq!(frame.power_w.validity, SampleValidity::Missing);
        assert_eq!(frame.gpu_temp_c.value, None);
    }
}
