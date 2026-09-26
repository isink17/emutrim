# EmuTrim

## Identity

- EmuTrim is an independent, Windows-first Rust Android Emulator optimizer.
- `kdbhalala/avdslim` is a functional reference only. Keep EmuTrim identity and branding distinct; follow upstream trademark policy.

## Work method

- Use CodeGraph first for navigation; refresh its index when stale and verify findings in source.
- Inspect callers and nearby tests before editing. Make the smallest coherent safe patch; preserve current architecture unless evidence requires change.
- Use RTK-backed commands when the active Windows RTK skill requires them. Keep terminal output terse.
- Run targeted checks while iterating. For meaningful code changes, run applicable full checks before closeout: `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --release`.
- Do not add dependencies, async runtimes, or frameworks without demonstrated need.

## Architecture and safety invariants

- Runtime ADB uses direct smart-socket TCP at `127.0.0.1:5037`, `host:track-devices`, and shell-v2. No runtime `adb.exe` fallback or `avdslim` dependency without explicit documented justification.
- Preserve blocking/event-driven operation; no steady-state busy polling or Tokio/async rewrite without measured need.
- `emulator.exe` launch is valid. Do not replace or shim Android Studio executables on Windows.
- Never mutate physical devices or unresolved targets. Establish emulator identity positively and require guest boot completion before mutation.
- Persist reversible state and read it back before guest mutation. Restore recorded originals only; make restore retryable/idempotent. Fail closed on unknown or partial state; never guess defaults or ignore shell-v2 nonzero status.
- Detect 16 KB images and report RAM constraints honestly; never claim unsupported values were applied.
- Tests must not change real user AVD configs or physical devices. Use temporary configs and disposable AVDs for intentional mutation tests; prefer dry-run first.

Safety order: physical target refusal; resolved emulator identity; boot complete; reversible state persisted/read back; then mutation.

## Git

- Canonical branch: `master`. Do not create or migrate work to `main`.
- Inspect status before and after. Preserve unrelated user changes.
- Do not commit, push, or add attribution trailers unless explicitly requested.

## Repo skills

- `emutrim-core`: general architecture and engineering workflow.
- `emutrim-adb`: ADB protocol, tracking, transport, and shell execution changes.
- `emutrim-safety`: guest mutation, profiles, state, and restore changes.
- `emutrim-windows-avd`: Windows SDK/AVD discovery, config, launch, and 16 KB behavior.
- `emutrim-macos-avd`: Apple Silicon SDK/AVD paths and fail-closed emulator launch identity.
