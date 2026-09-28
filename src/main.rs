//! `thalamic-relay` supervisor binary.
//!
//! Process plumbing (lockfile, Prometheus, NVML, the async loop) lives in the
//! library crate as private modules. The reusable API is `thalamic_relay::{telemetry, safety, publish}`.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--gpu-hardware-smoke")
    {
        return thalamic_relay::run_gpu_hardware_smoke();
    }

    thalamic_relay::run().await
}
