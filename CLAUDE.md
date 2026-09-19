# CLAUDE.md

Guidance for Claude Code sessions working in this repo. See `AGENTS.md` for the
canonical environment/toolchain notes (Cursor Cloud-oriented but applicable
here too) — this file adds Claude-specific workflow notes and defers to
`AGENTS.md` for anything that overlaps.

## Project

`thalamic-relay` — a Rust CLI (binary `thalamic-relay`) that observes hardware
telemetry and provides deterministic hardware safety (thermal/power emergency
brake) for the Spikenaut runtime stack, exposing Prometheus metrics.
No FPGA/silicon-bridge dependency. It does **not** run any
neural computation itself — the in-process SNN it used to step was removed
in [RM-1143 / GH#39](https://github.com/rmems/thalamic-relay/issues/39);
neural execution lives in `brainstem-daemon`.

## Build / test

```bash
cargo build
cargo test --all-features
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

**Edition** is Rust 2024 (rustc/Cargo ≥ 1.85 to parse the manifest). **MSRV** is
1.98.1 (`package.rust-version`; authoritative). CI installs exactly that
toolchain; `rust-toolchain.toml` must match. No sibling path dependencies — a
plain `cargo build` from the repo root is sufficient. CI also qualifies
packaging (`cargo package --locked`, packaged-source tests, token-free
`cargo publish --dry-run --locked`) and rustdoc with warnings denied.

## Interfaces

- Sensory publish: fire-and-forget UDP `IpcMessage::Stimuli` JSON to
  `127.0.0.1:9900` by default (`--ipc-endpoint`). This is not a control/query
  surface — the retired UDP protocol (`Stimuli`/`LearningReward`/`GetNeuroState`
  at `127.0.0.1:9898`) was removed in RM-1143. See [`docs/ipc.md`](docs/ipc.md).
- Prometheus metrics on `:9000/metrics`.
- Frozen hardware-telemetry CSV interchange:
  `timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w`
  (`src/telemetry_csv.rs`, [`docs/telemetry_csv.md`](docs/telemetry_csv.md)).
  Do not change that schema.

## Reviewing / responding to automated PR review bots

This repo has several review bots active on PRs (CodeRabbit, Codex/ChatGPT
connector, Amazon Q, cubic, CodeAnt). Experience from PR #38 (`docs/ipc.md`):

- **Verify every finding against the actual source before fixing it** —
  including third-party dependency source (`~/.cargo/registry/src/...` or
  `cargo fetch` into a scratch crate) when a finding is about a pinned
  dependency's behavior, not just this repo's code. Several of Codex's
  findings on that PR were only checkable by reading `neuromod`'s source
  directly; they were correct.
- **Don't blindly fix every comment.** Treat each one as a claim to check,
  not an instruction to follow. Skip (with a brief reply explaining why)
  anything that's already handled, restates a fixed issue, or is wrong.
  Push back or ask the user when a "fix" would overstate a guarantee the
  code doesn't actually make (e.g. don't claim a timing workaround is
  reliable when it isn't — see the pulse/reset timing note in
  `docs/ipc.md`).
- **CodeRabbit rate-limits to ~1 included review per hour** on this plan.
  A stale `CHANGES_REQUESTED` review from before a fix can block
  `mergeable_state` even after the fix lands; either wait for the limit to
  reset or trigger a fresh pass with `@coderabbitai review` once it has.
  Its automatic re-review doesn't always fire — the manual trigger is
  sometimes required.
- **If a bot's findings stop converging** (new finding every round with no
  end in sight), say so once in a PR comment rather than iterating
  indefinitely — that's a signal to hand the remaining judgment calls to a
  human rather than keep chasing diminishing-value nits.

## Boundaries

Same as `AGENTS.md`: owns `src/`, `Cargo.toml`, `README.md`, `AGENTS.md`,
`docs/`. Does not own sibling crates or `neuromod` upstream. Off-limits:
sibling path-dependency crates, mining/trading domain logic.
