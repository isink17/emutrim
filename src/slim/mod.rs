pub mod profile;
pub mod state;

use crate::adb::shell::{shell_v2, ShellOutput};
use crate::slim::profile::{installed_packages, targets, SETTINGS, STATE_PATH};
use crate::slim::state::{decode, encode, State};
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

fn persist_state(addr: SocketAddr, serial: &str, state: &State) -> io::Result<()> {
    let temporary = format!("{STATE_PATH}.tmp.{}", std::process::id());
    let encoded = encode(state);
    run(
        addr,
        serial,
        &format!("printf %s {} > {temporary}", quote(&encoded)),
    )?;
    if run(addr, serial, &format!("cat {temporary}"))? != encoded {
        return Err(io::Error::other(
            "state record read-back did not match; no guest changes made",
        ));
    }
    run(addr, serial, &format!("mv {temporary} {STATE_PATH}"))?;
    if run(addr, serial, &format!("cat {STATE_PATH}"))? != encoded {
        return Err(io::Error::other(
            "state record verification failed; no guest changes made",
        ));
    }
    Ok(())
}

fn persist_remaining(addr: SocketAddr, serial: &str, state: &State) -> io::Result<()> {
    if state.disabled.is_empty() && state.settings.is_empty() {
        run(addr, serial, &format!("rm -f {STATE_PATH}"))?;
        Ok(())
    } else {
        persist_state(addr, serial, state)
    }
}

fn read_state(addr: SocketAddr, serial: &str) -> io::Result<Option<State>> {
    let response = run(
        addr,
        serial,
        &format!(
            "if [ -e {STATE_PATH} ]; then printf 'present\\n'; cat {STATE_PATH}; else printf 'missing\\n'; fi"
        ),
    )?;
    if response == "missing\n" {
        return Ok(None);
    }
    let encoded = response.strip_prefix("present\n").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "invalid state lookup response")
    })?;
    decode(encoded)
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unrecognized EmuTrim state"))
}

pub fn already_applied(addr: SocketAddr, serial: &str, options: &Options) -> io::Result<bool> {
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
    let Some(state) = read_state(addr, serial)? else {
        return Ok(false);
    };
    let installed = installed_packages(&run(addr, serial, "pm list packages")?);
    let planned = targets(&installed, &options.keep, &options.skip);
    if planned
        .iter()
        .any(|package| !state.disabled.contains(package))
    {
        return Ok(false);
    }
    let disabled = installed_packages(&run(addr, serial, "pm list packages -d")?);
    if planned.iter().any(|package| !disabled.contains(package)) {
        return Ok(false);
    }
    for (group, namespace, key, value) in SETTINGS {
        if options.skip.contains(*group) {
            continue;
        }
        if !state
            .settings
            .contains_key(&(namespace.to_string(), key.to_string()))
            || run(addr, serial, &format!("settings get {namespace} {key}"))?.trim() != *value
        {
            return Ok(false);
        }
    }
    Ok(true)
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

    let prior = read_state(addr, serial)?.unwrap_or_default();
    let disabled = installed_packages(&run(addr, serial, "pm list packages -d")?);
    if planned
        .iter()
        .any(|package| disabled.contains(package) && !prior.disabled.contains(package))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a planned package is already disabled outside EmuTrim state; refusing to overwrite its original state",
        ));
    }
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
    persist_state(addr, serial, &state)?;
    let mut changed = 0;
    for package in planned {
        let output = run(addr, serial, &format!("pm disable-user --user 0 {package}"))?;
        if output.contains("disabled-user") || output.contains("new state") {
            changed += 1;
        } else {
            return Err(io::Error::other(format!(
                "package {package} returned unexpected disable output: {}",
                output.trim()
            )));
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
    Ok(changed)
}

pub fn restore(addr: SocketAddr, serial: &str) -> io::Result<Option<usize>> {
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
    let Some(mut state) = read_state(addr, serial)? else {
        return Ok(None);
    };
    let had_changes = !state.disabled.is_empty() || !state.settings.is_empty();
    let mut restored = 0;
    let mut failure = None;
    for package in state.disabled.clone() {
        let result = run(addr, serial, &format!("pm enable {package}")).and_then(|out| {
            if out.to_ascii_lowercase().contains("enabled") {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "restore package {package} returned unexpected output: {}",
                    out.trim()
                )))
            }
        });
        if let Err(err) = result {
            failure.get_or_insert_with(|| err.to_string());
            continue;
        }
        state.disabled.retain(|saved| saved != &package);
        persist_remaining(addr, serial, &state)?;
        restored += 1;
    }
    for ((ns, key), value) in state.settings.clone() {
        let result = (|| {
            let command = match &value {
                Some(value) => format!("settings put {ns} {key} {value}"),
                None => format!("settings delete {ns} {key}"),
            };
            run(addr, serial, &command)?;
            if ns == "global" && key == "bluetooth_on" {
                if let Some(value) = &value {
                    if value != "0" {
                        run(addr, serial, "cmd bluetooth_manager enable")?;
                    }
                }
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(err) = result {
            failure.get_or_insert_with(|| err.to_string());
            continue;
        }
        state.settings.remove(&(ns, key));
        persist_remaining(addr, serial, &state)?;
    }
    if let Some(err) = failure {
        return Err(io::Error::other(format!(
            "restore incomplete; rerun restore to retry remaining changes: {err}"
        )));
    }
    // State is removed by the final successful per-item checkpoint above. Empty
    // records are removed here for legacy records that contained no changes.
    if !had_changes {
        run(addr, serial, &format!("rm -f {STATE_PATH}"))?;
    }
    Ok(Some(restored))
}

#[cfg(test)]
mod integration_tests;
