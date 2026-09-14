# Telemetry contract

This is the normative validity / freshness / normalization / provenance
contract for `thalamic-relay` ([GH#41](https://github.com/rmems/thalamic-relay/issues/41)).

Invalid, missing, stale, and simulated data is **never** inferred from magic
numeric values. Missing sensors stay `None`; they are never silently converted
into a legitimate `0.0`.

Acquisition (`src/gpu.rs`) is separate from validation / normalization / encoding
(`src/telemetry.rs`). Safety policy consumes [`TelemetryFrame`] samples and
does not depend on corpus-ipc transport ([GH#40](https://github.com/rmems/thalamic-relay/issues/40)).

## Typed sample

```text
TelemetrySample<T> {
    value: Option<T>,       // engineering units; None if missing or non-finite
    observed_at: unix ms,
    source: TelemetrySource, // Nvml | SoftwareFallback
    validity: SampleValidity, // Valid | Missing | Invalid | Stale
    unit: Unit,
}
```

Simulated data is [`TelemetrySource::SoftwareFallback`]. It is not inferred
from conventions such as `temperature <= 0 && power <= 25`. An NVML sample
that happens to read `0 °C` and `25 W` is still `source = Nvml` and may be a
legitimate idle GPU.

## Pipeline

```text
RawTelemetry          (optional engineering values + acquisition source)
        │
        ▼  assess() / TelemetryFrame::from_raw()
TelemetryFrame        (per-signal TelemetrySample)
        │
        ├─ HardwareBridge::check_safety()     safety-only / Both
        ├─ to_sensory_mapping()               runtime-input / Both  → GH#40
        └─ to_observability_snapshot()      every signal, raw preserved
```

The supervisor currently assesses each acquisition immediately (age ≈ 0).
If NVML is unavailable it switches to `SoftwareFallback` rather than holding a
last-good NVML sample until it goes stale. The `Stale` variant exists for
held frames, tests, and downstream `#40` consumers.

## Signal inventory

| Signal | Unit | Range | Origin | Class | Normalization | Cadence | Stale after |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `gpu_temp_c` | °C | 0…125 | measured | **both** (safety + runtime-input) | linear 0…100 → `[0, 1]` | 100 ms | 2 s |
| `power_w` | W | 0…500 | measured | **both** | linear 0…350 → `[0, 1]` | 100 ms | 2 s |
| `gpu_clock_mhz` | MHz | 0…3000 | measured | runtime-input | linear 0…2500 → `[0, 1]` | 100 ms | 5 s |
| `mem_util_pct` | % | 0…100 | measured | runtime-input | linear 0…100 → `[0, 1]` | 100 ms | 5 s |
| `vram_temp_c` | °C | 0…125 | measured | observability-only | linear 0…100 → `[0, 1]` | 100 ms | 5 s |
| `mem_clock_mhz` | MHz | 0…12000 | measured | observability-only | linear 0…10000 → `[0, 1]` | 100 ms | 5 s |
| `fan_speed_pct` | % | 0…100 | measured | observability-only | linear 0…100 → `[0, 1]` | 100 ms | 5 s |
| `vddcr_gfx_v` | V | 0.4…1.5 | **derived** from power | observability-only | linear 0.5…1.2 → `[0, 1]` | 100 ms | 5 s |

There is currently no safety-only-only channel: temperature and power are
classified **both**. Thermal (85 °C critical / 75 °C warn) and power (350 W
critical / 300 W warn) thresholds are safety *policy* (#47 will make them
configurable) and are **not** the sensor engineering range.

`vddcr_gfx_v` is not an NVML voltage sensor. It is derived from board power
for observability and is omitted from the sensory mapping (no filler rails).

`vram_temp_c` is not fabricated as `gpu_temp + 8`. `nvml-wrapper` 0.10 only
exposes `TemperatureSensor::Gpu`, so VRAM temperature is `Missing` until a
future adapter can read it.

## Safety vs sensory vs observability

- **Safety-critical** (`gpu_temp_c`, `power_w`): missing / invalid / stale fail
  closed (`SafetyStatus::Critical`). Software-fallback frames skip hardware
  thresholds because provenance says there is no real GPU to protect.
- **Runtime-input candidates**: `gpu_temp_c`, `power_w`, `gpu_clock_mhz`,
  `mem_util_pct`. These are the only channels in `SensoryMapping`.
- **Observability-only**: `vram_temp_c`, `vddcr_gfx_v`, `mem_clock_mhz`,
  `fan_speed_pct`. Present in `ObservabilitySnapshot` with raw values even
  when invalid; never used as silent model-input zeros.

A legitimate zero (for example `mem_util_pct = 0.0` while `Valid`) is
distinct from missing (`value = None`, `validity = Missing`).

## Software fallback estimates

When NVML is unavailable or `--force-software-only` is set, acquisition
emits the documented idle estimates in `telemetry::software_fallback` with
`TelemetrySource::SoftwareFallback`. VRAM temperature stays `None`. These
numbers are estimates, not a second set of magic flags.

## corpus-ipc mapping surface (#41 owns types, #40 owns transport)

`TelemetryFrame::to_sensory_mapping()` produces `SensoryMapping` /
`MappedStimulus` with timestamp, source, validity, raw engineering value, and
normalized `[0, 1]` (only when `Valid`). `#40` should map this into published
`corpus-ipc` types rather than copying a second wire schema. This crate does
not take a `corpus-ipc` dependency here.

## Fixtures

`telemetry::fixtures` provides deterministic bags for tests:

- `healthy_real` — NVML-like, including legitimate `mem_util_pct = 0.0`
- `software_fallback` — explicit simulated idle
- `stale` — healthy values with `observed_at` 10 s in the past
- `sensor_dropout` — `power_w = None` (not `0.0` / `NaN`)
- `non_finite` — `power_w = NaN`
- `out_of_range` — `gpu_temp_c = 200`
- `nvml_looks_like_old_magic` — NVML `0 °C` / `25 W`, **not** simulated
