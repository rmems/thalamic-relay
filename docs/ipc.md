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

A typed, normalized stimulus-egress contract (Thalamic → `corpus-ipc` →
`brainstem-daemon`) is planned as separate follow-up work — see GH#40
(`RM-1144`, emitting typed `corpus-ipc` stimuli) and GH#41 (`RM-1145`,
defining the normalized stimulus contract with validity/staleness/
provenance). This file will be replaced with that contract's normative
reference once it lands.
