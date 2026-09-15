//! Thalamic Relay — sensory + hardware-safety library.
//!
//! Typed telemetry (validity, freshness, provenance, normalization) lives in
//! [`telemetry`]. Pure safety evaluation lives in [`safety`]. GPU acquisition
//! and privileged actuation live in [`gpu`]. Sensory publication is a
//! non-blocking stub in [`publish`] (transport is GH#40). Fail-closed
//! SIGINT/SIGTERM and leftover-brake restart policy live in [`shutdown`].

pub mod cpu;
pub mod gpu;
pub mod publish;
pub mod safety;
pub mod shutdown;
pub mod telemetry;
