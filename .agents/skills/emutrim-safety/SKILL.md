---
name: emutrim-safety
description: Use for guest mutation, package profiles, state format, restore, or guest-side tweaks.
---

# EmuTrim mutation safety

- Main code: `src/slim/mod.rs` plans/applies/restores changes; `profile.rs` defines package/settings targets; `state.rs` encodes recorded originals.
- Require positive emulator identity (`emulator-*` transport and `ro.kernel.qemu=1`) and `sys.boot_completed=1` before mutation. Refuse physical or unresolved targets.
- Dry-run may inspect and print a plan only. Build the complete package plan before any mutation.
- Before mutation, capture original settings and intended disabled packages, stage state, read it back, install it, and verify canonical state. Unknown/unreadable existing state fails closed.
- Before saving a new plan, refuse any planned package already disabled outside the existing EmuTrim record; state format does not preserve pre-existing disabled status, so enabling such a package during recovery would be unsafe.
- Slim stops on the first failed or ambiguous package disable; pre-recorded plan keeps already-applied and not-yet-attempted packages recoverable.
- Restore only values/packages recorded by EmuTrim. Never guess defaults. Checkpoint each successful reversal; retain failed items for retry. Repeated restore must be safe.
- A missing state record is a valid already-restored condition: `restore` succeeds as a no-op. Read/protocol failures and malformed or unknown existing state remain errors.
- Keep boot-critical packages protected. Treat unrecognized/new Android versions conservatively; do not infer safety from upstream profile alone.
- Check shell-v2 exit status for every mutation. Unknown or partial failures fail closed and retain enough state for recovery.
- Unit tests use fakes/parsers. Intentional guest mutation integration tests use disposable emulators; never physical devices or user AVD config.
