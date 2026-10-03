# Windows product-proof benchmark

Measures the reversible standard profile against `--no-slim` on the same EmuTrim-managed AVD. Both conditions use `start --managed --headless --cold-boot --timings`; Slim omits only `--no-slim`. This is not a comparison between Emulator builds.

The fixture is Android's `testing-samples/ui/espresso/BasicSample`, Apache-2.0, pinned in `fixture.lock.json`. Clone it outside this repository at `%TEMP%\emutrim-benchmark-080\testing-samples`; do not edit it. The benchmark build SDK and EmuTrim managed runtime SDK are separate roots.

## Run

Provision `platforms;android-34` and `build-tools;34.0.0` into `%TEMP%\emutrim-benchmark-080\build-sdk` with the standalone Android CLI. Do not use the Android Studio SDK. Build and offline-preflight the fixture before running this harness. Create the runtime with `EMUTRIM_HOME=%USERPROFILE%\.emutrim-benchmark-080` and `emutrim managed setup`.

Pass `-RuntimeRoot` for that managed runtime. The standalone CLI defaults to `%LOCALAPPDATA%\AndroidCLI\android.exe`; use `-AndroidCli` for another location. Set `JAVA_HOME` or pass `-JavaHome`. The external Android Studio SDK inventory defaults to `%LOCALAPPDATA%\Android\Sdk`; use `-ExternalSdkRoot` if it lives elsewhere. The separate WHPX endurance script requires `-BenchmarkRoot` and `-EmuTrimExe`; its Emulator is resolved only from the selected benchmark root and must report version 37.1.11.

```powershell
pwsh -NoProfile -File tools/benchmark/windows-product-proof.ps1 -Mode SelfTest
pwsh -NoProfile -File tools/benchmark/windows-product-proof.ps1 -Mode DryRun
pwsh -NoProfile -File tools/benchmark/windows-product-proof.ps1 -Mode Official
```

Dry run executes one control and one Slim cycle with the full 120-second stabilization and five 10-second idle samples per trial. Official mode performs one control and one Slim warm-up, then five paired cycles in fixed AB/BA/AB/BA/AB order with the same idle protocol. Every trial starts after successful `emutrim reset EmuTrim_Managed --yes`; reset is outside measured time. Failed actions remain in raw evidence; no retry or outlier removal.

The harness uses managed `adb.exe` for exact-serial guest checks and PowerShell process APIs after matching the console-owner PID reported by `emutrim stats`. Gradle child processes receive the benchmark build SDK roots and `ANDROID_SERIAL`; timed Gradle tasks use `--offline`. The ADB server is stopped only if this harness started it and only benchmark emulator transports remain.

Official files go to `benchmarks/windows-product-proof/`. No Emulator system image is downloaded by the build SDK provisioning step. If Gradle proves build-SDK `platform-tools` is required, install it there before official mode and rerun the dry run.

`raw.csv` columns are `pair, condition, order, boot_complete_ms, start_total_ms, idle_cpu_median, idle_working_set_median, idle_private_median, install_debug_ms, first_launch_this_time_ms, first_launch_total_time_ms, first_launch_wait_time_ms, connected_test_wall_ms, connected_test_suite_ms, success, notes, run_status`. `boot_complete_ms` comes from `trial.start.boot_complete_ms`, summed from launch-to-console, console-to-ADB, ADB-to-device, and device-to-boot phases. Metrics whose phase did not run are empty CSV fields; trial shape mismatches fail export. Partial runs use `run_status=aborted` and remain diagnostic, not publication data. To export a preserved partial JSON without modifying it, use `-Mode ExportPartial -InputRawPath <raw.json> -CsvPath <diagnostic.csv>`.
