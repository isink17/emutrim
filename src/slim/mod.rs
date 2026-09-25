pub mod profile;
pub mod state;

use crate::adb::shell::{shell_v2, ShellOutput};
use crate::slim::profile::{installed_packages, targets, SETTINGS, STATE_PATH};
use crate::slim::state::{decode, encode};
use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;

#[derive(Debug, Default)]
pub struct Options {
    pub keep: BTreeSet<String>,
    pub skip: BTreeSet<String>,
    pub dry_run: bool,
}
pub fn is_emulator(serial: &str, qemu: &str) -> bool {
    serial.starts_with("emulator-") && qemu.trim() == "1"
}
fn ok(out: ShellOutput, action: &str) -> io::Result<String> {
    if out.status == 0 {
        Ok(out.stdout)
    } else {
        Err(io::Error::other(format!(
            "{action} failed ({}): {}",
            out.status,
            out.stderr.trim()
        )))
    }
}
fn run(addr: SocketAddr, serial: &str, command: &str) -> io::Result<String> {
    ok(shell_v2(addr, serial, command)?, command)
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn slim(addr: SocketAddr, serial: &str, options: &Options) -> io::Result<usize> {
    if !is_emulator(serial, &run(addr, serial, "getprop ro.kernel.qemu")?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing non-emulator transport",
        ));
    }
    if run(addr, serial, "getprop sys.boot_completed")?.trim() != "1" {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "refusing emulator before boot completion",
        ));
    }
    let installed = installed_packages(&run(addr, serial, "pm list packages")?);
    if installed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "guest package manager returned no packages",
        ));
    }
    let planned = targets(&installed, &options.keep, &options.skip);
    for package in &planned {
        println!(
            "  {} {}",
            if options.dry_run {
                "would disable"
            } else {
                "disable"
            },
            package
        );
    }
    if options.dry_run {
        return Ok(planned.len());
    }

    let prior = decode(&run(
        addr,
        serial,
        &format!("cat {STATE_PATH} 2>/dev/null || true"),
    )?)
    .unwrap_or_default();
    let mut state = prior.clone();
    for package in &planned {
        if !state.disabled.contains(package) {
            state.disabled.push(package.clone());
        }
    }
    for (group, ns, key, _) in SETTINGS {
        if !options.skip.contains(*group)
            && !state
                .settings
                .contains_key(&(ns.to_string(), key.to_string()))
        {
            let value = run(addr, serial, &format!("settings get {ns} {key}"))?;
            state.settings.insert(
                (ns.to_string(), key.to_string()),
                match value.trim() {
                    "" | "null" => None,
                    v => Some(v.to_owned()),
                },
            );
        }
    }
    let encoded = encode(&state);
    let write = format!(
        "printf %s {} > {STATE_PATH} && cat {STATE_PATH}",
        quote(&encoded)
    );
    if run(addr, serial, &write)? != encoded {
        return Err(io::Error::other(
            "state record read-back did not match; no guest changes made",
        ));
    }
    let mut changed = 0;
    for package in planned {
        let output = run(addr, serial, &format!("pm disable-user --user 0 {package}"))?;
        if output.contains("disabled-user") || output.contains("new state") {
            changed += 1;
        } else {
            eprintln!("  package not disabled: {package}: {}", output.trim());
        }
    }
    for (group, ns, key, value) in SETTINGS {
        if !options.skip.contains(*group) {
            run(addr, serial, &format!("settings put {ns} {key} {value}"))?;
        }
    }
    if !options.skip.contains("bluetooth") {
        run(addr, serial, "cmd bluetooth_manager disable")?;
    }
    run(addr, serial, "am kill-all")?;
    run(addr, serial, "am trim-memory --all COMPLETE")?;
    Ok(changed)
}

pub fn restore(addr: SocketAddr, serial: &str) -> io::Result<usize> {
    if !is_emulator(serial, &run(addr, serial, "getprop ro.kernel.qemu")?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing non-emulator transport",
        ));
    }
    let mut state = decode(&run(
        addr,
        serial,
        &format!("cat {STATE_PATH} 2>/dev/null || true"),
    )?)
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no EmuTrim state record on this emulator",
        )
    })?;
    let mut pending = Vec::new();
    let mut restored = 0;
    for package in &state.disabled {
        match run(addr, serial, &format!("pm enable {package}")) {
            Ok(out) if out.contains("new state") || out.contains("enabled") => restored += 1,
            Ok(_) => {}
            Err(_) => pending.push(package.clone()),
        }
    }
    for ((ns, key), value) in &state.settings {
        let command = match value {
            Some(value) => format!("settings put {ns} {key} {value}"),
            None => format!("settings delete {ns} {key}"),
        };
        run(addr, serial, &command)?;
    }
    if let Some(Some(value)) = state
        .settings
        .get(&("global".into(), "bluetooth_on".into()))
    {
        if value != "0" {
            run(addr, serial, "cmd bluetooth_manager enable")?;
        }
    }
    if pending.is_empty() {
        run(addr, serial, &format!("rm -f {STATE_PATH}"))?;
    } else {
        state.disabled = pending;
        let encoded = encode(&state);
        run(
            addr,
            serial,
            &format!("printf %s {} > {STATE_PATH}", quote(&encoded)),
        )?;
        return Err(io::Error::other(
            "some packages remain disabled; rerun restore",
        ));
    }
    Ok(restored)
}
