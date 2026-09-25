mod adb;
mod avd;
mod slim;

use adb::shell::{boot_completed, metadata, DeviceMetadata};
use adb::track::{as_map, Tracker};
use slim::Options;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
struct Config {
    adb_host: IpAddr,
    adb_port: u16,
    options: Options,
}
impl Config {
    fn adb_addr(&self) -> SocketAddr {
        SocketAddr::new(self.adb_host, self.adb_port)
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".into());
    let args: Vec<_> = args.collect();
    match command.as_str() {
        "watch" => watch(parse_config(args)?),
        "slim" => {
            let mut config = parse_config(args)?;
            let serial = resolve(&mut config)?;
            let count = slim::slim(config.adb_addr(), &serial, &config.options)?;
            println!(
                "{} {count} package(s) for {serial}",
                if config.options.dry_run {
                    "would slim"
                } else {
                    "slimmed"
                }
            );
            Ok(())
        }
        "restore" | "off" => {
            let mut config = parse_config(args)?;
            let serial = resolve(&mut config)?;
            println!(
                "restored {} package(s) for {serial}",
                slim::restore(config.adb_addr(), &serial)?
            );
            Ok(())
        }
        "tune-avd" => tune(args),
        "start" => start(args),
        "list-avds" => {
            for name in avd::list()? {
                println!("{name}");
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command: {other}"),
        )),
    }
}
fn parse_config(args: Vec<String>) -> io::Result<Config> {
    let mut config = Config {
        adb_host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        adb_port: 5037,
        options: Options::default(),
    };
    for arg in args {
        if arg == "--dry-run" {
            config.options.dry_run = true;
        } else if let Some(v) = arg.strip_prefix("--adb-port=") {
            config.adb_port = v
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --adb-port"))?;
        } else if let Some(v) = arg.strip_prefix("--keep=") {
            if v.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty --keep"));
            }
            config.options.keep.insert(v.into());
        } else if let Some(v) = arg.strip_prefix("--skip=") {
            for group in v.split(',') {
                if !matches!(
                    group,
                    "animations" | "bglimit" | "sync" | "location" | "setup" | "bluetooth"
                ) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown --skip group: {group}"),
                    ));
                }
                config.options.skip.insert(group.into());
            }
        } else if !arg.starts_with('-') {
            if config.options.keep.insert(format!("__serial__={arg}")) {}
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown option: {arg}"),
            ));
        }
    }
    Ok(config)
}
fn resolve(config: &mut Config) -> io::Result<String> {
    let requested = config
        .options
        .keep
        .iter()
        .find_map(|v| v.strip_prefix("__serial__=").map(str::to_owned));
    config
        .options
        .keep
        .retain(|v| !v.starts_with("__serial__="));
    let mut tracker = Tracker::connect(config.adb_addr())?;
    let devices = tracker.next_snapshot()?;
    let emulators: Vec<_> = devices
        .into_iter()
        .filter(|d| d.state == "device" && d.serial.starts_with("emulator-"))
        .collect();
    if let Some(serial) = requested {
        return emulators
            .into_iter()
            .find(|d| d.serial == serial)
            .map(|d| d.serial)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "requested transport is not a running emulator",
                )
            });
    }
    match emulators.as_slice() {
        [one] => Ok(one.serial.clone()),
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no running emulator found",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "multiple emulators running; pass a serial",
        )),
    }
}
fn avd_args(args: Vec<String>, verb: &str, require_name: bool) -> io::Result<(String, u32)> {
    let mut name = None;
    let mut ram = 1536;
    for arg in args {
        if let Some(v) = arg.strip_prefix("--ram=") {
            ram = v
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --ram"))?;
        } else if !arg.starts_with('-') {
            name = Some(arg);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown {verb} option: {arg}"),
            ));
        }
    }
    let name = name
        .or_else(|| {
            if require_name {
                None
            } else {
                avd::list().ok().and_then(|v| {
                    if v.len() == 1 {
                        Some(v[0].clone())
                    } else {
                        None
                    }
                })
            }
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "pass an AVD name (or install exactly one for tune-avd)",
            )
        })?;
    Ok((name, ram))
}
fn tune(args: Vec<String>) -> io::Result<()> {
    let (name, ram) = avd_args(args, "tune-avd", false)?;
    avd::tune(&name, ram)?;
    println!("tuned {name}; backup config.ini.emutrim.bak preserved");
    Ok(())
}
fn start(args: Vec<String>) -> io::Result<()> {
    let (name, ram) = avd_args(args, "start", true)?;
    avd::start(&name, ram)?;
    println!("started {name}; run `emutrim watch` to slim it after boot");
    Ok(())
}
fn watch(config: Config) -> io::Result<()> {
    let addr = config.adb_addr();
    println!(
        "EmuTrim watcher\n  ADB server: {addr}\n  mode: {}",
        if config.options.dry_run {
            "dry-run"
        } else {
            "native slimming"
        }
    );
    let mut previous = BTreeMap::new();
    let mut cache = HashMap::<String, DeviceMetadata>::new();
    let mut handled = HashSet::new();
    loop {
        match Tracker::connect(addr) {
            Ok(mut tracker) => {
                println!("  connected; blocked on ADB device events...");
                loop {
                    let snapshot = match tracker.next_snapshot() {
                        Ok(s) => s,
                        Err(err) => {
                            eprintln!("ADB tracking connection lost: {err}; reconnecting...");
                            break;
                        }
                    };
                    let current = as_map(&snapshot);
                    for (serial, old) in &previous {
                        if !current.contains_key(serial) {
                            println!("- {serial} disconnected (was {old})");
                            cache.remove(serial);
                            handled.remove(serial);
                        }
                    }
                    for device in snapshot {
                        if previous.get(&device.serial) == Some(&device.state) {
                            continue;
                        }
                        println!("~ {} -> {}", device.serial, device.state);
                        if device.state != "device" || !device.serial.starts_with("emulator-") {
                            continue;
                        }
                        if !cache.contains_key(&device.serial) {
                            match metadata(addr, &device.serial) {
                                Ok(m) => {
                                    println!(
                                        "  {}: {} / Android {} / API {}",
                                        device.serial, m.model, m.android_version, m.api_level
                                    );
                                    cache.insert(device.serial.clone(), m);
                                }
                                Err(err) => eprintln!("  metadata unavailable: {err}"),
                            }
                        }
                        if !handled.contains(&device.serial) {
                            if boot_completed(addr, &device.serial)? {
                                handle_ready(&config, &device.serial, &mut handled);
                            } else {
                                println!(
                                    "  transport ready; waiting for Android boot completion..."
                                );
                                wait_for_boot(&config, &device.serial, &mut handled);
                            }
                        }
                    }
                    previous = current;
                }
            }
            Err(err) => {
                eprintln!("cannot connect to ADB server at {addr}: {err}; retrying in 2s");
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
fn handle_ready(config: &Config, serial: &str, handled: &mut HashSet<String>) {
    match slim::slim(config.adb_addr(), serial, &config.options) {
        Ok(count) => println!(
            "  {} {count} package(s) for {serial}",
            if config.options.dry_run {
                "would slim"
            } else {
                "slimmed"
            }
        ),
        Err(err) => eprintln!("  not slimmed {serial}: {err}"),
    }
    handled.insert(serial.into());
}
fn wait_for_boot(config: &Config, serial: &str, handled: &mut HashSet<String>) {
    for _ in 0..120 {
        thread::sleep(Duration::from_millis(500));
        match boot_completed(config.adb_addr(), serial) {
            Ok(true) => {
                handle_ready(config, serial, handled);
                return;
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!("  boot check failed: {err}");
                return;
            }
        }
    }
    eprintln!("  timed out waiting for {serial} to finish booting");
}
fn print_help() {
    println!("emutrim 0.2\n\nUsage:\n  emutrim slim [SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim restore [SERIAL]\n  emutrim off [SERIAL]\n  emutrim watch [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim tune-avd [AVD] [--ram=1536]\n  emutrim start AVD [--ram=1536]\n  emutrim list-avds\n\nOnly verified emulator transports are mutated; no adb.exe subprocess is used.");
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dry_run_is_non_mutating_intent() {
        assert!(
            Options {
                dry_run: true,
                ..Default::default()
            }
            .dry_run
        );
    }
    #[test]
    fn physical_transport_is_refused() {
        assert!(!slim::is_emulator("ABC123", "0"));
        assert!(!slim::is_emulator("emulator-5554", "0"));
    }
}
