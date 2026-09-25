---
name: emutrim-safety
description: Use for guest mutation, package profiles, state format, restore, or guest-side tweaks.
---

# EmuTrim mutation safety

- Main code: `src/slim/mod.rs` plans/applies/restores changes; `profile.rs` defines package/settings targets; `state.rs` encodes recorded originals.
- Require positive emulator identity (`emulator-*` transport and `ro.kernel.qemu=1`) and `sys.boot_completed=1` before mutation. Refuse physical or unresolved targets.
- Dry-run may inspect and print a plan only. Build the complete package plan before any mutation.
- Before mutation, capture original settings and intended disabled packages, persist state, then read it back and compare. On failure, make no guest changes.
- Restore only values/packages recorded by EmuTrim. Never guess defaults. Preserve state for retry after partial package restore; make repeated restore safe.
- Keep boot-critical packages protected. Treat unrecognized/new Android versions conservatively; do not infer safety from upstream profile alone.
- Check shell-v2 exit status for every mutation. Unknown or partial failures fail closed and retain enough state for recovery.
- Unit tests use fakes/parsers. Intentional guest mutation integration tests use disposable emulators; never physical devices or user AVD config.
