# Thalamic Relay

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/rmems/thalamic-relay#license)

A lightweight CLI relay that observes hardware telemetry and provides
deterministic hardware safety for the Spikenaut runtime stack (software-only;
silicon-bridge/**FPGA (Field-Programmable Gate Array)** bridge dep removed
for modularity).

## Overview

Thalamic Relay is the hardware-facing **sensory + deterministic safety**
process for the Spikenaut runtime stack:

```text
hardware telemetry
      ↓
thalamic-relay
  - sensing
  - validation
  - normalization
  - staleness/missingness
  - hard safety
      ↓  IpcMessage::Stimuli (best-effort; not on the safety path)
corpus-ipc
      ↓
brainstem-daemon
  - SpikingNetwork
  - neuromodulation
  - tick loop
```

It collects GPU telemetry, runs deterministic thermal/power safety
checks, and exposes observability over Prometheus metrics. It does **not**
run any neural computation itself — that lives in `brainstem-daemon`. The
relay is platform-agnostic: it degrades gracefully to a software-only mode
when no GPU is present, and **hardware safety is an isolated failure domain**:
it keeps evaluating with `brainstem-daemon` absent or crashed, and IPC
send/disconnect/slow-consumer cannot stall the safety loop. Brainstem has
no authority to override Thalamic hard-safety.

## Features

- **GPU Telemetry**: Real-time monitoring of GPU sensors via NVML (temperature,
  power, clocks, fan, utilization) with an explicit software-fallback
  provenance tag — missing sensors stay absent (`None`), never a silent `0.0`
- **Deterministic Hardware Safety**: Isolated state machine (healthy-real,
  warning, critical/braked, recovering/hysteresis, telemetry
  missing/stale/invalid, simulated/software-only, actuator-failure) with
  emergency brake and hysteresis-gated release. Independent of Brainstem and
  of any IPC publisher.
- **Metrics Collection**: Prometheus-compatible metrics export (freshness,
  safety state, brake state, transition and actuator-failure counters)
- **Process Safety**: Single-instance protection via a lockfile mechanism

## Installation

### Prerequisites

- Rust 2024 edition (MSRV 1.98.1)
- `pkg-config` (used by some native dependencies)
- Linux operating system (tested on Linux)
- Optional: an NVIDIA GPU with NVML support

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run --bin thalamic-relay
```

## Usage

The relay runs in software-only mode and continuously monitors GPU telemetry
and hardware safety when available.

While running it exposes (address configurable via CLI/env; see Configuration):

- **Prometheus metrics** on `http://localhost:9000/metrics` (bind IP configurable via --metrics-ip)
- **corpus-ipc sensory publish** on UDP `127.0.0.1:9900` by default (`--ipc-endpoint`): fire-and-forget `IpcMessage::Stimuli` JSON. This is not a control/query socket and is not the retired neural UDP protocol; see [`docs/ipc.md`](docs/ipc.md).

Hardware safety keeps evaluating if Brainstem is absent or the queue is full.

## Architecture

Thalamic is a sensory + **independent hard-safety** process. Brainstem is
the neural runtime. They do not share a fate:

```text
hardware telemetry
      ↓
thalamic-relay
  - sensing / validation / freshness
  - SafetyMachine (never waits on IPC)
  - privileged brake actuator
  - Prometheus safety/brake state
      ↓  IsolatedPublishQueue.try_enqueue (drop on full / absent)
corpus-ipc          IpcMessage::Stimuli JSON over UDP (not required for safety)
      ↓
brainstem-daemon
  - SpikingNetwork
  - neuromodulation
  - tick loop
```

### What Thalamic guarantees vs Brainstem

| Owner | Guarantees |
| --- | --- |
| **Thalamic** | Hardware telemetry contract; fail-closed protection on missing/stale/invalid telemetry; brake apply/release; observable safety/brake state **without** querying neural state; continues with Brainstem absent |
| **Brainstem** | SNN execution, neural state, reward/plasticity. Consumes sensory mappings if transport exists. **Cannot** inhibit or override the Thalamic brake |
| **corpus-ipc** | Transport only. Send failure is not a safety pause |

See [`docs/safety.md`](docs/safety.md) for named states and hysteresis
rules, and [`docs/telemetry.md`](docs/telemetry.md) for the sample contract.

### Core Modules

- **`telemetry`**: Typed sample contract (validity, freshness, provenance, normalization) and the internal sensory mapping surface
- **`safety`**: Pure deterministic classification + hysteresis (`SafetyMachine`); no NVML, no IPC
- **`gpu`**: Raw NVML acquisition and privileged power-limit actuation
- **`publish`**: Maps `SensoryMapping` → `corpus-ipc` `StimulusBatch` / `IpcMessage::Stimuli` and UDP-publishes off the safety path (`CorpusIpcPublisher`, `AbsentPublisher`, `IsolatedPublishQueue`)
- **`cpu`**: Telemetry initialization and metrics collection

### Key Components

1. **Hardware Bridge**: GPU acquisition and privileged emergency-brake actuator
2. **Safety machine**: Named relay states, hysteresis, actuator-failure overlay
3. **Telemetry System**: Real-time metrics collection and export
4. **Publish sink**: Best-effort, never on the `evaluate` path

## Dependencies

### Core Dependencies

- `tokio`: Async runtime with full features
- `serde` / `serde_json`: Serialization of the typed telemetry contract and `IpcMessage`
- `corpus-ipc` 0.1.0: canonical `StimulusBatch` / `IpcMessage` wire schema (default features; no ZeroMQ)
- `tracing` / `tracing-subscriber`: Structured logging and telemetry
- `metrics` / `metrics-exporter-prometheus`: Metrics collection with Prometheus export

### Hardware Interfaces

- `nvml-wrapper`: GPU monitoring via NVIDIA Management Library

## Configuration

The relay supports CLI flags **and** environment variables (clap derive + "env" feature; added for #11). Defaults preserve prior hardcoded behavior.

Run `thalamic-relay --help` (or `-V`) for the full documented surface.

Key options (with env var equivalent):

- `--metrics-ip` / `THALAMIC_METRICS_IP` (default: 127.0.0.1; port is always 9000)
- `--step-interval-ms` / `THALAMIC_STEP_INTERVAL_MS` (default: 100) — relay loop tick interval
- `--force-software-only` / `THALAMIC_FORCE_SOFTWARE_ONLY`
- `--ipc-endpoint` / `THALAMIC_IPC_ENDPOINT` (default: `127.0.0.1:9900`) — UDP destination for `IpcMessage::Stimuli`
- `--ipc-disabled` / `THALAMIC_IPC_DISABLED` — skip publication; safety still runs
- `--ipc-session-id` / `THALAMIC_IPC_SESSION_ID` (default: `thalamic-relay`)
- `RUST_LOG` (standard for tracing; or --log-level in future extensions)

Example with env + flag:
```bash
THALAMIC_METRICS_IP=0.0.0.0 \
  cargo run --bin thalamic-relay -- --force-software-only --step-interval-ms 50
```

## Monitoring

### Prometheus Metrics

The relay exports metrics compatible with Prometheus monitoring. Safety
state is observable here; there is no neural-state query:

- `telemetry_freshness_s` — sample age at scrape time
- `safety_state{state=...}` / `safety_state_id` — current named safety state
- `safety_policy_state{state=...}` — policy classification before the ActuatorFailure overlay (`safety_state` is the overlay)
- `safety_brake_engaged` — last successful brake still claimed
- `safety_hysteresis_ok_count` — Ok streak while braked
- `safety_transitions_total` / `safety_actuator_failures_total` — counters

See [`docs/safety.md`](docs/safety.md) for the label set and numeric ids.

### Logging

Structured logging via `tracing` with configurable output levels.

## Safety Features

- **Instance Protection**: Lockfile mechanism prevents multiple relay instances (lock acquired before port binding)
- **Independent safety loop**: `SafetyMachine::evaluate` has no publisher argument and is not awaited on IPC. Production uses `CorpusIpcPublisher` (`try_send` + detached UDP worker). `--ipc-disabled` or a bind failure falls back to `AbsentPublisher`.
- **GPU Safety Monitoring**: Safety cadence every ~1 second (every 10 ticks); named states for healthy-real, warning, critical/braked, recovering, missing/stale/invalid, simulated, actuator-failure
- **Emergency Brakes**: Automatically throttles GPU power limit to 50% via `nvidia-smi -pl` on fail-closed or critical; 3 consecutive real Ok readings to release; warn immediately after release re-applies
- **Graceful Degradation**: Continues in software-only mode when
  `--force-software-only` is set (`TelemetrySource::SoftwareFallback`).
  NVML/driver failure without that flag is `NvmlUnavailable` and fail-closes
  safety. Simulation is never inferred from magic numbers such as
  `temperature <= 0 && power <= 25`

## Telemetry contract

Every GPU reading is a typed `TelemetrySample` with `value: Option<T>`,
`observed_at`, `source`, `validity`, and `unit`. See
[`docs/telemetry.md`](docs/telemetry.md) for the full inventory.

| Signal | Class | Notes |
| --- | --- | --- |
| `gpu_temp_c`, `power_w` | safety + runtime-input | Missing/invalid/stale fail closed |
| `gpu_clock_mhz`, `mem_util_pct` | runtime-input | Sensory mapping → `StimulusBatch` |
| `vram_temp_c`, `mem_clock_mhz`, `fan_speed_pct` | observability-only | Raw preserved; not model input |
| `vddcr_gfx_v` | observability-only (derived) | Estimated from power; not an NVML voltage sensor |

A legitimate zero (for example 0% memory utilization) is distinct from a
missing sensor. Simulated idle estimates are tagged
`TelemetrySource::SoftwareFallback` (forced software-only only).
NVML/driver failure is `NvmlUnavailable`. Normalization to `[0, 1]` is
deterministic and is not applied to missing, invalid, or stale samples.

## License

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE-2.0](LICENSE-APACHE-2.0) or [http://www.apache.org/licenses/LICENSE-2.0])
- MIT license ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT])

at your option.

## Contributing

Contributions are welcome! Please ensure all submissions follow the project's
coding standards and include appropriate tests.

## Releasing

This crate is not yet published to crates.io (that is planned for v1.0; see #29).
To cut a tag and GitHub Release for a `0.1.x` patch:

1. Make sure `CHANGELOG.md` is up to date and the version in `Cargo.toml` matches the intended release.
2. Run the validation suite locally:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   cargo build --release
   ```
3. Create an annotated tag from a clean `main` branch and push it:
   ```bash
   git checkout main
   git pull origin main
   git tag -a v0.1.0 -m "Release v0.1.0"
   git push origin v0.1.0
   ```
4. On GitHub, create a Release from the new tag and paste the relevant `CHANGELOG.md` section into the release notes.
5. Optionally attach the `cargo build --release` binary.

For a hotfix, bump the patch version in `Cargo.toml` (e.g. `0.1.1`) and use the same tag pattern (`v0.1.1`).

## Troubleshooting

### Common Issues

1. **Telemetry**: GPU access requires NVML; runs without it in software mode.
2. **GPU Access**: Verify NVML installation and proper permissions
3. **Instance Conflicts**: Check for an existing relay process holding the lockfile
   at `/tmp/thalamic_relay.lock`

### Debug Mode

Enable debug logging for detailed troubleshooting:

```bash
RUST_LOG=debug cargo run --bin thalamic-relay
```
