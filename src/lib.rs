//! Typed hardware telemetry and deterministic safety for the Spikenaut stack.
//!
//! This crate is both a **library** (`thalamic_relay`) and the
//! `thalamic-relay` **executable**. Downstream crates should depend on the
//! library surface below — telemetry validation, [`safety::SafetyMachine`],
//! and best-effort [`publish::SensoryPublisher`] — without starting the
//! supervisor, binding Prometheus, or taking the process lock.
//!
//! ```
//! use thalamic_relay::safety::{SafetyMachine, SafetyState};
//! use thalamic_relay::telemetry::{assess, fixtures};
//!
//! let frame = assess(&fixtures::healthy_real(), fixtures::NOW);
//! let mut machine = SafetyMachine::new();
//! let snapshot = machine.evaluate(&frame);
//! assert_eq!(snapshot.state, SafetyState::HealthyReal);
//! assert!(!snapshot.desired_brake);
//! ```
//!
//! Architecture:
//!
//! ```text
//! RawTelemetry  →  assess()  →  TelemetryFrame
//!                                   ├─ SafetyMachine::evaluate()  →  BrakeIntent
//!                                   ├─ to_sensory_mapping()       →  corpus-ipc
//!                                   └─ SensoryPublisher::try_publish()  (never on evaluate)
//! ```
//!
//! The crate does **not** run neural computation. NVML acquisition, privileged
//! `nvidia-smi` actuation, Prometheus initialization, CLI parsing, and the
//! single-instance lock live in private modules used only by the executable.
//!
//! # Stability
//!
//! Pre-1.0: this public surface is intentional but not frozen. Items that are
//! not reachable from the modules below are not semver-facing API.
//!
//! # Missing-docs policy
//!
//! Every public item must have rustdoc (`#![deny(missing_docs)]`). Private
//! binary plumbing is undocumented by design.
//!
//! ```compile_fail
//! use thalamic_relay::gpu::HardwareBridge;
//! ```
//!
//! ```compile_fail
//! use thalamic_relay::cpu::init_telemetry;
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![doc(test(attr(deny(unused))))]

mod cpu;
mod daemon;
mod gpu;

pub mod publish;
pub mod safety;
pub mod telemetry;
pub mod telemetry_csv;
pub mod time;

/// Process entry for the `thalamic-relay` executable. Not reusable library API.
#[doc(hidden)]
pub use daemon::run;
