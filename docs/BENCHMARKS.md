# Benchmarks

All resource measurements below are Windows x86_64 only. They describe tested configurations and do not establish general performance or macOS results.

## API 37 idle resource comparison

API 37 measurements used disposable `EmuTrim_API37_FreshControl_20260926`, Emulator 37.2.7.0 (package metadata references `android-sdk-preview-license`), Platform-Tools 37.0.1, ADB server protocol 41, WHPX, image revision 5, 16 KB, and 4096 MB. In three cold-start resource cycles, stock/slim order was AB, BA, AB; each state stabilized for 120 seconds and had five 10-second samples. Statistics are for the console-owner QEMU process. Working Set is its resident working set; `PrivateUsage` is committed private memory, not physical RAM.

Pooled median Working Set was 4772.7 MB stock vs 4788.0 MB slim (+15.3 MB); PrivateUsage was 5630.2 vs 5631.8 MB (+1.6 MB); CPU was 2.266 vs 1.984 seconds per 10-second sample (−0.282s). Median within-cycle CPU delta was −0.328s/10s; Working Set and private-memory results did not show savings. These idle measurements do not establish application performance.

## Emulator startup comparison

Windows x86_64 cold-start comparison: stable Emulator 37.1.11.0 build 15917651 and preview-license Emulator 37.2.7.0 build 16195039 alternated on the same API 37 AVD and host, three starts per build. Total launch-to-boot times were 26.07, 24.63, 24.78s (stable; median 24.78, range 24.63–26.07) and 22.82, 22.62, 22.56s (preview; median 22.62, range 22.56–22.82). This small, single-host sample does not establish a general version advantage or root cause.

EmuTrim `start --timings` reports launch, authenticated console, ADB transport, device, boot-complete, and ready-decision phases only when requested. One API 31 Windows integrated-start acceptance completed in 6.0s; an API 37 Windows integrated start remained offline through its existing 120-second transport bound. No general startup-time claim is made.
