---
name: emutrim-windows-avd
description: Use for Windows SDK/AVD discovery, config editing, tune-avd/start/list-avds, or 16 KB image constraints.
---

# EmuTrim Windows AVD

- Main code: `src/avd/mod.rs`; CLI routing and arguments are in `src/main.rs`.
- Windows is first-class. Resolve SDK from `ANDROID_SDK_ROOT`, `ANDROID_HOME`, then `%LOCALAPPDATA%\Android\Sdk`; resolve AVDs from `ANDROID_AVD_HOME` then `%USERPROFILE%\.android\avd`.
- Edit only intended `config.ini` keys. Preserve unrelated entries, create `config.ini.emutrim.bak` before first write, and keep repeated tuning idempotent.
- Launch discovered `emulator.exe` directly; this subprocess is expected. Do not replace/shim Android Studio binaries. Do not introduce shell-wrapper assumptions or repeated `adb.exe` spawning.
- Preserve host GPU behavior. Detect 16 KB images and enforce the supported minimum RAM honestly; never pass/report incompatible low RAM as applied.
- Test config parsing and edits with temporary files only. Do not modify real user AVD configs during tests.
