# EmuTrim

Windows-first Rust tooling to reduce background work in Android Emulator images. Independent from [avdslim](https://github.com/kdbhalala/avdslim).

## Quick start

```powershell
emutrim doctor
emutrim start My_AVD
```

`start` validates the installed AVD and its RAM, launches `emulator.exe`, binds to the console/ADB port assigned for that launch, waits for that exact transport and Android boot, then slims it. If the AVD already has a verified applied profile, it makes no duplicate guest changes. Use `--no-slim` for launch-only behavior or `--ram=N` to override configured RAM.

```powershell
emutrim doctor My_AVD [--serial=emulator-5556]
emutrim stats emulator-5556 [--seconds=10]
emutrim slim emulator-5556 --dry-run
emutrim restore emulator-5556
emutrim watch --serial=emulator-5556
emutrim list-avds
```

`doctor` is read-only. It checks SDK/emulator availability, ADB reachability, installed AVDs, selected image/config/RAM, and an optional running target and saved state. It exits nonzero on material failures; warnings alone succeed. `stats` reports the Windows emulator process working set, private memory, CPU time/delta, threads, and handles when available. Sampling is read-only.

If startup times out, EmuTrim reports the transport/boot phase and leaves the guest unchanged. It does not restart the emulator, wipe data, or delete snapshots. An offline or missing transport with a live emulator process is below the guest mutation path; inspect emulator logs or try Android Studio Cold Boot.

## Safety and implementation

- Runtime ADB uses direct smart-socket TCP to `127.0.0.1:5037`, including shell-v2 where command status matters. EmuTrim does not spawn `adb.exe` or use `avdslim` at runtime.
- Slim and restore require an `emulator-*` serial, positive `ro.kernel.qemu=1` identity, and `sys.boot_completed=1`. Physical and unresolved targets are never mutated.
- Before mutation, EmuTrim persists and reads back the intended reversible state. Restore uses recorded originals only, checkpoints completed reversals, and is safe to retry. Missing state is a no-op; malformed/unreadable state fails closed.
- The native standard profile protects boot-critical packages. A new plan refuses planned packages already disabled outside EmuTrim's state.
- AVD RAM validation enforces 4096 MB minimum for detected 16 KB images and refuses values outside 1536–8192 MB. `tune-avd` backs up `config.ini` before changing RAM/GPU keys.
- `start` is Windows-only when slimming, because it verifies that the selected console port belongs to the process tree it launched before guest mutation. Port selection is bounded to Android Emulator's supported console range. Other online emulators do not redirect it.
- Watch is event-driven (`host:track-devices`) with bounded boot checks; no steady-state polling or async runtime.

## Live-tested images

These are specific disposable AVDs, not broad Android-version support claims.

| Image | Page size | RAM | Slim | Restore | Reconnect |
| --- | ---: | ---: | ---: | ---: | ---: |
| Android 17 / API 37.2 Google APIs x86_64 | 16 KB | 4096 MB | 49 packages | exact package/settings restore; repeat no-op | live tested |
| Android 12 / API 31 Android TV x86 | 4 KB | 1536 MB | 5 packages | exact package/settings restore; repeat no-op | not tested |

The Android 12 image has TV-specific packages and lacks `com.google.android.bluetooth`; package counts differ by image.

## Tests and local build

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Tests use a scripted local ADB smart-socket server and temporary configs; they do not require a real emulator or modify user AVDs. Real mutation checks use only disposable AVDs.
