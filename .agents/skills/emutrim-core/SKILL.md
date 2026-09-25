---
name: emutrim-core
description: Use for general EmuTrim engineering or changes crossing module boundaries.
---

# EmuTrim core

- CLI entrypoint is `src/main.rs`; runtime ADB is `src/adb/`; guest slimming and reversible state are `src/slim/`; Windows AVD work is `src/avd/`.
- Navigate with CodeGraph first; verify relevant symbols and callers in source. Refresh stale index before relying on it.
- Preserve direct blocking/event-driven design and zero runtime dependencies. Do not add an async runtime or framework without measured need.
- Make the smallest coherent patch. Check adjacent tests and add focused coverage for changed behavior.
- For meaningful code changes, run applicable `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo build --release` before closeout. Use targeted checks while iterating.
- Closeout: inspect diff/status; confirm no unrelated or runtime changes slipped in; report checks and any unverified behavior.
