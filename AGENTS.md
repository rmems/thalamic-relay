## Cursor Cloud specific instructions

This repo is the `thalamic-relay` crate (binary `thalamic-relay`), a Rust CLI that
observes hardware telemetry and provides deterministic hardware safety for the
Spikenaut runtime stack. It does not run any neural computation itself (see
[RM-1143 / GH#39](https://github.com/rmems/thalamic-relay/issues/39)) —
that lives in `brainstem-daemon`.
Below are the non-obvious gotchas.

### Dependencies

This crate no longer has any sibling `../` path dependencies. The former Core
Backend Crates (`silicon-bridge`, `plasticity-lab`, `metabolic-ledger`,
`limbic-critic`) were removed for modularity, so there is no need to clone
sibling repos next to this one. It also no longer depends on `neuromod`: the
in-process SNN it used to step was removed in RM-1143. Build with a plain
`cargo build` from the repo root.

### Toolchain / system deps

- Requires Rust edition 2024 with MSRV 1.98.1 (toolchain >= 1.98.1; `u64::is_multiple_of` and
  clippy lints are used in CI; stable is set as the rustup default in this
  environment). `cargo`/`cargo build`/`cargo test`/`cargo clippy` all work from
  `/workspace`.
- `pkg-config` may be used by native dependencies. `libudev-dev` is no longer
  required: the serial backend (`serialport`, via `silicon-bridge`) was removed,
  and `nvml-wrapper` (NVML — NVIDIA Management Library) loads `libnvidia-ml.so` at runtime without linking libudev.

### Running the app

- Run with `cargo run --bin thalamic-relay [OPTIONS]` (or the installed binary).
  It is a long-running supervisor. Use `--help` for options (or the equivalent
  THALAMIC_* environment variables). The supervisor now supports graceful arg
  parsing via clap (derive + env features; implemented for #11). Run it in tmux /
  background when testing (unless the user explicitly requests foreground behavior).
- The relay degrades gracefully with no GPU/FPGA: it prints `nvidia-smi hung` /
  runs in "software-only mode" and keeps monitoring telemetry and hardware safety.
- Single-instance guard: writes `/tmp/thalamic_relay.lock` with its PID. A stale
  lock for a dead PID is ignored automatically, but a second concurrent instance
  exits immediately. Delete the lockfile only if no instance is actually running.

### Interfaces (used for end-to-end testing)

- No control/query IPC surface currently — the prior UDP protocol
  (`Stimuli`/`LearningReward`/`GetNeuroState`) was removed in RM-1143 along
  with the in-process SNN it existed to drive. See [`docs/ipc.md`](docs/ipc.md)
  for the removal note and the planned `corpus-ipc`-based replacement.
- Prometheus metrics on `http://localhost:9000/metrics` (bind IP (Internet Protocol) configurable via --metrics-ip).
- Binds on startup, so only one instance can run at a time.

### Responding to automated PR review bots

This repo runs several automated reviewers (CodeRabbit, Codex, Amazon Q,
cubic, CodeAnt). Verify each finding against the actual source — including
pinned dependency source (e.g. `neuromod`, via `cargo fetch` into a scratch
crate if not already cached) when the finding concerns a dependency's
behavior — before fixing it. Don't fix mechanically: skip or push back (with
a short reply explaining why) on anything already handled, restated, or
wrong, and don't let a "fix" overstate a guarantee the code doesn't actually
provide. See `CLAUDE.md` for more detail and the concrete lessons from PR
#38 (`docs/ipc.md`).

## Packaging

`Cargo.toml` uses an `include` allowlist for crates.io. Do not add
contributor-only files (`AGENTS.md`, `CLAUDE.md`, `REVIEW.md`, CI, local
tool configs) to `include`. After changing packaged paths, run
`cargo package --list` and `cargo publish --dry-run --locked`. Never run the
real `cargo publish` without explicit maintainer approval (#44 / #48).

## Boundaries

- **Owns**: `src/`, `Cargo.toml`, `README.md`, `AGENTS.md`, `docs/`
- **Does Not Own**: sibling crates, `neuromod` upstream
- **Off-limits**: do not edit sibling path-dependency crates, do not introduce mining/trading domain logic
