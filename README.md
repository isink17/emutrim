# EmuTrim

EmuTrim is Rust tooling for Android Emulator startup, optimization, and diagnostics on Windows and Apple Silicon macOS.

## Supported platforms

- **Windows x86_64:** supported and live qualified.
- **macOS Apple Silicon (arm64):** supported and live qualified. `stats` is currently unavailable.
- **Linux:** portable/core code is verified by CI; Linux is not a first-class runtime target.

Intel Mac and Linux runtime support are not claimed.

## Quick start

EmuTrim managed Android assets are a development/pre-1.0 feature. On macOS, normal external environments use `$HOME/Library/Android/sdk` and `$HOME/.android/avd`. Managed mode keeps its own SDK and AVD under `~/.emutrim` (payloads under `~/.emutrim/managed`); set `EMUTRIM_HOME` to choose another EmuTrim-owned root. `managed clean --yes` removes only EmuTrim-owned managed payloads, never Android Studio SDKs or external AVDs.

```sh
emutrim doctor
emutrim list-avds
emutrim start My_AVD
```

EmuTrim launches the SDK's emulator directly. It does not replace or shim Android Studio executables. macOS release binaries are unsigned; macOS may require the user to approve opening the downloaded app in Privacy & Security.

`start` validates the installed AVD and its RAM, launches the SDK emulator, binds to the console/ADB port assigned for that launch, waits for that exact transport and Android boot, then slims it. If the AVD already has a verified applied profile, it makes no duplicate guest changes. `--no-slim` waits through boot and returns without guest mutation; `--timings` reports startup phases and composes with `--no-slim`. Use `--ram=N` to override configured RAM. `--cold-boot` bypasses Quick Boot for that launch (`-no-snapshot`); it does not wipe data or delete snapshot files.

```sh
emutrim doctor My_AVD [--serial=emulator-5556]
emutrim start My_AVD --cold-boot
emutrim start My_AVD --no-slim --timings
emutrim stats emulator-5556 [--seconds=10]
emutrim slim emulator-5556 --dry-run
emutrim restore emulator-5556
emutrim watch --serial=emulator-5556
emutrim list-avds
emutrim doctor --managed
emutrim list-avds --managed
emutrim start My_AVD --managed --no-slim --timings
emutrim tune-avd My_AVD --managed
emutrim managed root
emutrim managed status
emutrim managed setup
emutrim managed clean
emutrim managed clean --yes
```

`--managed` selects EmuTrim's isolated SDK/AVD environment for `doctor`, `list-avds`, `start`, and `tune-avd`. `doctor` is read-only. It checks SDK/emulator availability, ADB reachability, installed AVDs, selected image/config/RAM, and an optional running target and saved state. It exits nonzero on material failures; warnings alone succeed. `stats` reports Windows emulator process working set, private memory, CPU time/delta, threads, and handles. It is unsupported on macOS. Sampling is read-only.

If startup times out, EmuTrim reports the transport/boot phase and leaves the guest unchanged. It does not restart the emulator, wipe data, or delete snapshots. If an AVD remains offline during Quick Boot, retry with `emutrim start <AVD> --cold-boot`; this bypasses Quick Boot for one launch without deleting snapshots or wiping data.

## Safety and implementation

- Runtime ADB uses direct smart-socket TCP to `127.0.0.1:5037`, including shell-v2 where command status matters. EmuTrim does not spawn `adb.exe` or use `avdslim` at runtime.
- Slim and restore require an `emulator-*` serial, positive `ro.kernel.qemu=1` identity, and `sys.boot_completed=1`. Physical and unresolved targets are never mutated.
- Before mutation, EmuTrim persists and reads back the intended reversible state. Restore uses recorded originals only, checkpoints completed reversals, and is safe to retry. Missing state is a no-op; malformed/unreadable state fails closed.
- The native standard profile protects boot-critical packages. A new plan refuses planned packages already disabled outside EmuTrim's state.
- AVD RAM validation enforces 4096 MB minimum for detected 16 KB images and refuses values outside 1536–8192 MB. `tune-avd` backs up `config.ini` before changing RAM/GPU keys.
- Integrated `start` verifies that the selected console port belongs to the process it launched before guest mutation. Port selection is bounded to Android Emulator's supported console range. Other online emulators do not redirect it.
- Watch is event-driven (`host:track-devices`) with bounded boot checks; no steady-state polling or async runtime.

## Live-tested images

These are specific disposable AVDs, not broad Android-version support claims.

| Image | Page size | RAM | Slim | Restore | Reconnect |
| --- | ---: | ---: | ---: | ---: | ---: |
| Android 17 / API 37.2 Google APIs x86_64 | 16 KB | 4096 MB | 49 packages | exact package/settings restore; repeat no-op | live tested |
| Android 12 / API 31 Android TV x86 | 4 KB | 1536 MB | 5 packages | exact package/settings restore; repeat no-op | not tested |

The Android 12 image has TV-specific packages and lacks `com.google.android.bluetooth`; package counts differ by image.

The following resource measurements are Windows x86_64 only: API 37 measurements used disposable `EmuTrim_API37_FreshControl_20260926`, Emulator 37.2.7.0 (package metadata references `android-sdk-preview-license`), Platform-Tools 37.0.1, ADB server protocol 41, WHPX, image revision 5, 16 KB, and 4096 MB. In three cold-start resource cycles, stock/slim order was AB, BA, AB; each state stabilized for 120 seconds and had five 10-second samples. Statistics are for the console-owner QEMU process. Working Set is its resident working set; `PrivateUsage` is committed private memory, not physical RAM. Pooled median Working Set was 4772.7 MB stock vs 4788.0 MB slim (+15.3 MB); PrivateUsage was 5630.2 vs 5631.8 MB (+1.6 MB); CPU was 2.266 vs 1.984 seconds per 10-second sample (−0.282s). Median within-cycle CPU delta was −0.328s/10s; Working Set and private-memory results did not show savings. These idle measurements do not establish application performance and are not macOS measurements.

Windows x86_64 cold-start comparison: stable Emulator 37.1.11.0 build 15917651 and preview-license Emulator 37.2.7.0 build 16195039 alternated on the same API 37 AVD and host, three starts per build. Total launch-to-boot times were 26.07, 24.63, 24.78s (stable; median 24.78, range 24.63–26.07) and 22.82, 22.62, 22.56s (preview; median 22.62, range 22.56–22.82). This small, single-host sample does not establish a general version advantage or root cause. EmuTrim `start --timings` reports launch, authenticated console, ADB transport, device, boot-complete, and ready-decision phases only when requested. One API 31 Windows integrated-start acceptance completed in 6.0s; an API 37 Windows integrated start remained offline through its existing 120-second transport bound. No general startup-time claim is made.

## Tests and local build

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Tests use a scripted local ADB smart-socket server and temporary configs; they do not require a real emulator or modify user AVDs. Real mutation checks use only disposable AVDs.
