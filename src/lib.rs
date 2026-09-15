//! Sensory + deterministic hardware-safety library for GPU telemetry.
//!
//! `thalamic-relay` observes hardware, validates it, and evaluates an isolated
//! thermal/power protection policy. It does **not** run a spiking neural
//! network, own neural state, or perform neuromorphic inference. Neural
//! execution, if present at all, lives in a separate process
//! (`brainstem-daemon`).
//!
//! This crate is both:
//!
//! - a **library** of telemetry, safety, and (optional) publish types a
//!   downstream crate can use without starting the daemon;
//! - the **`thalamic-relay` binary**, a long-running supervisor that acquires
//!   GPU samples, drives the safety machine, and exports Prometheus metrics.
//!
//! # Architecture
//!
//! ```text
//! telemetry source (NVML | SoftwareFallback | NvmlUnavailable)
//!         │
//!         ▼  validate / normalize / freshness / provenance
//! TelemetryFrame
//!         │
//!         ├─ SafetyMachine::evaluate     deterministic policy (no I/O)
//!         │         │
//!         │         ▼
//!         │   BrakeIntent → SafetyActuator   best-effort privileged side effect
//!         │
//!         └─ SensoryPublisher::try_publish   best-effort, never on the eval path
//!                   │
//!                   ▼
//!            corpus-ipc transport            not implemented (see IPC status)
//! ```
//!
//! Safety evaluation is an isolated failure domain: it continues with
//! Brainstem absent, and a slow or missing publisher cannot stall it.
//!
//! # Software-only versus real hardware
//!
//! | Path | How you get it | What the numbers are | Safety |
//! | --- | --- | --- | --- |
//! | [`telemetry::TelemetrySource::Nvml`] | Live NVIDIA device via NVML | Real sensors | Thresholds apply; missing/stale/invalid **fail closed** |
//! | [`telemetry::TelemetrySource::SoftwareFallback`] | `--force-software-only` only | Documented idle **estimates** | Named `simulated_software_only`; does **not** apply a new brake (no real GPU to protect) |
//! | [`telemetry::TelemetrySource::NvmlUnavailable`] | NVML/driver/device lookup failed without that flag | Every channel **missing** | **Fail closed** (not simulated) |
//!
//! Simulated idle is never inferred from magic numbers such as
//! `temperature <= 0 && power <= 25`.
//!
//! # GPU power-limit actuation (privileges)
//!
//! Policy ([`safety::SafetyMachine`]) is deterministic and unprivileged.
//! Applying or releasing a brake is a best-effort side effect on Linux:
//! `timeout` + `sudo -n nvidia-smi -pl <watts>`. That requires
//! **passwordless sudo** for `nvidia-smi`, a working NVIDIA driver, and NVML
//! (`libnvidia-ml.so`). Without those, actuation returns
//! [`safety::ActuatorError`] and the machine records
//! [`safety::SafetyState::ActuatorFailure`] while continuing to evaluate.
//!
//! # What is deterministic versus best-effort
//!
//! **Deterministic** (pure functions of inputs + machine state, GPU-less
//! testable):
//!
//! - [`telemetry::assess`] / [`telemetry::TelemetryFrame::from_raw`]
//! - freshness, validity, and `[0, 1]` normalization
//! - [`telemetry::TelemetryFrame::to_sensory_mapping_at`]
//! - [`safety::SafetyMachine::evaluate`] / [`safety::classify_frame`]
//!
//! **Best-effort** (can fail, time out, or be unavailable):
//!
//! - NVML acquisition and `nvidia-smi` liveness checks
//! - privileged power-limit apply/release
//! - leftover-brake detection at process start
//! - Prometheus scrape and logs
//! - sensory publication (currently always absent)
//!
//! This library is not a substitute for GPU firmware thermal protection.
//! A fail-closed *intent* does not guarantee the board power limit changed.
//!
//! # IPC status
//!
//! There is **no** control/query IPC surface and **no** `corpus-ipc`
//! transport in this crate. The retired UDP protocol (`Stimuli` /
//! `LearningReward` / `GetNeuroState`) was removed with the in-process SNN.
//! [`telemetry::SensoryMapping`] is a typed mapping hook toward a future
//! publisher; production uses [`publish::AbsentPublisher`]. Prometheus on
//! `:9000/metrics` is the only process-level observation channel.
//!
//! # Library example (no GPU)
//!
//! ```
//! use thalamic_relay::safety::{SafetyMachine, SafetyState};
//! use thalamic_relay::telemetry::{assess, fixtures, TelemetrySource};
//!
//! let frame = assess(&fixtures::software_fallback(), fixtures::NOW);
//! assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
//!
//! let mut machine = SafetyMachine::new();
//! let snap = machine.evaluate(&frame);
//! assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
//! assert!(!snap.desired_brake);
//! ```
//!
//! The `thalamic-relay` binary is documented by `thalamic-relay --help` and
//! the crate README. Normative contracts live in the repository files
//! `docs/telemetry.md`, `docs/safety.md`, and `docs/ipc.md`.
//!
//! # Modules
//!
//! - [`telemetry`] — sample contract and sensory mapping types
//! - [`safety`] — pure policy/state machine and actuation trait
//! - [`gpu`] — NVML acquisition and `nvidia-smi` actuator
//! - [`publish`] — non-blocking publish stub (no transport)
//! - [`cpu`] — Prometheus / process metrics used by the daemon

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod cpu;
pub mod gpu;
pub mod publish;
pub mod safety;
pub mod telemetry;
