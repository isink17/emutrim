---
name: emutrim-adb
description: Use when changing ADB transport, smart-socket framing, device tracking, shell execution, or reconnect behavior.
---

# EmuTrim ADB

- Main code: `src/adb/protocol.rs` frames smart-socket services and length-prefixed payloads; `track.rs` owns `host:track-devices`; `shell.rs` selects transport and reads shell-v2 packets.
- Preserve direct TCP to the configured ADB server. Distinguish host services from device services: select `host:transport:<serial>` before guest shell commands.
- Keep shell-v2 exit status and stderr. Mutation callers must reject nonzero status; do not fall back to legacy `shell:` for mutations.
- Handle malformed, invalid, or truncated framing as errors. Add focused parser tests for protocol changes; verify partial reads and EOF behavior.
- Bound accumulated shell-v2 output at 16 MiB; reject oversized frames before allocation. Exercise production framing with `src/adb/test_support.rs` loopback server.
- Tracking is event-driven. Reconnect after stream loss without steady-state polling; keep shell requests on their own connections.
- ADB `device` state does not imply Android framework boot completion. Wait for `sys.boot_completed=1`, retrying boot checks within a bounded per-target wait.
- Explicit target selection is sticky: unrelated ADB transports must never influence or replace the selected device. Scoped watchers (`--serial`) must ignore every other transport and resume only for the selected serial after reconnect.
- No runtime `adb.exe` subprocess fallback by default. Physical transports may be observed but must never reach guest mutation.
- Use a local/mock TCP integration harness for service ordering, stream closure, and reconnect changes; never require a physical device for protocol tests.
