# Thalamic Relay — Review Guide

## Running Tests

```bash
cargo test                    # all tests
cargo test --lib              # src/lib.rs tests (telemetry + gpu + cpu)
cargo test --bin thalamic-relay  # src/main.rs tests only (11)
cargo test telemetry          # typed contract / fixtures
cargo test gpu                # gpu acquisition + check_safety facade
cargo test safety              # SafetyMachine + named states (no GPU)
cargo test publish             # IPC isolation / failing publisher / bounded queue
cargo test cpu                # cpu metrics tests
cargo test queue              # bounded queue capacity, policy, overflow counters
cargo test metrics            # metrics defaults, safety snapshot copy, queue exposition
cargo test test_safety        # safety-related tests
cargo test lock_guard         # single-instance lockfile tests
```

### Test inventory

| Binary | Test | What it covers |
|--------|------|----------------|
| lib.rs | `healthy_real_is_valid_nvml_with_legitimate_zero_util` | Healthy NVML fixture; 0% util ≠ missing |
| lib.rs | `software_fallback_is_explicit_source_not_magic_values` | Software fallback provenance |
| lib.rs | `nvml_matching_old_magic_is_not_simulated` | `0°C`/`25W` NVML is not simulated |
| lib.rs | `stale_samples_keep_raw_but_are_not_valid` | Stale keeps raw, normalized `None` |
| lib.rs | `sensor_dropout_is_missing_not_zero` | Dropout is `None`, not `0.0` |
| lib.rs | `non_finite_is_invalid_with_none_value` | NaN → invalid + `None` |
| lib.rs | `out_of_range_keeps_raw_and_is_invalid` | Out-of-range preserves raw |
| lib.rs | `normalization_is_deterministic` | Linear `[0, 1]` mapping |
| lib.rs | `sensory_mapping_omits_observability_only_without_filler` | corpus-ipc mapping surface |
| lib.rs | `observability_snapshot_preserves_raw_independently_of_normalization` | Raw vs normalized split |
| lib.rs | `missing_runtime_input_is_not_normalized_to_zero` | Missing ≠ normalized 0 |
| lib.rs | `inventory_classifies_every_gpu_signal` | Signal class/unit/range inventory |
| lib.rs | `test_raw_software_fallback_never_uses_silent_zero_for_missing_vram` | VRAM stays missing in fallback |
| lib.rs | `test_read_telemetry_force_software_only` | Forced software-only telemetry read |
| lib.rs | `test_is_gpu_healthy_does_not_panic` | GPU health check no-panic |
| lib.rs | `test_safety_ok_on_simulated_values` | Simulated telemetry skips safety |
| lib.rs | `test_safety_does_not_infer_simulation_from_old_magic_values` | No magic-value sim inference |
| lib.rs | `test_safety_warn_on_elevated_temp` | 75-85°C warning threshold |
| lib.rs | `test_safety_warn_on_elevated_power` | 300-350W warning threshold |
| lib.rs | `test_safety_critical_on_high_temp` | >85°C critical threshold |
| lib.rs | `test_safety_critical_on_high_power` | >350W critical threshold |
| lib.rs | `test_safety_critical_on_non_finite_telemetry` | NaN/Inf telemetry handling |
| lib.rs | `test_safety_critical_on_unknown_power_with_real_temperature` | Missing power with real temp |
| lib.rs | `test_check_safety_fail_closes_on_missing_temp_or_power` | Missing/None does not skip safety |
| lib.rs | `test_nvml_unavailable_fail_closes_safety` | Unavailable NVML is not simulated |
| lib.rs | `test_acquire_raw_force_software_is_fallback_not_unavailable` | force vs unavailable provenance |
| lib.rs | `future_observed_at_is_invalid_not_valid` | Future timestamps are Invalid |
| lib.rs | `mapping_re_evaluates_stale_and_carries_thresholds` | Mapping-time freshness + stale_after |
| lib.rs | `mapping_reports_configured_acquisition_cadence` | Actual vs default cadence |
| lib.rs | `freshness_seconds_grows_when_export_time_advances` | Gauge age at scrape time |
| lib.rs | `test_safety_ok_on_normal_telemetry` | Normal readings pass |
| lib.rs | `test_safety_critical_on_stale_and_out_of_range` | Stale/OOR fail closed |
| lib.rs | `test_sensory_mapping_from_software_fallback_carries_provenance` | Mapping carries fallback source |
| lib.rs | `relay_metrics_default_values` | RelayMetrics default acquired_at |
| lib.rs | `record_safety_snapshot_copies_state_and_brake` | Shared metrics copy safety snapshot |
| lib.rs | `healthy_real_is_named_state` … `transitions_are_counted` | `SafetyMachine` GPU-less transitions (`src/safety.rs`) |
| lib.rs | `absent_brainstem_does_not_block_or_change_safety` … | IPC/Brainstem absence cannot stall safety (`src/publish.rs`) |
| lib.rs | `queue_capacity_is_validated` … `queue_metrics_exposition_shows_forced_overflow` | Bounded queue, policies, drop-reason cardinality (`src/publish.rs`) |
| lib.rs | `metrics_queue_snapshot_names_are_stable` | Queue Prometheus names under overflow (`src/cpu.rs`) |
| main.rs | `dashboard_shows_only_valid_present_as_live_hardware` | Dashboard hides stale/invalid/missing |
| main.rs | `parses_custom_args_and_env_equiv` | CLI flag parsing |
| main.rs | `parses_defaults` | CLI defaults (including sensory-queue capacity/policy) |
| main.rs | `parses_sensory_queue_config` / `rejects_zero_queue_capacity` / `rejects_unknown_queue_policy` | Queue CLI validation |
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

- Software-only GPU telemetry fallback (`HardwareBridge::read_telemetry_force(true)`, `TelemetryFrame::to_sensory_mapping`, `is_gpu_healthy` no-panic).
- Single-instance lockfile lifecycle (`try_acquire_lock`, active PID rejection, stale lock reclamation, create/remove behavior).

### Gaps explicitly deferred

- `cpu::init_telemetry` / `cpu::run_metrics_collector`: bind a network port and run an indefinite loop; not safely unit-testable without an integration harness.
- `NvmlActuator` (`apply_emergency_brake` / `release_emergency_brake` / `detect_engaged_brake`, GH#46): the NVML/`nvidia-smi` backend requires a real NVML-capable GPU and can mutate board power limits, so its hardware path is intentionally not exercised in software-only CI. It fails closed with a typed `ActuatorError` when NVML is unavailable (regression-tested), and the brake/release *policy* it serves is unit-tested via `SafetyMachine` plus the in-memory `safety::FakeActuator`.
- The `main` async supervisor loop: covered by manual run instructions and end-to-end smoke tests rather than unit tests. Dashboard reading formatting is unit-tested. Hysteresis and IPC isolation are unit-tested via `SafetyMachine` / `evaluate_then_try_publish` without driving `main`, and the actuation boundary is exercised through the `SafetyActuator` trait with `FakeActuator`.

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
