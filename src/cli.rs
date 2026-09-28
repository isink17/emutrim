pub fn print_help() {
    let managed_root = if cfg!(windows) {
        "%USERPROFILE%\\.emutrim"
    } else {
        "~/.emutrim"
    };
    let stats_note = if cfg!(windows) {
        "stats reports Windows process metrics."
    } else if cfg!(target_os = "macos") {
        "stats is not supported on macOS."
    } else {
        "stats is not available on this platform."
    };
    println!("emutrim {}\n\nUsage:\n  emutrim doctor [AVD] [--serial=SERIAL] [--managed] [--json]\n  emutrim status <SERIAL> [--json]\n  emutrim stop <SERIAL|AVD>\n  emutrim start <AVD> [--managed] [--ram=N] [--no-slim] [--timings] [--cold-boot] [--headless]\n  emutrim watch [SERIAL | --serial=SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim slim [SERIAL | --serial=SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim restore [SERIAL | --serial=SERIAL]\n  emutrim off [SERIAL | --serial=SERIAL]\n  emutrim stats <SERIAL> [--seconds=N]\n  emutrim tune-avd [AVD] [--managed] [--ram=N]\n  emutrim list-avds [--managed] [--json]\n  emutrim managed root|status|setup|clean [--yes]\n  emutrim --version, -V\n\nDefault EmuTrim managed root: {managed_root}; override: EMUTRIM_HOME.\nstart launches and waits for the exact transport to finish booting, then applies reversible slimming unless --no-slim is set. --timings reports startup phases; --cold-boot bypasses Quick Boot for that launch. --headless adds only -no-window.\nstatus is read-only; unavailable values are omitted in human output and null in JSON. stop requires exact authenticated console identity and never targets physical devices.\nGuest mutation requires verified emulator identity and completed boot. Runtime ADB uses the smart socket; no adb.exe subprocess.\n{stats_note}", env!("CARGO_PKG_VERSION"));
}

pub fn command_help(command: &str, args: &[String]) -> Option<&'static str> {
    if !args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return None;
    }
    Some(match command {
        "start" => "Usage: emutrim start <AVD> [--managed] [--ram=N] [--no-slim] [--cold-boot] [--timings] [--headless]\n\n--headless adds -no-window. --ram=N sets RAM in MB; omitted uses AVD's configured RAM. --no-slim skips slimming after boot. --cold-boot bypasses Quick Boot for this launch. --timings reports startup phases.",
        "status" => "Usage: emutrim status <SERIAL> [--json]\n\nSERIAL is required. Reports transport, emulator/physical/unknown identity, available boot and saved state, AVD name, and managed/external/unknown ownership. Physical devices are read-only.",
        "stop" => "Usage: emutrim stop <SERIAL|AVD>\n\nSERIAL targets exact emulator. AVD name requires one exact authenticated console match. Physical devices are refused; shutdown does not require boot completion. Existing stopped AVD reports already stopped; ambiguous identity refuses.",
        "doctor" => "Usage: emutrim doctor [AVD] [--serial=SERIAL] [--managed] [--json]\n\nChecks SDK, Emulator, ADB, installed AVDs, and optional AVD or running target. --managed selects EmuTrim-managed Android assets. --json emits structured checks.",
        "slim" => "Usage: emutrim slim [SERIAL | --serial=SERIAL] [--adb-port=N] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n\n--skip groups: animations,bglimit,sync,location,setup,bluetooth. Without a serial, requires exactly one running emulator.",
        "restore" | "off" => "Usage: emutrim restore [SERIAL | --serial=SERIAL] [--adb-port=N]\n       emutrim off [SERIAL | --serial=SERIAL] [--adb-port=N]\n\nRestores saved original state. Without a serial, requires exactly one running emulator. off is an alias for restore.",
        "watch" => "Usage: emutrim watch [SERIAL | --serial=SERIAL] [--adb-port=N] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n\n--skip groups: animations,bglimit,sync,location,setup,bluetooth. Watches ADB device events and handles eligible emulators.",
        "stats" => stats_help(),
        "tune-avd" => "Usage: emutrim tune-avd [AVD] [--managed] [--ram=N]\n\nWithout --ram, selects 4096 MB for detected 16 KB images and 1536 MB for other configured images. Missing image.sysdir.1 is an error. Without AVD, exactly one AVD must be installed in the selected environment. Backs up config.ini before changing RAM and GPU settings.",
        "list-avds" => "Usage: emutrim list-avds [--managed] [--json]\n\nLists installed AVDs in the selected environment.",
        "managed" => managed_help(args),
        _ => return None,
    })
}

fn stats_help() -> &'static str {
    if cfg!(windows) {
        "Usage: emutrim stats <SERIAL> [--seconds=N]\n\nN must be 1..=300 (default 1). Reports Windows process metrics."
    } else if cfg!(target_os = "macos") {
        "Usage: emutrim stats <SERIAL> [--seconds=N]\n\nN must be 1..=300 (default 1). stats is not supported on macOS."
    } else {
        "Usage: emutrim stats <SERIAL> [--seconds=N]\n\nN must be 1..=300 (default 1). stats is not available on this platform."
    }
}

fn managed_help(args: &[String]) -> &'static str {
    match args.iter().find(|arg| arg.as_str() != "--help" && arg.as_str() != "-h").map(String::as_str) {
        Some("root") => "Usage: emutrim managed root\n\nPrints managed data paths. EMUTRIM_HOME overrides the default root.",
        Some("status") => "Usage: emutrim managed status\n\nReports managed SDK, AVD, manifest, and disk usage.",
        Some("setup") => "Usage: emutrim managed setup\n\nSets up managed Android assets. Currently supported on Apple Silicon macOS.",
        Some("clean") => "Usage: emutrim managed clean [--yes]\n\nWithout --yes, previews managed payload cleanup. --yes removes managed payloads after safety checks.",
        _ => "Usage: emutrim managed root|status|setup|clean [--yes]\n\nUse 'emutrim managed <command> --help' for command details.",
    }
}
