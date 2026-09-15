//! Thalamic Relay — sensory + hardware-safety library.
//!
//! Typed telemetry (validity, freshness, provenance, normalization) lives in
//! [`telemetry`]. Pure safety policy — instantaneous classification, brake
//! hysteresis, and the [`safety::SafetyActuator`] boundary — lives in
//! [`safety`]. GPU telemetry acquisition and the privileged NVML/`nvidia-smi`
//! actuation backend live in [`gpu`].

pub mod cpu;
pub mod gpu;
pub mod safety;
pub mod telemetry;
