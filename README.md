# EmuTrim

Windows-first, zero-dependency Rust tooling for a running Android Emulator. It is an independent project inspired by ideas in [avdslim](https://github.com/kdbhalala/avdslim), not an official build or fork.

## What v0.2 supports

- direct ADB smart-socket connections to `127.0.0.1:5037`; runtime ADB operations never spawn `adb.exe`
- event-driven `host:track-devices` watching, cached metadata, reconnect handling, and no steady-state polling
- native `slim`, `restore`/`off`, and real dry-runs
- Windows AVD discovery, conservative `tune-avd`, and direct `emulator.exe` launch via `start`

```powershell
cargo build --release
.\target\release\emutrim.exe tune-avd Pixel_API_35 --ram=1536
.\target\release\emutrim.exe start Pixel_API_35 --ram=1536
.\target\release\emutrim.exe watch

# Inspect only: no setting, package, state file, or memory command is changed.
.\target\release\emutrim.exe slim --dry-run
.\target\release\emutrim.exe slim emulator-5554 --keep=com.google.android.apps.maps --skip=location
.\target\release\emutrim.exe restore emulator-5554
```

Windows intentionally has no Android Studio shim: Android Studio launches `emulator.exe` directly, so EmuTrim neither replaces nor renames it. `tune-avd`, `start`, and `watch` are the supported workflow.

## Safety model

`slim` and the watcher require both an `emulator-*` transport and `ro.kernel.qemu=1`; physical devices are observed by the watcher but never mutated. They also require `sys.boot_completed=1`.

The native standard profile excludes the Android 16+ boot-critical `com.google.android.bluetooth`. Before any package or setting change, EmuTrim writes and read-backs `/data/local/tmp/emutrim_state.v1` with the intended disabled packages and original setting values. If that fails, it makes no guest changes. `restore` changes only packages in that record and restores recorded settings; partial package failures retain the record for retry. Existing state is not an interoperability promise with other tools.

`--dry-run` performs package discovery and prints only the planned package disables. It does not write guest state or invoke mutating shell commands.

## v0.3 failure-path checks

`cargo test` exercises the production smart-socket client against a scripted loopback ADB server; no `adb.exe`, emulator, or device is required. Slimming stages state, reads it back, installs it, and verifies the saved record before the first guest mutation. Restore checkpoints completed reversals, keeping only unresolved work for retry. Malformed, truncated, or over-16-MiB shell-v2 output is rejected.

## AVD configuration and 16 KB images

`tune-avd` locates the SDK from `ANDROID_SDK_ROOT`, `ANDROID_HOME`, or `%LOCALAPPDATA%\Android\Sdk`; it locates AVDs from `ANDROID_AVD_HOME` or `%USERPROFILE%\.android\avd`. It preserves `config.ini.emutrim.bak`, changes only RAM and host-GPU keys, and leaves audio/camera keys untouched because their safe defaults are image/workload-specific.

16 KB (`ps16k`) images enforce a 4096 MB minimum. EmuTrim refuses an incompatible `--ram=1536` for `tune-avd` or `start`; it does not pretend the Android Emulator accepted it. `start` uses `-gpu host` and, for compatible non-16-KB images, `-lowram -memory <MB>`.

## Watcher footprint

Windows release watcher, 60 s idle:

```text
CPU delta:    0.0000 s
Working set:  8.08 MB
Private RAM:  3.28 MB
Threads:      2
Handles:      81
```

Windows WMI/CIM permissions blocked independent process-start tracing, so this is not evidence of a measured zero `adb.exe` process count. The implementation uses only direct TCP smart-socket requests for runtime ADB work; `start` intentionally creates only `emulator.exe`.

## Manual emulator check

```powershell
.\target\release\emutrim.exe slim emulator-5554 --dry-run
.\target\release\emutrim.exe slim emulator-5554
.\target\release\emutrim.exe restore emulator-5554
```

Use a disposable emulator first. Confirm the first command prints packages without guest changes, the second creates the EmuTrim state file and disables only listed installed targets, and restore re-enables only recorded packages and restores the exact saved settings.
