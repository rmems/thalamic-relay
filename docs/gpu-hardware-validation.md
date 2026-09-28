# Trusted GPU hardware validation

The normal GitHub-hosted CI remains the merge and release qualification path.
It runs without an NVIDIA driver or GPU. This optional workflow adds evidence
from a physical NVIDIA device for a trusted commit; it is not a required PR
check and does not replace the hosted jobs.

## Read-only smoke

The `Trusted GPU Hardware Smoke` workflow is started manually from the GitHub
Actions page using the `main` branch. Its job has an explicit `main` ref guard
and runs only on a runner carrying all of these labels:

```text
self-hosted, linux, x64, gpu, nvidia
```

The runner needs the NVIDIA driver, `nvidia-smi`, a working NVML library, and GNU coreutils (`timeout`). The production NVIDIA adapter also uses `timeout` to bound health and actuation commands. Keep the runner dedicated to this repository and offline from untrusted workloads. Public pull requests do not trigger this workflow. GitHub
allows manual dispatch only to users with write access; protect `main` and
limit that access to maintainers trusted to run code on the hardware host.
The workflow has only `contents: read` permission and checks out without
persistent credentials.

The job runs `timeout -k 2s 3s nvidia-smi -L`, then executes the supervisor's explicit
`--gpu-hardware-smoke` mode. It uses the same NVML adapter as the supervisor
to:

1. Resolve NVML index 0 to a validated physical GPU UUID and round-trip it
   through NVML.
2. Read live temperature and board-power telemetry through that UUID.
3. Read current and default power limits through that UUID.
4. Print the UUID, readings, and read-only validation scope in the job log.

Missing driver/device access, invalid telemetry, an unresolved or
non-round-tripping UUID, or unreadable power limits fail the job explicitly.
This phase does not invoke power-limit actuation, `sudo`, or any `nvidia-smi`
write operation. No passwordless sudo rule is needed.

For an equivalent local run on a trusted Linux host with the repository's Rust
toolchain installed:

```sh
nvidia-smi -L
cargo run --locked --bin thalamic-relay -- --gpu-hardware-smoke
```

## Actuation validation

This issue adds no actuation lane. Any future power-limit test must remain a
separate deliberate maintainer action, require explicit UUID confirmation, and
restore and verify the original state even after a failed assertion. It must
never use an unscoped `nvidia-smi -pl` command.
