//! Thalamic Relay — sensory + hardware-safety library.
//!
//! Typed telemetry (validity, freshness, provenance, normalization) lives in
//! [`telemetry`]. GPU acquisition and safety evaluation live in [`gpu`].

pub mod cpu;
pub mod gpu;
pub mod telemetry;
