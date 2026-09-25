# Sensory IPC contract (`corpus-ipc`)

As of [RM-1143 / GH#39](https://github.com/rmems/thalamic-relay/issues/39),
`thalamic-relay` no longer runs an in-process spiking neural network, and the
retired UDP **control** surface (`Stimuli` / `LearningReward` / `GetNeuroState`
at `127.0.0.1:9898`) is gone. That protocol existed only to drive and query the
relay's own SNN.

Thalamic is a sensory + hardware-safety relay: it collects, validates,
and safety-gates GPU telemetry, independent of whether any downstream neural
runtime (`brainstem-daemon`) is present. Safety evaluation is documented in
[`docs/safety.md`](safety.md) and does not wait on this transport. It publishes validated,
normalized runtime-input channels to Brainstem as canonical
[`corpus-ipc`](https://github.com/Limen-Neural/corpus-ipc) `IpcMessage`
values ([GH#40](https://github.com/rmems/thalamic-relay/issues/40) /
[RM-1144](https://linear.app/rpd-34/issue/RM-1144)). Hardware safety does
**not** wait on this transport. There is still no control/query IPC: the
relay does not listen, does not accept `LearningReward`, and does not
expose neural state.

Prometheus metrics remain on `:9000/metrics`.

The typed validity / freshness / provenance / normalization contract
(GH#41) lives in [`docs/telemetry.md`](telemetry.md) and
`thalamic_relay::telemetry`. Frame ordering and timestamp provenance
(RM-1335) live in `thalamic_relay::time`: `session_id` / `batch_id` match
corpus-ipc `StimulusBatch`, source wall time is preserved separately from
receive/emit time, and a process restart is a new `session_id`; telemetry
`SampleClock` in `thalamic_relay::time` resets `batch_id` to 0 (wire
allocation below). `TelemetryFrame::to_sensory_mapping()` is the
deterministic mapping surface toward `corpus-ipc`; it is **not** a second
wire schema and does not implement transport. `publish` maps that into
`corpus_ipc::StimulusBatch` and sends `IpcMessage::Stimuli` via
`CorpusIpcPublisher`: a bounded `IsolatedPublishQueue` (capacity finite and
configurable, full-queue policy explicit `drop-oldest` or `reject-newest`,
overflow visible on Prometheus) feeding a detached UDP worker.
`AbsentPublisher` remains the GH#42 isolation stub for “no consumer at all”.
A stalled consumer, full queue, or missing drain cannot stall the safety
loop, and transport failures never stall `SafetyMachine::evaluate`.

## Ownership

| State | Owner |
| --- | --- |
| Hardware telemetry, validity, freshness, provenance | Thalamic (`telemetry` + `gpu`) |
| Hard-safety classification and brake intent | Thalamic (`safety`) — never awaits IPC |
| Canonical sensory wire schema | `corpus-ipc` (`IpcMessage`, `StimulusBatch`) |
| SNN tick, neuromodulation, neural state | Brainstem |

Relay/hardware state is observable from Thalamic (dashboard, Prometheus
safety gauges). Neural-runtime state is Brainstem's and is not queried here.

## Wire schema

One datagram = one JSON [`IpcMessage`](https://github.com/Limen-Neural/corpus-ipc)
value, serde **externally tagged**. Production emits only:

```json
{
  "Stimuli": {
    "session_id": "thalamic-relay",
    "batch_id": 1,
    "timestamp": 1700000000000000000,
    "values": [0.65, 0.5714286, 0.6, 0.0],
    "valid_mask": [true, true, true, true],
    "metadata": {
      "processing_latency_ns": null,
      "source": "thalamic-relay",
      "custom": {
        "acquisition_source": "nvml",
        "acquisition_cadence_ms": "100",
        "channels": "gpu_temp_c,power_w,gpu_clock_mhz,mem_util_pct",
        "validity": "valid,valid,valid,valid",
        "stale_after_ms": "2000,2000,5000,5000"
      }
    }
  }
}
```

This is the published `corpus-ipc` 0.1.0 type, not a local duplicate.
`valid_mask[i] == false` means channel `i` has no valid reading this tick;
the corresponding `values[i]` is the corpus-ipc placeholder `0.0` and is
**not** a real zero. A legitimate idle reading (for example
`mem_util_pct = 0.0` while `Valid`) has `valid_mask[i] = true`.

`timestamp` is the mapping/emission wall time in Unix-epoch nanoseconds:
Thalamic's `emitted_at_unix_ms` multiplied by 1,000,000 with saturation on
overflow. It is not `source_unix_ms`, and copying either millisecond field
without conversion would make the corpus-ipc timestamp 1,000,000× too small.
Freshness is computed before encoding from the frame's receive time, as
specified in [`telemetry.md`](telemetry.md); this wire timestamp is not its
authoritative freshness clock.
`batch_id` on the wire is allocated by `CorpusIpcPublisher` (counter
initialized to 1; each attempt uses `fetch_add` before validation, so the
first emitted batch after process start is `1`, not `0`). Rejected attempts
can leave gaps. `session_id` comes from
`--ipc-session-id` / `THALAMIC_IPC_SESSION_ID` (default `thalamic-relay`).

Channel order is the GH#41 runtime-input inventory (no observability filler):

| Index | Signal | Normalized when `Valid` |
| --- | --- | --- |
| 0 | `gpu_temp_c` | linear 0…100 °C → `[0, 1]` |
| 1 | `power_w` | linear 0…350 W → `[0, 1]` |
| 2 | `gpu_clock_mhz` | linear 0…2500 MHz → `[0, 1]` |
| 3 | `mem_util_pct` | linear 0…100 % → `[0, 1]` |

`acquisition_source` is `nvml`, `software_fallback`, or `nvml_unavailable`.
Hardware-policy thresholds (85 °C / 350 W) are **not** part of this schema.

## Transport

Default: UDP to `127.0.0.1:9900` (`--ipc-endpoint` /
`THALAMIC_IPC_ENDPOINT`). The supervisor binds an ephemeral local socket and
`sendto`s one JSON datagram per frame. It does **not** bind the destination
port, does not `recv`, and does not speak the retired 9898 control protocol.

`--ipc-disabled` / `THALAMIC_IPC_DISABLED` uses `AbsentPublisher` (no worker).
Invalid endpoint or a bind failure logs and continues; safety still evaluates.

This crate depends on `corpus-ipc` **without** the `zmq` feature. ZeroMQ
in `corpus-ipc` 0.1.0 is a binary readout **subscriber**, not a
`StimulusBatch` publisher, and would pull a C++ vendor build. JSON
`IpcMessage` is the single wire schema; UDP is only the datagram carrier.

## Isolation from the safety loop

```text
TelemetryFrame
      │
      ▼  SafetyMachine::evaluate   ← never blocks, never IPC
SafetySnapshot (state, brake, intent)
      │
      ├─ spawn_blocking apply/release   (gpu, not on the eval path)
      └─ SensoryPublisher::try_publish   (best-effort, after eval)
             IsolatedPublishQueue.try_enqueue  (drop on full)
                   │
                   ▼  detached worker (not awaited)
             serde_json(IpcMessage::Stimuli) → UDP sendto
```

A full or disconnected queue is `SlowConsumer` / `Disconnected`. UDP send
errors and a missing Brainstem listener are dropped on the worker. None of
these paths can stall or disable `SafetyMachine::evaluate`.
