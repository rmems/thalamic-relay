# Thalamic Relay

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/rmems/thalamic-relay#license)

A lightweight **library** (`thalamic_relay`) and **CLI** (`thalamic-relay`)
that observes hardware telemetry and provides deterministic hardware
safety for the Spikenaut runtime stack (software-only;
silicon-bridge/**FPGA (Field-Programmable Gate Array)** bridge dep removed
for modularity).

## Library vs executable

This crate ships two surfaces. They are not interchangeable:

| Surface | Crate / binary | Use it when |
| --- | --- | --- |
| **Library** | `thalamic_relay` (`telemetry`, `safety`, `publish`) | A downstream crate needs typed samples, `SafetyMachine`, or a best-effort publisher **without** running the daemon |
| **Executable** | `thalamic-relay` | You want the supervisor process: NVML acquisition, privileged power-limit brake, Prometheus on `:9000`, single-instance lock |

```rust
use thalamic_relay::safety::{SafetyMachine, SafetyState};
use thalamic_relay::telemetry::{assess, fixtures};

let frame = assess(&fixtures::healthy_real(), fixtures::NOW);
let policy = thalamic_relay::safety::SafetyPolicyConfig::default()
    .resolve(Some(400.0)).unwrap(); // Example device default: 400 W
let mut machine = SafetyMachine::with_policy(policy);
let snapshot = machine.evaluate(&frame);
assert_eq!(snapshot.state, SafetyState::HealthyReal);
```

The NVML/`nvidia-smi` adapter, Prometheus exporter, clap CLI, and
`/tmp/thalamic_relay.lock` are **not** part of the library API (they are
private process plumbing). Public items are documented; missing rustdoc on
that surface is a compile error (`#![deny(missing_docs)]`). This is a
pre-1.0 crate: the library API is intentional, not frozen.

## Overview

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
- **Process Safety**: Single-instance protection via a lockfile mechanism;
  SIGINT/SIGTERM orderly shutdown with fail-closed emergency-brake recovery

## Installation

### Prerequisites

- **Edition:** Rust 2024 (requires rustc/Cargo ≥ 1.85 to parse `edition = "2024"`)
- **MSRV:** 1.98.1 (`package.rust-version` in `Cargo.toml` is authoritative; CI installs exactly that toolchain). Edition and MSRV are not the same number: 2024 became usable in 1.85, while this crate’s declared floor is the policy pin 1.98.1.
- `pkg-config` (used by some native dependencies)
- Linux operating system (tested on Linux)
- Optional: an NVIDIA GPU with NVML support
- Optional, for actual power-limit actuation: passwordless `sudo` for
  `nvidia-smi`

The first intended crates.io version is **0.2.0**, currently prepared for
publication. Until the maintainer publishes it, build from this repository.
After publication, install the CLI with `cargo install thalamic-relay --version
0.2.0 --locked`, or add `thalamic-relay = "0.2.0"` to a library consumer.

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
| **Thalamic** | Hardware telemetry contract; fail-closed **intent** on missing/stale/invalid telemetry; brake apply/release **attempts**; observable safety/brake state **without** querying neural state; continues with Brainstem absent |
| **Brainstem** | SNN execution, neural state, reward/plasticity. Consumes sensory mappings over corpus-ipc UDP. **Cannot** inhibit or override the Thalamic brake |
| **corpus-ipc** | Transport only. Send failure is not a safety pause |

See [`docs/safety.md`](docs/safety.md) for named states and hysteresis
rules, and [`docs/telemetry.md`](docs/telemetry.md) for the sample contract.

### Deterministic versus best-effort

**Deterministic** (pure; GPU-less testable): `assess` / `TelemetryFrame`,
freshness/validity/normalization, `to_sensory_mapping_at`,
`SafetyMachine::evaluate`.

**Best-effort** (can fail or time out): NVML acquisition, `nvidia-smi`
liveness, privileged power-limit apply/release, leftover-brake detection,
Prometheus, best-effort sensory publication.

### Public library modules

Reusable from a downstream crate (no GPU, no supervisor process):

- **`telemetry`**: Typed sample contract (validity, freshness, provenance, normalization) and the corpus-ipc mapping surface
- **`time`**: Process-local sample clock (`session_id` + `batch_id`) and timestamp provenance
- **`telemetry_csv`**: Frozen hardware-telemetry CSV header + reader/validator for corinth ingest (one-way copy; no corinth dependency)
- **`safety`**: Pure deterministic classification + hysteresis (`SafetyMachine`) and the `SafetyActuator` trait; no NVML, no IPC
- **`publish`**: Maps `SensoryMapping` → `corpus-ipc` `StimulusBatch` / `IpcMessage::Stimuli` and UDP-publishes off the safety path (`CorpusIpcPublisher`, `AbsentPublisher`, `IsolatedPublishQueue`)

Binary-only (not semver-facing): NVML acquisition (`gpu`), CPU metrics (`cpu`), privileged `nvidia-smi`
actuation, Prometheus initialization, CLI, process lock, supervisor loop, SIGINT/SIGTERM shutdown.

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

The daemon supports CLI flags **and** environment variables (clap derive +
"env" feature). Defaults preserve prior hardcoded behavior.

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

### Safety policy

The daemon resolves an immutable `SafetyPolicyConfig` at startup and prints
`effective_safety_policy` with the effective limits and their provenance.
Invalid or contradictory settings stop startup before locks, ports or workers.

| Flag (environment variable uses `THALAMIC_` plus the uppercase flag with underscores) | Default / meaning |
| --- | --- |
| `--safety-temp-warn-c` / `--safety-temp-critical-c` | 75 / 85 C; explicit operator defaults, not vendor thermal limits |
| `--safety-power-warn-w` / `--safety-power-critical-w` | Both omitted: 85% / 100% of NVML's device default power limit; supply both to override |
| `--safety-release-ok-streak` | 3 consecutive healthy real evaluations |
| `--safety-max-sample-age-ms` | 2000 ms; may tighten the telemetry contract |
| `--safety-max-acquisition-interval-ms` | 100 ms; bounds the declared acquisition interval |

For example, `--safety-power-warn-w 80 --safety-power-critical-w 95` sets an
explicit 80/95 W envelope (`THALAMIC_SAFETY_POWER_WARN_W` and
`THALAMIC_SAFETY_POWER_CRITICAL_W` are equivalent). A reported device default
below 95 W rejects this configuration. Limits must be finite, positive and
ordered warning < critical. Thermal critical is at most 125 C; power critical
and device default are at most the telemetry sanity bound of 2000 W.

Explicit paired watts take precedence over derived watts. If neither a usable
device default nor paired overrides exists, real frames fail closed as
`telemetry_missing` with a power-policy-unavailable reason. Tagged software-only
frames remain simulated and cannot release an adopted real brake. The library's
`SafetyMachine::new()` also has no device envelope: use
`SafetyPolicyConfig::resolve` and `SafetyMachine::with_policy` for real telemetry.

The default power limit is a device capability used as a policy ceiling, not a
claim about safe sustained operation for every workload. NVML queries target
device index 0; this release is not a multi-GPU supervisor. It does not infer
vendor thermal limits or overclock settings. The brake remains 50% of default
to keep restart matching consistent. Foreign sub-default caps are refused by
the actuator and reported as `actuator_failure`; the relay does not claim them
as its brake or restore them to default. Detection is a 2 W target match, not
proof of ownership. An external cap exactly matching the target is indistinguishable,
and independent operator changes can race the read/command sequence.

The daemon checks `step_interval_ms <= max_acquisition_interval_ms` and
`10 * step_interval_ms <= max_sample_age_ms`. Evaluation remains every ten
iterations, plus startup and post-actuation checks. Acquisition and scheduling
add elapsed time: these checks do not promise a wall-clock response deadline.
Library sample-age checks use the frame's assessment time; reassess held frames
before calling `evaluate`. See [the safety contract](docs/safety.md).

## Monitoring

### Prometheus Metrics

The daemon exports metrics compatible with Prometheus. Safety state is
observable here; there is no neural-state query:

- `telemetry_freshness_s` — sample age at scrape time (monotonic receive instant, not source wall time)
- `safety_state{state=...}` / `safety_state_id` — current named safety state
- `safety_policy_state{state=...}` — policy classification before the ActuatorFailure overlay (`safety_state` is the overlay)
- `safety_brake_engaged` — last successful brake still claimed
- `safety_hysteresis_ok_count` — Ok streak while braked
- `safety_transitions_total` / `safety_actuator_failures_total` — counters
- `shutdown_total{reason}` / `shutdown_unresolved_brake` /
  `shutdown_unresolved_actuator` / `shutdown_brake_left_engaged` — last
  orderly shutdown (SIGINT/SIGTERM)

See [`docs/safety.md`](docs/safety.md) for the label set, numeric ids,
and fail-closed shutdown/restart rules.

### Logging

Structured logging via `tracing` with configurable output levels.

## Safety Features

- **Instance Protection**: Lockfile mechanism prevents multiple relay instances (lock acquired before port binding)
- **Independent safety loop**: `SafetyMachine::evaluate` has no publisher argument and is not awaited on IPC. Production uses `CorpusIpcPublisher` (`try_send` + detached UDP worker). `--ipc-disabled` or a bind failure falls back to `AbsentPublisher`.
- **GPU Safety Monitoring**: Safety cadence every ~1 second (every 10 ticks); named states for healthy-real, warning, critical/braked, recovering, missing/stale/invalid, simulated, actuator-failure
- **Emergency Brakes**: Automatically throttles GPU power limit to 50% via `nvidia-smi -pl` on fail-closed or critical **when actuation succeeds**; the configured consecutive real Ok streak (default 3) to release; warn immediately after release re-applies
- **Fail-closed shutdown / restart**: Ctrl-C and SIGTERM stop the run loop, join background tasks with a timeout, and release `/tmp/thalamic_relay.lock`. Shutdown **does not** restore the default GPU power limit. A persistent relay-owned brake (current PL matching the 50% target) is adopted on the next start and released only through the same Ok-streak hysteresis. An operator-configured sub-default cap is left unchanged. Simulated/software-only telemetry cannot authorize release of a real brake. SIGKILL/power loss have no cleanup promise.
- **Graceful Degradation**: Continues in software-only mode when
  `--force-software-only` is set (`TelemetrySource::SoftwareFallback`).
  NVML/driver failure without that flag is `NvmlUnavailable` and fail-closes
  safety. Simulation is never inferred from magic numbers such as
  `temperature <= 0 && power <= 25`

## Telemetry contract

Every GPU reading is a typed `TelemetrySample` with `value: Option<T>`,
`observed_at`, `source`, `validity`, and `unit`. Every emitted frame also
carries `session_id`, a strictly increasing `batch_id`, source vs
receive/emit timestamps, and `source_time_status`. See
[`docs/telemetry.md`](docs/telemetry.md) for the full inventory.

A separate frozen **CSV interchange** for corinth-canal ingest lives in
[`docs/telemetry_csv.md`](docs/telemetry_csv.md) and
`thalamic_relay::telemetry_csv` (header
`timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w`).
Producers should validate against that module before publishing a file
corinth will read. The CSV schema is frozen; do not add columns.

| Signal | Class | Notes |
| --- | --- | --- |
| `gpu_temp_c`, `power_w` | safety + runtime-input | Missing/invalid/stale fail closed |
| `gpu_clock_mhz`, `mem_util_pct` | runtime-input | Sensory mapping → `StimulusBatch` |
| `vram_temp_c`, `mem_clock_mhz`, `fan_speed_pct` | observability-only | Raw preserved; not model input |
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

## Crate package

The crates.io artifact is an **allowlist** (`include` in `Cargo.toml`), not a
denylist, so development-only files cannot ship by accident. The package
contains:

- `src/` (library + `thalamic-relay` binary)
- consumer docs: `README.md`, `CHANGELOG.md`, and the four explicit `docs/*.md` contracts
- `examples/software_only.rs` for a GPU-less library demonstration
- `Cargo.lock` (this package has a binary)
- `LICENSE-MIT` and `LICENSE-APACHE-2.0`

Contributor and agent files (`AGENTS.md`, `CLAUDE.md`, `REVIEW.md`), CI
(`.github/`), and local tool configs (`.codacy.yml`, `.gitignore`) stay in git
and are **not** part of the `.crate`. Inspect with `cargo package --list`.

## Contributing

Contributions are welcome! Please ensure all submissions follow the project's
coding standards and include appropriate tests.

## Releasing

The first intended registry release is **0.2.0**, gated by
[GH#44](https://github.com/rmems/thalamic-relay/issues/44). The 0.1.0 changelog
entry records repository history; it is not evidence of a crates.io release.
Real publication requires explicit maintainer approval immediately before
upload. CI performs only a token-free dry run.

1. Merge the reviewed release preparation and confirm a clean `main` at the
   reviewed commit, with `HEAD` equal to `origin/main`. Check all publication
   gates and exact-commit CI/reviews. Confirm `Cargo.toml`, `Cargo.lock`, and the
   0.2.0 changelog agree; finalize its release date before the final checks.
2. Run qualification on that exact clean commit:

   ```bash
   cargo fmt --check
   cargo clippy --locked --all-targets --all-features -- -D warnings
   cargo test --locked --all-features
   RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
   cargo build --locked --release
   cargo package --list --locked
   cargo package --locked
   cargo publish --dry-run --locked
   ```

   Inspect every archive path and both license files. Unpack the `.crate`
   outside the repository and run its tests and `software_only` example;
   verify a separate consumer against the extracted library. The allowlist
   excludes contributor instructions, plans, CI, and local tool configuration.
3. Obtain explicit approval for the exact commit and 0.2.0 artifact. Only then
   run `cargo publish --locked`. A passing dry run does not verify credentials,
   reserve the crate name, or upload a release.
4. Confirm the version is retrievable from crates.io with
   `cargo info thalamic-relay@0.2.0 --registry crates-io` from outside this repo
   and build a separate consumer using the registry version. Check docs.rs
   built 0.2.0 successfully before declaring hosted documentation available.
5. After successful registry verification, create the annotated `v0.2.0` tag
   on the published commit and push it; create the GitHub Release from that
   tag using the finalized changelog. Mark the publication gate and Linear
   release complete only with the actual registry/release evidence.

For later patches, advance all version references together (for example,
`0.2.1` and `v0.2.1`) and repeat the same qualification and approval procedure.

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

Brake targets whose ±2 W matching band overlaps the device-default band are
refused as ambiguous (including default limits of 8 W or less with the 50%
strategy). They cannot be adopted as an already-engaged relay brake.
