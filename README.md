# Thalamic Relay

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/rmems/thalamic-relay#license)

A Rust **library** and **`thalamic-relay` daemon** that observe GPU telemetry
and provide deterministic hardware safety for the Spikenaut runtime stack.

Thalamic does **not** run neural computation, own neural state, or perform
neuromorphic inference. That lives in `brainstem-daemon` (a separate process).
There is currently **no** `corpus-ipc` transport and **no** control/query IPC
surface — only Prometheus metrics on `:9000/metrics`.

## Library versus daemon

| Surface | Use it when | Starts a process? |
| --- | --- | --- |
| Crate `thalamic_relay` | Validate telemetry, evaluate [`SafetyMachine`](https://docs.rs/thalamic-relay), map sensory frames | No |
| Binary `thalamic-relay` | Acquire NVML (or simulated idle), drive the safety loop, export Prometheus | Yes |

Library consumers do not need a GPU. The daemon attempts NVML unless
`--force-software-only` is set.

```rust
use thalamic_relay::safety::{SafetyMachine, SafetyState};
use thalamic_relay::telemetry::{assess, fixtures, TelemetrySource};

let frame = assess(&fixtures::software_fallback(), fixtures::NOW);
assert_eq!(frame.source, TelemetrySource::SoftwareFallback);

let mut machine = SafetyMachine::new();
let snap = machine.evaluate(&frame);
assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
```

Full rustdoc (architecture, privileges, limitations) is the docs.rs landing
page for this crate.

## Overview

```text
telemetry source (NVML | SoftwareFallback | NvmlUnavailable)
      ↓  validate / normalize / freshness / provenance
TelemetryFrame
      ├─ SafetyMachine::evaluate     deterministic policy (no I/O)
      │         ↓
      │   BrakeIntent → SafetyActuator   best-effort privileged side effect
      └─ SensoryPublisher::try_publish   best-effort; currently AbsentPublisher
                ↓
         corpus-ipc transport            not implemented (GH#40)
                ↓
         brainstem-daemon                SNN / neural state (separate process)
```

Hardware safety is an isolated failure domain: it keeps evaluating with
`brainstem-daemon` absent or crashed, and a missing/slow publisher cannot
stall the safety loop. Brainstem has no authority to override Thalamic
hard-safety.

## Software-only versus real hardware

| Provenance | How you get it | What the numbers are | Safety |
| --- | --- | --- | --- |
| `TelemetrySource::Nvml` | Live NVIDIA device via NVML | Real sensors | Thresholds apply; missing/stale/invalid **fail closed** |
| `TelemetrySource::SoftwareFallback` | `--force-software-only` **only** | Documented idle **estimates** | `simulated_software_only`; does **not** apply a new brake |
| `TelemetrySource::NvmlUnavailable` | NVML/driver/device lookup failed without that flag | Every channel **missing** | **Fail closed** (not simulated) |

Simulated idle is never inferred from magic numbers such as
`temperature <= 0 && power <= 25`. `--force-software-only` is the only
simulated path. Running the daemon without a GPU and without that flag is
**not** software-only: it is `NvmlUnavailable` and fail-closes.

## GPU power-limit actuation (privileges)

Policy (`SafetyMachine`) is deterministic and unprivileged. Applying or
releasing a brake is a best-effort Linux side effect:

`timeout -k 2 5s sudo -n nvidia-smi -pl <watts>`

That requires:

- Linux
- a working NVIDIA driver and NVML (`libnvidia-ml.so`)
- **passwordless sudo** for `nvidia-smi` (`sudo -n`; a password prompt is a
  failure, not a hang — the command is non-interactive)

Without those, actuation returns `ActuatorError` and the machine records
`actuator_failure` while continuing to evaluate. A fail-closed *intent* does
not guarantee the board power limit changed. This is not a substitute for GPU
firmware thermal protection.

## Features

- **GPU Telemetry**: NVML sensors (temperature, power, clocks, fan,
  utilization) with explicit provenance — missing sensors stay absent
  (`None`), never a silent `0.0`
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
- Optional, for actual power-limit actuation: passwordless `sudo` for
  `nvidia-smi`

### Build

```bash
cargo build --release
```

### Run the daemon

```bash
cargo run --bin thalamic-relay
```

Force documented idle estimates (no NVML attempt):

```bash
cargo run --bin thalamic-relay -- --force-software-only
```

## Usage

The daemon acquires telemetry on `--step-interval-ms` (default 100 ms) and
evaluates safety on a ~1 s cadence. While running it exposes (address
configurable via CLI/env; see Configuration):

- **Prometheus metrics** on `http://localhost:9000/metrics` (bind IP
  configurable via `--metrics-ip`)

It currently has no control/query IPC surface — the prior UDP protocol was
removed along with the in-process SNN it existed to drive; see
[`docs/ipc.md`](docs/ipc.md) for the retired UDP surface, the GH#41 mapping
types, and the planned `corpus-ipc` transport (GH#40, **not implemented**).

Run `thalamic-relay --help` for the CLI surface.

## Architecture

Thalamic is a sensory + **independent hard-safety** process. Brainstem is
the neural runtime. They do not share a fate. See the pipeline diagram
above.

### What Thalamic guarantees vs Brainstem

| Owner | Guarantees |
| --- | --- |
| **Thalamic** | Hardware telemetry contract; fail-closed **intent** on missing/stale/invalid telemetry; brake apply/release **attempts**; observable safety/brake state **without** querying neural state; continues with Brainstem absent |
| **Brainstem** | SNN execution, neural state, reward/plasticity. Would consume sensory mappings **if** transport existed. **Cannot** inhibit or override the Thalamic brake |
| **corpus-ipc** | Transport only (not implemented). Send failure is not a safety pause |

See [`docs/safety.md`](docs/safety.md) for named states and hysteresis
rules, and [`docs/telemetry.md`](docs/telemetry.md) for the sample contract.

### Deterministic versus best-effort

**Deterministic** (pure; GPU-less testable): `assess` / `TelemetryFrame`,
freshness/validity/normalization, `to_sensory_mapping_at`,
`SafetyMachine::evaluate`.

**Best-effort** (can fail or time out): NVML acquisition, `nvidia-smi`
liveness, privileged power-limit apply/release, leftover-brake detection,
Prometheus, sensory publication (currently always absent).

### Core modules (library)

- **`telemetry`**: Typed sample contract (validity, freshness, provenance,
  normalization) and the sensory mapping hook (`SensoryMapping`). No transport.
- **`safety`**: Pure deterministic classification + hysteresis
  (`SafetyMachine`); `SafetyActuator` trait. No NVML, no IPC.
- **`gpu`**: Raw NVML acquisition and privileged power-limit actuation
  (`NvmlActuator`)
- **`publish`**: Non-blocking sensory publish stub (`AbsentPublisher`,
  `IsolatedPublishQueue`); transport is GH#40
- **`cpu`**: Daemon Prometheus initialization and metrics collection

## Dependencies

### Core Dependencies

- `tokio`: Async runtime with full features
- `serde`: Serialization framework (used by the typed telemetry contract)
- `tracing` / `tracing-subscriber`: Structured logging and telemetry
- `metrics` / `metrics-exporter-prometheus`: Metrics collection with Prometheus export

### Hardware Interfaces

- `nvml-wrapper`: GPU monitoring via NVIDIA Management Library

## Configuration

The daemon supports CLI flags **and** environment variables (clap derive +
"env" feature). Defaults preserve prior hardcoded behavior.

Run `thalamic-relay --help` (or `-V`) for the full documented surface.

Key options (with env var equivalent):

- `--metrics-ip` / `THALAMIC_METRICS_IP` (default: 127.0.0.1; port is always 9000)
- `--step-interval-ms` / `THALAMIC_STEP_INTERVAL_MS` (default: 100) — relay loop tick interval
- `--force-software-only` / `THALAMIC_FORCE_SOFTWARE_ONLY`
- `RUST_LOG` (standard for tracing)

Example with env + flag:
```bash
THALAMIC_METRICS_IP=0.0.0.0 \
  cargo run --bin thalamic-relay -- --force-software-only --step-interval-ms 50
```

## Monitoring

### Prometheus Metrics

The daemon exports metrics compatible with Prometheus. Safety state is
observable here; there is no neural-state query:

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
- **Independent safety loop**: `SafetyMachine::evaluate` has no publisher argument and is not awaited on IPC. Production uses `AbsentPublisher` until GH#40.
- **GPU Safety Monitoring**: Safety cadence every ~1 second (every 10 ticks); named states for healthy-real, warning, critical/braked, recovering, missing/stale/invalid, simulated, actuator-failure
- **Emergency Brakes**: Automatically throttles GPU power limit to 50% via `nvidia-smi -pl` on fail-closed or critical **when actuation succeeds**; 3 consecutive real Ok readings to release; warn immediately after release re-applies
- **Graceful Degradation**: Continues when `--force-software-only` is set
  (`TelemetrySource::SoftwareFallback`). NVML/driver failure without that
  flag is `NvmlUnavailable` and fail-closes safety. Simulation is never
  inferred from magic numbers such as `temperature <= 0 && power <= 25`

## Telemetry contract

Every GPU reading is a typed `TelemetrySample` with `value: Option<T>`,
`observed_at`, `source`, `validity`, and `unit`. See
[`docs/telemetry.md`](docs/telemetry.md) for the full inventory.

| Signal | Class | Notes |
| --- | --- | --- |
| `gpu_temp_c`, `power_w` | safety + runtime-input | Missing/invalid/stale fail closed |
| `gpu_clock_mhz`, `mem_util_pct` | runtime-input | Sensory mapping toward `#40` |
| `vram_temp_c`, `mem_clock_mhz`, `fan_speed_pct` | observability-only | Raw preserved; not mapping input |
| `vddcr_gfx_v` | observability-only (derived) | Historical GFX-rail name; estimated from power; not an NVML voltage sensor |

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

This crate is not yet published to crates.io. Publication for 0.2.0 is
tracked as [GH#44](https://github.com/rmems/thalamic-relay/issues/44).
To cut a tag and GitHub Release for a `0.1.x` patch:

1. Make sure `CHANGELOG.md` is up to date and the version in `Cargo.toml` matches the intended release.
2. Run the validation suite locally:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
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

1. **Telemetry**: GPU access requires NVML. Without `--force-software-only`,
   a missing GPU is `NvmlUnavailable` (fail-closed), not simulated idle.
2. **GPU Access**: Verify NVML installation and proper permissions.
3. **Brake actuation**: Requires passwordless `sudo -n nvidia-smi`. Failures
   are `ActuatorError` / `actuator_failure`, not a frozen safety loop.
4. **Instance Conflicts**: Check for an existing relay process holding the lockfile
   at `/tmp/thalamic_relay.lock`

### Debug Mode

Enable debug logging for detailed troubleshooting:

```bash
RUST_LOG=debug cargo run --bin thalamic-relay
```
