# Thalamic Relay — Review Guide

## Running Tests

```bash
cargo test                    # all tests (19 total)
cargo test --lib              # src/lib.rs tests only (13: gpu + cpu)
cargo test --bin thalamic-relay  # src/main.rs tests only (6)
cargo test gpu                # gpu safety tests (12)
cargo test cpu                # cpu metrics tests (1)
cargo test test_safety        # safety-related tests
cargo test lock_guard         # single-instance lockfile tests
```

### Test inventory

| Binary | Test | What it covers |
|--------|------|----------------|
| lib.rs | `test_telemetry_struct` | GpuTelemetry default values |
| lib.rs | `test_telemetry_to_rails` | GpuTelemetry rail conversion |
| lib.rs | `test_read_telemetry_force_software_only` | Forced software-only telemetry read |
| lib.rs | `test_is_gpu_healthy_does_not_panic` | GPU health check no-panic |
| lib.rs | `test_safety_ok_on_simulated_values` | Simulated telemetry skips safety |
| lib.rs | `test_safety_warn_on_elevated_temp` | 75-85°C warning threshold |
| lib.rs | `test_safety_warn_on_elevated_power` | 300-350W warning threshold |
| lib.rs | `test_safety_critical_on_high_temp` | >85°C critical threshold |
| lib.rs | `test_safety_critical_on_high_power` | >350W critical threshold |
| lib.rs | `test_safety_critical_on_non_finite_telemetry` | NaN/Inf telemetry handling |
| lib.rs | `test_safety_critical_on_unknown_power_with_real_temperature` | Missing power with real temp |
| lib.rs | `test_safety_ok_on_normal_telemetry` | Normal readings pass |
| lib.rs | `relay_metrics_default_values` | RelayMetrics defaults to 0 |
| main.rs | `parses_custom_args_and_env_equiv` | CLI flag parsing |
| main.rs | `parses_defaults` | CLI defaults |
| main.rs | `parses_force_software_only_false` | `--force-software-only=false` |
| main.rs | `lock_guard_created_and_removed` | Lockfile create/drop lifecycle |
| main.rs | `lock_guard_rejects_active_pid` | Second instance refused while PID alive |
| main.rs | `lock_guard_reclaims_stale_lock` | Stale lock (dead PID) reclaimed |

As of RM-1143 (GH#39), `thalamic-relay` no longer runs an in-process SNN or
exposes a UDP control surface — the `Stimuli`/`LearningReward`/`GetNeuroState`
tests and the `num-channels`/`num-lif`/`num-izh` CLI tests were removed along
with that code. See [`docs/ipc.md`](docs/ipc.md).

## Coverage update (RM-342)

### Gaps closed (at the time, since partly superseded by RM-1143's UDP/SNN removal above)

- Software-only GPU telemetry fallback (`HardwareBridge::read_telemetry_force(true)`, `GpuTelemetry::to_rails`, `is_gpu_healthy` no-panic).
- Single-instance lockfile lifecycle (`try_acquire_lock`, active PID rejection, stale lock reclamation, create/remove behavior).

### Gaps explicitly deferred

- `cpu::init_telemetry` / `cpu::run_metrics_collector`: bind a network port and run an indefinite loop; not safely unit-testable without an integration harness.
- `HardwareBridge::apply_emergency_brake` / `release_emergency_brake` / `power_limit_matches_emergency_brake`: require a real NVML-capable GPU and can mutate board power limits, so they are intentionally not exercised in software-only CI.
- The `main` async supervisor loop and `print_dashboard`: covered by manual run instructions and end-to-end smoke tests rather than unit tests.

## Linting

```bash
cargo clippy --all-targets -- -D warnings   # lint (CI uses --all-features too)
cargo fmt --check                           # format check
cargo fmt                                   # auto-format
```

## Building

```bash
cargo build                  # debug build
cargo build --release        # release build
cargo build --all-features   # CI-equivalent build
```

## Running

```bash
cargo run -- --help                          # show CLI options
cargo run -- --force-software-only           # software-only mode (no GPU)
cargo run -- --metrics-ip 0.0.0.0            # custom Prometheus bind
cargo run -- --step-interval-ms 50           # faster tick interval
```

## CI Checks

The CI workflow (`.github/workflows/ci.yml`) pins Rust 1.98.1:

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo build --all-features`
4. `cargo test --all-features`

Bot reviewers: Codacy, CodeRabbit, Codex, Kilo, Devin, Gitar, Cursor Bugbot.
