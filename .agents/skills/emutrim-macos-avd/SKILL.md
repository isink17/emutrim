---
name: emutrim-macos-avd
description: Use for Apple Silicon macOS Android Emulator discovery and verified launch identity.
---

# EmuTrim macOS AVD

- Apple Silicon (`aarch64-apple-darwin`) is the only Mac target in this milestone. Do not add Intel Mac support.
- SDK default: `$HOME/Library/Android/sdk`; normal AVD home: `$HOME/.android/avd`.
- Emulator executable: `<sdk>/emulator/emulator`.
- Mutating integrated start requires selected-port listener ownership and process ancestry to the launched process.
- Authenticated console AVD name must match request; expected ADB serial, `ro.kernel.qemu=1`, and boot-complete gates remain mandatory.
- Listener ownership uses exact `/usr/sbin/lsof`; ancestry uses exact `/bin/ps` parent PID lookup. Missing, malformed, or ambiguous results fail closed.
- Process ancestry is bounded; PID reuse across process snapshots is residual risk, so console name, exact ADB serial, qemu identity, and boot gates stay mandatory.
- CI compile/unit tests do not qualify live Emulator runtime. Require physical Apple Silicon acceptance before claiming support.
- Do not weaken safety gates for portability.
