# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **Safety actuation boundary (GH#46):** Extracted privileged GPU actuation behind a `SafetyActuator` trait in `safety`, complementing the pure `SafetyMachine` policy (GH#42). The NVML/`nvidia-smi` backend is now `NvmlActuator` in `gpu` — a hardware adapter that implements the trait but defines no safety semantics. Actuator failures are typed and observable (`ActuatorError`) instead of stringly coupled to the supervisor, startup brake detection returns a typed `BrakeMatch`, and an in-memory `FakeActuator` enables deterministic apply/release tests with no GPU or subprocess. The supervisor drives actuation through `Arc<dyn SafetyActuator>`, preserving fail-closed behavior.
- **Breaking (internal):** Replaced numeric `GpuTelemetry` fields with a typed `TelemetrySample` contract (`Option` values, explicit source/validity/unit/freshness). Software-only mode is tagged `TelemetrySource::SoftwareFallback` and is no longer inferred from `temperature <= 0 && power <= 25`. Missing sensors stay `None` instead of silent `0.0`/`NaN`. See `docs/telemetry.md` (GH#41)
- Added `TelemetryFrame::to_sensory_mapping()` as the deterministic mapping surface toward corpus-ipc (transport remains GH#40)
- NVML/driver failure is `TelemetrySource::NvmlUnavailable` (fail closed); `SoftwareFallback` is reserved for `--force-software-only`. Mapping re-evaluates freshness at emit time and carries stale threshold plus configured `--step-interval-ms` cadence. Freshness gauge is computed at scrape time from `acquired_at`.
- **Safety failure domain (GH#42):** `SafetyMachine` is isolated from IPC/Brainstem. Named states (healthy-real, warning, critical/braked, recovering, telemetry missing/stale/invalid, simulated, actuator-failure), documented hysteresis, Prometheus safety/brake/transition/actuator-failure metrics, and GPU-less tests including a failing/slow publish sink. See `docs/safety.md`.
- **Breaking:** Removed in-process SNN execution (`neuromod::SpikingNetwork`, `NeuroModulators`) and the UDP control surface it existed to drive (`Stimuli` / `LearningReward` / `GetNeuroState`, and the `--udp-addr`/`--num-channels`/`--num-lif`/`--num-izh` flags). Thalamic is now a sensory + deterministic hardware-safety relay only; neural execution lives in `brainstem-daemon`. `docs/ipc.md` now documents the removal and points to the planned `corpus-ipc`-based replacement (RM-1143 / GH#39)
- Removed the `neuromod` and `serde_json` dependencies (no longer used)
- Bumped MSRV from `1.97.1` to `1.98.1`

## [0.1.0] - 2026-07-16

### Added

- Initial `thalamic-relay` binary: a Rust CLI that observes hardware telemetry and forwards normalized stimuli to an in-process spiking neural network
- Software-only SNN stepping via `neuromod` with graceful fallback when no GPU is present
- UDP IPC interface for streaming stimuli, applying reward signals, and querying neuromodulator state
- Prometheus-compatible metrics export on `localhost:9000/metrics`
- GPU telemetry collection via NVML (temperature, power, clocks, fan, utilization)
- Hardware safety monitoring with emergency brake, hysteresis recovery, and automatic throttle release
- CLI argument and environment variable parsing using `clap` (derive + env features)
- Single-instance process guard via a PID lockfile
- Initial test suite covering UDP stimuli, learning reward handling, safety thresholds, and metrics defaults
- CI pipeline with formatting, clippy, build, and test checks
- Dual MIT/Apache-2.0 licensing
- README, `AGENTS.md`, and repository `Boundaries` documentation
