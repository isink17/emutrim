mod adb;
mod avd;
mod slim;

use adb::shell::{boot_completed, metadata, DeviceMetadata};
use adb::track::{as_map, DeviceState, Tracker};
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
    serial: Option<String>,
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
            match slim::restore(config.adb_addr(), &serial)? {
                Some(count) => println!("restored {count} package(s) for {serial}"),
                None => println!("nothing to restore on {serial}"),
            }
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
        serial: None,
        options: Options::default(),
    };
    for arg in args {
        if arg == "--dry-run" {
            config.options.dry_run = true;
        } else if let Some(v) = arg.strip_prefix("--adb-port=") {
            config.adb_port = v
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --adb-port"))?;
        } else if let Some(v) = arg.strip_prefix("--serial=") {
            if v.is_empty() || config.serial.replace(v.into()).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid --serial",
                ));
            }
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
            if config.serial.replace(arg).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multiple target serials",
                ));
            }
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
    let mut tracker = Tracker::connect(config.adb_addr())?;
    let devices = tracker.next_snapshot()?;
    let emulators: Vec<_> = devices
        .into_iter()
        .filter(|d| d.state == "device" && d.serial.starts_with("emulator-"))
        .collect();
    if let Some(serial) = &config.serial {
        return emulators
            .into_iter()
            .find(|d| &d.serial == serial)
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
#[derive(Default)]
struct WatchState {
    previous: BTreeMap<String, String>,
    cache: HashMap<String, DeviceMetadata>,
    handled: HashSet<String>,
}
impl WatchState {
    fn reset_for_reconnect(&mut self, target: Option<&str>) {
        if let Some(serial) = target {
            self.previous.remove(serial);
            self.cache.remove(serial);
            self.handled.remove(serial);
        } else {
            self.previous.clear();
            self.cache.clear();
            self.handled.clear();
        }
    }

    fn changed_emulators(
        &mut self,
        snapshot: Vec<DeviceState>,
        target: Option<&str>,
    ) -> Vec<DeviceState> {
        let current = as_map(&snapshot);
        for (serial, old) in &self.previous {
            if watch_target_matches(target, serial) && !current.contains_key(serial) {
                println!("- {serial} disconnected (was {old})");
                self.cache.remove(serial);
                self.handled.remove(serial);
            }
        }

        let mut ready = Vec::new();
        for device in snapshot {
            if !watch_target_matches(target, &device.serial)
                || self.previous.get(&device.serial) == Some(&device.state)
            {
                continue;
            }
            println!("~ {} -> {}", device.serial, device.state);
            if device.state != "device" {
                if device.serial.starts_with("emulator-") {
                    self.cache.remove(&device.serial);
                    self.handled.remove(&device.serial);
                }
                continue;
            }
            if device.serial.starts_with("emulator-") {
                ready.push(device);
            }
        }
        self.previous = current;
        ready
    }
}
fn watch(config: Config) -> io::Result<()> {
    let addr = config.adb_addr();
    let target = config.serial.clone();
    println!(
        "EmuTrim watcher\n  ADB server: {addr}\n  mode: {}",
        if config.options.dry_run {
            "dry-run"
        } else {
            "native slimming"
        }
    );
    let mut state = WatchState::default();
    loop {
        match Tracker::connect(addr) {
            Ok(mut tracker) => {
                println!("  connected; blocked on ADB device events...");
                loop {
                    let snapshot = match tracker.next_snapshot() {
                        Ok(s) => s,
                        Err(err) => {
                            eprintln!("ADB tracking connection lost: {err}; reconnecting...");
                            state.reset_for_reconnect(target.as_deref());
                            break;
                        }
                    };
                    for device in state.changed_emulators(snapshot, target.as_deref()) {
                        if !state.cache.contains_key(&device.serial) {
                            match metadata(addr, &device.serial) {
                                Ok(m) => {
                                    println!(
                                        "  {}: {} / Android {} / API {}",
                                        device.serial, m.model, m.android_version, m.api_level
                                    );
                                    state.cache.insert(device.serial.clone(), m);
                                }
                                Err(err) => eprintln!("  metadata unavailable: {err}"),
                            }
                        }
                        if !state.handled.contains(&device.serial) {
                            println!("  transport ready; waiting for Android boot completion...");
                            wait_for_boot(&config, &device.serial, &mut state.handled);
                        }
                    }
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
    if !config.options.dry_run {
        match slim::already_applied(config.adb_addr(), serial, &config.options) {
            Ok(true) => {
                println!("  already slimmed {serial}; no guest changes needed");
                handled.insert(serial.into());
                return;
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!("  cannot verify slim state for {serial}: {err}");
                handled.remove(serial);
                return;
            }
        }
    }
    match slim::slim(config.adb_addr(), serial, &config.options) {
        Ok(count) => {
            println!(
                "  {} {count} package(s) for {serial}",
                if config.options.dry_run {
                    "would slim"
                } else {
                    "slimmed"
                }
            );
            handled.insert(serial.into());
        }
        Err(err) => {
            eprintln!("  not slimmed {serial}: {err}");
            handled.remove(serial);
        }
    }
}
fn wait_for_boot(config: &Config, serial: &str, handled: &mut HashSet<String>) {
    wait_for_boot_with(
        serial,
        120,
        || boot_completed(config.adb_addr(), serial),
        || thread::sleep(Duration::from_millis(500)),
        || handle_ready(config, serial, handled),
    );
}
fn wait_for_boot_with(
    serial: &str,
    attempts: usize,
    mut check: impl FnMut() -> io::Result<bool>,
    mut pause: impl FnMut(),
    on_ready: impl FnOnce(),
) -> bool {
    let mut first_error = None;
    for _ in 0..attempts {
        pause();
        match check() {
            Ok(true) => {
                on_ready();
                return true;
            }
            Ok(false) => {}
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err.to_string());
                }
            }
        }
    }
    if let Some(err) = first_error {
        eprintln!(
            "  timed out waiting for {serial} to finish booting; last boot-check error: {err}"
        );
    } else {
        eprintln!("  timed out waiting for {serial} to finish booting");
    }
    false
}
fn watch_target_matches(target: Option<&str>, serial: &str) -> bool {
    target.is_none_or(|target| target == serial)
}
fn print_help() {
    println!("emutrim 0.2\n\nUsage:\n  emutrim slim [SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim restore [SERIAL]\n  emutrim off [SERIAL]\n  emutrim watch [--serial=SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim tune-avd [AVD] [--ram=1536]\n  emutrim start AVD [--ram=1536]\n  emutrim list-avds\n\nOnly verified emulator transports are mutated; no adb.exe subprocess is used.");
}

#[cfg(test)]
mod target_tests {
    use super::*;
    use crate::adb::test_support::{frame, FakeAdb};
    use crate::adb::track::parse_snapshot;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const TWO_EMULATORS: &str = "emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n";

    fn run_explicit(
        serial: &str,
        target_boot: &'static str,
        other_boot: &'static str,
    ) -> (io::Result<usize>, Vec<(String, String)>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let server = FakeAdb::start_with_devices(TWO_EMULATORS, move |serial, command| {
            log.lock().unwrap().push((serial.into(), command.into()));
            let out = match command {
                "getprop ro.kernel.qemu" => "1",
                "getprop sys.boot_completed" if serial == "emulator-5556" => target_boot,
                "getprop sys.boot_completed" => other_boot,
                "pm list packages" => "package:com.google.android.apps.maps\n",
                _ => "",
            };
            [frame(1, out.as_bytes()), frame(3, &[0])].concat()
        });
        let mut config = parse_config(vec![serial.into(), "--dry-run".into()]).unwrap();
        config.adb_port = server.addr().port();
        let result = resolve(&mut config)
            .and_then(|resolved| slim::slim(config.adb_addr(), &resolved, &config.options));
        let calls = seen.lock().unwrap().clone();
        (result, calls)
    }

    #[test]
    fn explicit_target_stays_bound_when_other_emulator_has_opposite_boot_state() {
        for (target, other) in [("1", "0"), ("0", "1")] {
            let (result, calls) = run_explicit("emulator-5556", target, other);
            assert_eq!(result.is_ok(), target == "1");
            assert!(!calls.is_empty());
            assert!(calls.iter().all(|(serial, _)| serial == "emulator-5556"));
            if target == "1" {
                assert!(calls
                    .iter()
                    .any(|(_, command)| command == "pm list packages"));
            } else {
                assert!(!calls
                    .iter()
                    .any(|(_, command)| command == "pm list packages"));
            }
        }
    }

    #[test]
    fn multiple_emulators_without_target_are_rejected() {
        let server = FakeAdb::start_with_devices(TWO_EMULATORS, |_, _| Vec::new());
        let mut config = parse_config(vec!["--dry-run".into()]).unwrap();
        config.adb_port = server.addr().port();
        assert_eq!(
            resolve(&mut config).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn explicit_missing_or_physical_target_never_falls_back() {
        for serial in ["emulator-5558", "0123ABC"] {
            let server = FakeAdb::start_with_devices(TWO_EMULATORS, |_, _| Vec::new());
            let mut config = parse_config(vec![serial.into()]).unwrap();
            config.adb_port = server.addr().port();
            assert!(resolve(&mut config).is_err());
        }
    }

    #[test]
    fn scoped_watcher_matches_only_requested_serial() {
        assert!(watch_target_matches(Some("emulator-5556"), "emulator-5556"));
        assert!(!watch_target_matches(
            Some("emulator-5556"),
            "emulator-5554"
        ));
        assert!(!watch_target_matches(Some("emulator-5556"), "0123ABC"));
        assert!(watch_target_matches(None, "emulator-5554"));
    }

    #[test]
    fn offline_resets_target_and_duplicate_snapshots_do_not_retrigger() {
        let mut state = WatchState::default();
        let snapshot = |text| parse_snapshot(text).unwrap();
        assert!(state
            .changed_emulators(
                snapshot(b"emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n"),
                Some("emulator-5556")
            )
            .is_empty());
        assert!(state
            .changed_emulators(
                snapshot(b"emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n"),
                Some("emulator-5556")
            )
            .is_empty());
        assert_eq!(
            state
                .changed_emulators(
                    snapshot(b"emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n"),
                    Some("emulator-5556")
                )
                .len(),
            1
        );
        state.handled.insert("emulator-5556".into());
        assert!(state
            .changed_emulators(
                snapshot(b"emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n"),
                Some("emulator-5556")
            )
            .is_empty());
        assert!(!state.handled.contains("emulator-5556"));
        let repeated_device =
            snapshot(b"emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n");
        assert_eq!(
            state
                .changed_emulators(repeated_device.clone(), Some("emulator-5556"))
                .len(),
            1
        );
        assert!(state
            .changed_emulators(repeated_device, Some("emulator-5556"))
            .is_empty());
    }

    #[test]
    fn boot_wait_retries_empty_properties_and_transient_transport_error() {
        let values = Arc::new(Mutex::new(VecDeque::from([
            Err("offline".to_owned()),
            Ok(false),
            Ok(false),
            Ok(true),
        ])));
        let source = values.clone();
        let server = FakeAdb::start_with_devices("", move |serial, command| {
            assert_eq!(serial, "emulator-5556");
            assert_eq!(command, "getprop sys.boot_completed");
            match source.lock().unwrap().pop_front().unwrap() {
                Ok(booted) => {
                    let value = if booted { "1" } else { "" };
                    [frame(1, value.as_bytes()), frame(3, &[0])].concat()
                }
                Err(error) => [frame(2, error.as_bytes()), frame(3, &[1])].concat(),
            }
        });
        let checks = Arc::new(Mutex::new(0));
        let ready = checks.clone();
        assert!(wait_for_boot_with(
            "emulator-5556",
            5,
            || boot_completed(server.addr(), "emulator-5556"),
            || {},
            move || *ready.lock().unwrap() += 1,
        ));
        assert_eq!(*checks.lock().unwrap(), 1);
        assert!(values.lock().unwrap().is_empty());
    }

    #[test]
    fn boot_timeout_does_not_poison_later_reconnect() {
        let mut state = WatchState::default();
        let device = parse_snapshot(b"emulator-5556\tdevice\n").unwrap();
        assert_eq!(
            state.changed_emulators(device, Some("emulator-5556")).len(),
            1
        );
        let ready_count = Arc::new(Mutex::new(0));
        let ready = ready_count.clone();
        assert!(!wait_for_boot_with(
            "emulator-5556",
            2,
            || Ok(false),
            || {},
            move || *ready.lock().unwrap() += 1,
        ));
        assert_eq!(*ready_count.lock().unwrap(), 0);

        state.changed_emulators(
            parse_snapshot(b"emulator-5556\toffline\n").unwrap(),
            Some("emulator-5556"),
        );
        let reconnect = state.changed_emulators(
            parse_snapshot(b"emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n")
                .unwrap(),
            Some("emulator-5556"),
        );
        assert_eq!(reconnect.len(), 1);
        let ready = ready_count.clone();
        assert!(wait_for_boot_with(
            "emulator-5556",
            2,
            || Ok(true),
            || {},
            move || *ready.lock().unwrap() += 1,
        ));
        assert_eq!(*ready_count.lock().unwrap(), 1);
    }

    #[test]
    fn fake_tracker_reconnect_retries_only_scoped_target_after_slow_boot() {
        const EVENTS: &[&str] = &[
            "emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n",
            "emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n",
            "emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n",
            "emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n",
            "emulator-5554\tdevice\nemulator-5556\toffline\n0123ABC\tdevice\n",
            "emulator-5554\tdevice\nemulator-5556\tdevice\n0123ABC\tdevice\n",
        ];
        let server = FakeAdb::start_with_snapshots(EVENTS, |serial, command| {
            assert_eq!(serial, "emulator-5556");
            assert_eq!(command, "getprop sys.boot_completed");
            [frame(1, b"1"), frame(3, &[0])].concat()
        });
        let mut tracker = Tracker::connect(server.addr()).unwrap();
        let mut state = WatchState::default();
        let mut slim_attempts = 0;
        for (index, _) in EVENTS.iter().enumerate() {
            let snapshot = tracker.next_snapshot().unwrap();
            let candidates = state.changed_emulators(snapshot, Some("emulator-5556"));
            if candidates.is_empty() {
                continue;
            }
            if index == 2 {
                let serial = candidates[0].serial.clone();
                assert!(wait_for_boot_with(
                    &serial,
                    4,
                    || boot_completed(server.addr(), &serial),
                    || {},
                    || slim_attempts += 1,
                ));
                state.handled.insert(serial);
            } else {
                let serial = candidates[0].serial.clone();
                assert!(wait_for_boot_with(
                    &serial,
                    1,
                    || boot_completed(server.addr(), &serial),
                    || {},
                    || slim_attempts += 1,
                ));
                state.handled.insert(serial);
            }
        }
        assert_eq!(slim_attempts, 2);
        assert!(state.handled.contains("emulator-5556"));
        assert!(!state.handled.contains("emulator-5554"));
        assert!(!state.handled.contains("0123ABC"));
    }
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
