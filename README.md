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
      ↓
corpus-ipc          (follow-up work — not yet implemented)
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
when no GPU is present, and hardware safety keeps functioning even when
`brainstem-daemon` is absent or crashed.

## Features

- **GPU Telemetry**: Real-time monitoring of GPU sensors via NVML (temperature,
  power, clocks, fan, utilization) with an explicit software-fallback
  provenance tag — missing sensors stay absent (`None`), never a silent `0.0`
- **Deterministic Hardware Safety**: Thermal/power threshold checks with
  emergency brake and hysteresis-gated release, independent of any
  downstream neural runtime
- **Metrics Collection**: Prometheus-compatible metrics export
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

It currently has no control/query IPC surface — the prior UDP protocol was
removed along with the in-process SNN it existed to drive; see
[`docs/ipc.md`](docs/ipc.md) for the retired UDP surface, the GH#41 mapping
types, and the planned `corpus-ipc` transport (GH#40).

## Architecture

### Core Modules

- **`telemetry`**: Typed sample contract (validity, freshness, provenance, normalization) and the corpus-ipc mapping surface
- **`gpu`**: Raw NVML acquisition and safety evaluation against the typed frame
- **`cpu`**: Telemetry initialization and metrics collection

### Key Components

1. **Hardware Bridge**: Abstract interface for GPU communication
2. **Telemetry System**: Real-time metrics collection and export
3. **Emergency Brakes**: Safety mechanisms for hardware protection

## Dependencies

### Core Dependencies

- `tokio`: Async runtime with full features
- `serde`: Serialization framework (used by the typed telemetry contract)
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
- `RUST_LOG` (standard for tracing; or --log-level in future extensions)

Example with env + flag:
```bash
THALAMIC_METRICS_IP=0.0.0.0 \
  cargo run --bin thalamic-relay -- --force-software-only --step-interval-ms 50
```

## Monitoring

### Prometheus Metrics

The relay exports metrics compatible with Prometheus monitoring — currently
just hardware telemetry freshness (`telemetry_freshness_s`).

### Logging

Structured logging via `tracing` with configurable output levels.

## Safety Features

- **Instance Protection**: Lockfile mechanism prevents multiple relay instances (lock acquired before port binding)
- **GPU Safety Monitoring**: Main loop checks thermal (85°C) and power (350W) thresholds every ~1 second
- **Emergency Brakes**: Automatically throttles GPU power limit to 50% via `nvidia-smi -pl` on critical threshold
- **Graceful Degradation**: Continues in software-only mode without GPU.
  Safety skips software-fallback frames via explicit
  `TelemetrySource::SoftwareFallback`, not from magic numbers such as
  `temperature <= 0 && power <= 25`

## Telemetry contract

Every GPU reading is a typed `TelemetrySample` with `value: Option<T>`,
`observed_at`, `source`, `validity`, and `unit`. See
[`docs/telemetry.md`](docs/telemetry.md) for the full inventory.

| Signal | Class | Notes |
| --- | --- | --- |
| `gpu_temp_c`, `power_w` | safety + runtime-input | Missing/invalid/stale fail closed |
| `gpu_clock_mhz`, `mem_util_pct` | runtime-input | Sensory mapping toward `#40` |
| `vram_temp_c`, `mem_clock_mhz`, `fan_speed_pct` | observability-only | Raw preserved; not model input |
| `vddcr_gfx_v` | observability-only (derived) | Estimated from power; not an NVML voltage sensor |

A legitimate zero (for example 0% memory utilization) is distinct from a
missing sensor. Simulated idle estimates are tagged
`TelemetrySource::SoftwareFallback`. Normalization to `[0, 1]` is
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
