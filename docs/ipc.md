# UDP IPC Message Contract — Removed

As of [RM-1143 / GH#39](https://github.com/rmems/thalamic-relay/issues/39),
`thalamic-relay` no longer runs an in-process spiking neural network, and the
UDP control surface this document used to describe (`Stimuli`,
`LearningReward`, `GetNeuroState`) has been removed along with it. That
protocol existed only to drive and query the relay's own SNN; with the SNN
gone, there is nothing left for it to control.

Thalamic is now a sensory + hardware-safety relay: it collects, validates,
and safety-gates GPU telemetry, independent of whether any
downstream neural runtime (`brainstem-daemon`) is present. It currently
exposes no control/query IPC surface — only Prometheus metrics on
`:9000/metrics`.

The typed validity / freshness / provenance / normalization contract
(GH#41) now lives in [`docs/telemetry.md`](telemetry.md) and
`thalamic_relay::telemetry`. `TelemetryFrame::to_sensory_mapping()` is the
deterministic mapping surface toward `corpus-ipc`; it is **not** a second
wire schema and does not implement transport.

Transport (Thalamic → `corpus-ipc` → `brainstem-daemon`) remains follow-up
work — see GH#40 (`RM-1144`). This file will be replaced with that contract's
normative wire reference once it lands.
