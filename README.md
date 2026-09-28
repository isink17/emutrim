# EmuTrim

EmuTrim starts Android emulators, waits for the exact emulator to finish booting, diagnoses common SDK and AVD problems, and can apply a reversible slimming profile to development AVDs. Use it for one-command startup, readiness checks, safe restore, and isolated disposable Android environments.

## Why EmuTrim?

- Start an AVD and wait for its Android boot to complete with one command.
- Diagnose SDK, AVD, ADB, image, and RAM configuration with `doctor`.
- Apply a reversible profile to a development emulator, then restore its recorded state exactly.
- Keep disposable EmuTrim test assets isolated from Android Studio installations.

EmuTrim does not promise RAM savings or general application-performance improvements.

## Install

### Release binaries

Download the current release for Windows x86_64 or Apple Silicon macOS arm64 from [GitHub Releases](https://github.com/isink17/emutrim/releases). Extract the archive and run `emutrim.exe` on Windows or `./emutrim` on macOS. The macOS binary is unsigned; Gatekeeper may block its first launch. Review the downloaded binary and use macOS's per-app approval flow if you choose to run it. Do not disable Gatekeeper globally.

### Cargo from source

Install the latest source from the repository:

```sh
cargo install --git https://github.com/isink17/emutrim emutrim
```

For a reproducible version, install the release tag:

```sh
cargo install --git https://github.com/isink17/emutrim --tag v0.5.0 emutrim
```

EmuTrim is not published on crates.io.

## Quick start

An ADB server must already be running at `127.0.0.1:5037`. Android Studio often starts it automatically; otherwise run `<ANDROID_SDK>/platform-tools/adb start-server`.

```sh
emutrim doctor
emutrim list-avds
emutrim start My_AVD
```

`start` slims by default after the exact emulator finishes booting. For a completely non-mutating first boot, use `emutrim start My_AVD --no-slim`.

## What slimming changes

Slimming disables selected installed packages; it does not delete system packages. It also applies these settings:

| Group | Change |
| --- | --- |
| `animations` | Set window, transition, and animator animation scales to zero. |
| `bglimit` | Set background process limit and maximum cached processes to 4. |
| `sync` | Disable global automatic sync. |
| `location` | Disable location mode. |
| `setup` | Mark user setup and device provisioning complete. |
| `bluetooth` | Disable Bluetooth and, when present, the non-critical MIDI Bluetooth package. The protected boot-critical Google Bluetooth package is preserved. |

The standard package profile covers Google apps, media, communication, accessibility extras, printing, wallpapers and dreams, and other emulator/user-facing packages. Only applicable installed packages are disabled; not every image contains every package. Previously disabled packages are protected, and restore re-enables only packages recorded as enabled before EmuTrim changed them.

Customize `slim` with repeatable options:

- `--skip=<group>` skips a settings/profile group. Groups: `animations`, `bglimit`, `sync`, `location`, `setup`, `bluetooth`. Combine groups with commas, for example `--skip=animations,location`; repeat the option if preferred.
- `--keep=PACKAGE` prevents a package from being disabled. Repeat for multiple packages.
- `--dry-run` prints planned package disables without changing the guest.

## Commands

| Command | Behavior and important options |
| --- | --- |
| `doctor [AVD] [--serial=SERIAL]` | Diagnose SDK, emulator, ADB, installed AVDs, image/config/RAM, optional target, and saved state. `--managed` selects managed assets. |
| `list-avds` | List installed AVDs; add `--managed` for the managed environment. |
| `start <AVD>` | Launch, wait for exact ADB transport and Android boot, then slim. Options: `--managed`, `--cold-boot` (bypass Quick Boot for this launch), `--no-slim`, `--timings`, `--ram=N`. |
| `slim [SERIAL]` | Apply the reversible profile. Supports `--dry-run`, `--keep=PACKAGE`, and `--skip=GROUP`. |
| `off [SERIAL]` | Alias for `restore`; restore recorded original state. |
| `restore [SERIAL]` | Restore recorded package and setting state; retry incomplete restores safely. |
| `watch [--serial=SERIAL]` | Watch ADB device events and slim eligible emulator connections; `--dry-run` reports without mutation. |
| `stats <SERIAL> [--seconds=N]` | Read Windows process working set, private memory, CPU, threads, and handles. Unsupported on macOS. |
| `tune-avd [AVD] [--managed] [--ram=N]` | Update AVD RAM/GPU settings after backing up `config.ini`; without `--ram`, selects 4096 MB for detected 16 KB images and 1536 MB for other configured images. Missing image paths fail safely. Omitting AVD works only when exactly one is installed in selected environment. |
| `managed root` | Print EmuTrim data root. |
| `managed status` | Report isolated managed SDK/AVD setup. |
| `managed setup` | Set up managed Android assets; currently supported on Apple Silicon macOS. |
| `managed clean [--yes]` | Preview managed cleanup; `--yes` deletes managed payloads. |

`off` and `restore` accept an optional serial; without one, EmuTrim selects the sole running emulator and refuses ambiguity. `--ram=N` is in MB. `--cold-boot` does not wipe data or delete snapshots. `--timings` reports startup phases.

## Managed Android environment

By default, EmuTrim stores its root at `~/.emutrim`; managed SDK and AVD payloads live under `~/.emutrim/managed`. Set `EMUTRIM_HOME` to use another EmuTrim-owned root. The managed environment isolates disposable SDK, system image, and AVD assets from Android Studio's SDK and external AVDs, making cleanup straightforward.

```sh
emutrim managed setup
emutrim managed status
emutrim managed clean
emutrim managed clean --yes
```

`managed clean --yes` deletes only EmuTrim-owned managed payloads. It does not delete the user's Android Studio SDK or external AVDs. Cleanup refuses to run while a managed emulator is running.

## Safety model

When EmuTrim cannot prove that a target or saved state is safe, it refuses to mutate it rather than guessing. This is its fail-closed behavior.

- Physical devices and unresolved targets are never mutated. Guest changes require an `emulator-*` serial, positive emulator identity, and completed Android boot.
- EmuTrim targets the exact serial and, for integrated `start`, verifies the selected console port belongs to the launched process. Port selection stays within the supported console range.
- Reversible state is saved and read back before mutation. Restore uses recorded originals only, checkpoints successful reversals, and can be retried. Missing state is a no-op; malformed/unreadable state is an error.
- Packages disabled before EmuTrim are protected from overwrite. Boot-critical Google Bluetooth is protected.
- RAM validation enforces 4096 MB minimum for detected 16 KB images and rejects values outside 1536–8192 MB. `tune-avd` preserves a `config.ini` backup.
- Runtime ADB connects directly to `127.0.0.1:5037` using the smart socket; guest operations do not spawn `adb`. EmuTrim has no runtime avdslim dependency.
- `watch` uses event-driven ADB tracking with bounded boot checks.

## Supported platforms

- **Windows x86_64:** supported and live qualified.
- **macOS Apple Silicon (arm64):** supported and live qualified; `stats` is unavailable.
- **Linux:** CI/core code only; runtime support is not claimed.
- Intel Mac runtime support is not claimed.

Live tests used specific disposable images, not broad Android-version qualification:

| Image | Page size | RAM | Slim/restore |
| --- | ---: | ---: | --- |
| Android 17 / API 37.2 Google APIs x86_64 | 16 KB | 4096 MB | 49 packages; exact package/settings restore; repeat no-op |
| Android 12 / API 31 Android TV x86 | 4 KB | 1536 MB | 5 packages; exact package/settings restore; repeat no-op |

Package counts vary by image and architecture. Android 12 TV image lacks `com.google.android.bluetooth`.
Apple Silicon macOS managed AVD setup and isolation have also been live-qualified; no package-count comparison is claimed.

## Benchmarks

The measured Windows configuration did not show RAM savings. It showed a modest lower idle CPU signal in the tested setup. These results are not a general performance claim. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, full results, and qualifications. No macOS benchmark is claimed.

## Build and test

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Tests use a scripted local ADB smart-socket server and temporary configs; they do not require a real emulator or modify user AVDs. Intentional real mutation checks use disposable AVDs.

## License

EmuTrim v0.5.0 and later is source-available under the MIT License with Commons Clause License Condition v1.0. You may use, modify, and redistribute it subject to those terms, but you may not sell EmuTrim itself or a product/service whose value derives entirely or substantially from EmuTrim. See [LICENSE](LICENSE) for the full terms.

EmuTrim v0.4.0 and earlier remain under the MIT License under which they were released. Those prior grants are unchanged.

## Third-party notices

Portions of the standard slimming profile package selection were derived/adapted from `kdbhalala/avdslim`. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution and license text.
