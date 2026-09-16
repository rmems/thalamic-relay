# Safety failure domain

This is the normative hardware-protection contract for `thalamic-relay`
([GH#42](https://github.com/rmems/thalamic-relay/issues/42)).

Safety evaluation is an **isolated failure domain**. It continues with
Brainstem completely absent, and it never waits on `corpus-ipc` send,
disconnect, or a slow consumer. Brainstem has **no** authority to override
Thalamic hard-safety policy — there is no IPC command that can inhibit the
brake.

Privileged `nvidia-smi` actuation stays in private `src/gpu.rs` (used only by
the `thalamic-relay` executable). The state machine in `src/safety.rs` only
emits **intents**; hardware side effects go through [`SafetyActuator`].

## Ownership

| Guarantee | Owner |
| --- | --- |
| Telemetry acquisition, validity, freshness, provenance | Thalamic (`telemetry` + `gpu` acquire) |
| Hard-safety classification, hysteresis, brake intent | Thalamic (`safety`) |
| Power-limit apply/release | Thalamic (`gpu` actuator) |
| Safety/brake state and transition/error counters | Thalamic Prometheus (`:9000/metrics`) |
| Sensory mapping types | Thalamic (`TelemetryFrame::to_sensory_mapping`) |
| Sensory transport to Brainstem | `corpus-ipc` (GH#40, not required for safety) |
| SNN tick, neuromodulation, neural state | Brainstem |
| Reward / plasticity | Brainstem (never Thalamic) |

Thalamic does **not** run neural computation. Observing safety does **not**
require querying neural state.

## Named states

| `SafetyState` | When | Brake policy |
| --- | --- | --- |
| `healthy_real` | NVML, valid temp/power, below warn, brake released | none |
| `warning` | Valid warn-band (75–85 °C or 300–350 W) | hold if already on; **re-apply if just released** |
| `critical_braked` | Valid temp > 85 °C or power > 350 W | apply / hold |
| `recovering` | Brake on; consecutive **real** Ok streak | release after 3 Ok |
| `telemetry_missing` | Safety sample missing (`NvmlUnavailable`, dropout, Valid+`None`) | apply / hold (fail closed) |
| `telemetry_stale` | Safety sample older than stale threshold | apply / hold |
| `telemetry_invalid` | Non-finite or out of engineering range | apply / hold |
| `simulated_software_only` | `TelemetrySource::SoftwareFallback` | do not apply; **hold** if already on; reset hysteresis |
| `actuator_failure` | Last apply/release failed | overlay; retry the last intent; policy still evaluates |

`NvmlUnavailable` is **not** simulated. Missing safety samples fail closed.
`--force-software-only` is the only simulated path; simulated numbers never
drive thresholds.

When missing, invalid, and stale appear on different safety channels, the
worst fault wins: **missing > invalid > stale**. Equal rank prefers
`gpu_temp_c` (stable, matches the previous temp-then-power order).

## Hysteresis

1. Critical / missing / invalid / stale → `desired_brake = true`.
2. Warn while released (and not immediately post-release) does **not** apply.
3. Warn or critical **immediately after a successful release** re-applies
   (the restored default PL is not left in a still-hot/warn band).
4. Release requires **3 consecutive real Ok** evaluations while the brake is
   engaged (~3 s at the default 100 ms tick × every 10 ticks).
5. Simulated telemetry while braked holds the brake and resets the Ok streak.
6. Actuator failure does not freeze evaluation: the next frame still
   classifies and the intent is retried.

The supervisor evaluates the first acquired frame immediately, then on
the safety cadence (every 10 ticks) and again after an actuator task
completes, using a fresh frame. Failed apply/release is recorded and retried
on that cadence rather than every tick. `SafetyMachine::evaluate`
never publishes and never calls `nvidia-smi`. Pre-telemetry snapshots are
not dispatched as hardware commands: `--force-software-only` must be
classified first (hold, do not apply).

## IPC isolation

```text
TelemetryFrame
      │
      ▼  SafetyMachine::evaluate   ← never blocks, never IPC
SafetySnapshot (state, brake, intent)
      │
      ├─ spawn_blocking apply/release   (gpu, not on the eval path)
      └─ SensoryPublisher::try_publish   (best-effort, after eval)
             IsolatedPublishQueue.try_enqueue  (drop on full)
```

Production currently uses [`AbsentPublisher`](../src/publish.rs) (Brainstem
absent). GH#40 should plug a `corpus-ipc` publisher into
`IsolatedPublishQueue` (bounded `try_send`). A full or disconnected queue
is `SlowConsumer` / `Disconnected` and **must not** be `recv`'d from the
safety loop.

## Prometheus

Exported without querying Brainstem:

| Metric | Kind | Meaning |
| --- | --- | --- |
| `safety_state{state=...}` | gauge 0/1 | one-hot reported state |
| `safety_state_id` | gauge 0–8 | stable numeric id |
| `safety_policy_state{state=...}` | gauge 0/1 | policy classification before ActuatorFailure overlay |
| `safety_brake_engaged` | gauge 0/1 | last successful apply still claimed |
| `safety_hysteresis_ok_count` | gauge | Ok streak while braked |
| `safety_transitions_total` | counter | reported-state changes |
| `safety_actuator_failures_total` | counter | apply/release errors |
| `telemetry_freshness_s` | gauge | sample age at scrape time (monotonic receive instant) |

Numeric ids: 0 `healthy_real`, 1 `warning`, 2 `critical_braked`,
3 `recovering`, 4 `telemetry_missing`, 5 `telemetry_stale`,
6 `telemetry_invalid`, 7 `simulated_software_only`, 8 `actuator_failure`.
