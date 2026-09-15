//! Thalamic Relay — sensory + hardware-safety library.
//!
//! Typed telemetry (validity, freshness, provenance, normalization) lives in
//! [`telemetry`]. Pure safety evaluation lives in [`safety`]. GPU acquisition
//! and privileged actuation live in [`gpu`]. Sensory publication lives in
//! [`publish`]: internal [`telemetry::SensoryMapping`] is mapped to canonical
//! `corpus-ipc` [`corpus_ipc::StimulusBatch`] / [`corpus_ipc::IpcMessage`]
//! and sent best-effort, never from inside [`safety::SafetyMachine::evaluate`].

pub mod cpu;
pub mod gpu;
pub mod publish;
pub mod safety;
pub mod telemetry;
