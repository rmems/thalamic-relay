//! Thalamic Relay — sensory + hardware-safety library.
//!
//! Typed telemetry (validity, freshness, provenance, normalization) lives in
//! [`telemetry`]. The frozen hardware-telemetry CSV interchange lives in
//! [`telemetry_csv`]. Pure safety evaluation lives in [`safety`]. GPU
//! acquisition and privileged actuation live in [`gpu`]. Sensory publication
//! is a non-blocking stub in [`publish`] (transport is GH#40).

pub mod cpu;
pub mod gpu;
pub mod publish;
pub mod safety;
pub mod telemetry;
pub mod telemetry_csv;
