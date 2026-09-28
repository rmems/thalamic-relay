//! `thalamic-relay` supervisor binary.
//!
//! Process plumbing (lockfile, Prometheus, NVML, the async loop) lives in the
//! library crate as private modules. The reusable API is `thalamic_relay::{telemetry, safety, publish}`.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if thalamic_relay::run_gpu_hardware_smoke_if_requested()? {
        return Ok(());
    }

    thalamic_relay::run().await
}
