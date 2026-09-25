mod adb;
mod avd;
mod platform;
mod slim;

use adb::shell::{boot_completed, metadata, shell_v2, DeviceMetadata};
use adb::track::{as_map, devices, DeviceState, Tracker};
use slim::Options;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
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
        "doctor" => doctor(args),
        "stats" => stats(args),
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
    let (name, requested_ram, no_slim) = start_args(args)?;
    let info = avd::inspect(&name)?;
    let ram = requested_ram.unwrap_or(info.ram_mb);
    avd::validate_ram(&info, ram)?;
    if !info.image.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "AVD system image directory not found: {}",
                info.image.display()
            ),
        ));
    }
    if !no_slim && !cfg!(windows) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "integrated start requires Windows process-to-console identity verification",
        ));
    }
    let port = avd::available_console_port()?;
    let serial = format!("emulator-{port}");
    let mut child = avd::start(&name, ram, port)?;
    let launch_pid = child.id();
    println!("started {name} as {serial}");
    if no_slim {
        return Ok(());
    }

    println!("waiting for ADB transport...");
    let mut observation = TransportObservation::default();
    let ready = wait_for_transport_with(
        &serial,
        240,
        || devices(default_adb_addr()),
        || match platform::console_owner_pid(port)? {
            Some(owner) => Ok(Some(platform::belongs_to_launch(owner, launch_pid)?)),
            None => Ok(None),
        },
        || child.try_wait().map(|status| status.is_none()),
        || thread::sleep(Duration::from_millis(500)),
        &mut observation,
    )?;
    if !ready {
        return Err(transport_timeout_error(
            &serial,
            child.try_wait()?.is_none(),
            observation.state.as_deref(),
            observation.error.as_deref(),
        ));
    }
    let qemu = shell_v2(default_adb_addr(), &serial, "getprop ro.kernel.qemu")?;
    if qemu.status != 0 || !slim::is_emulator(&serial, &qemu.stdout) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("requested launch did not verify as emulator transport {serial}"),
        ));
    }

    println!("waiting for Android boot...");
    let mut action = None;
    let boot_result = wait_for_boot_checked(
        &serial,
        240,
        || boot_completed(default_adb_addr(), &serial),
        || child.try_wait().map(|status| status.is_none()),
        || thread::sleep(Duration::from_millis(500)),
        || {
            action = Some(match slim_after_boot(default_adb_addr(), &serial) {
                Ok(SlimResult::AlreadyApplied) => {
                    println!("already slimmed; no guest changes needed");
                    Ok(())
                }
                Ok(SlimResult::Slimmed(count)) => {
                    println!("slimmed {count} package(s)");
                    Ok(())
                }
                Err(error) => Err(error),
            });
        },
    );
    match boot_result? {
        BootWait::Ready => {}
        BootWait::TimedOut => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Android boot timed out on {serial}; guest was not modified"),
            ));
        }
        BootWait::ProcessExited => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("emulator process exited before Android boot completed on {serial}"),
            ));
        }
    }
    action.unwrap_or_else(|| Err(io::Error::other("boot completed without start action")))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlimResult {
    AlreadyApplied,
    Slimmed(usize),
}

fn slim_after_boot(addr: SocketAddr, serial: &str) -> io::Result<SlimResult> {
    match slim::already_applied(addr, serial, &Options::default())? {
        true => Ok(SlimResult::AlreadyApplied),
        false => slim::slim(addr, serial, &Options::default()).map(SlimResult::Slimmed),
    }
}

fn start_args(args: Vec<String>) -> io::Result<(String, Option<u32>, bool)> {
    let mut name = None;
    let mut ram = None;
    let mut no_slim = false;
    for arg in args {
        if arg == "--no-slim" {
            no_slim = true;
        } else if let Some(value) = arg.strip_prefix("--ram=") {
            if ram.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate --ram",
                ));
            }
            ram = Some(
                value
                    .parse()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --ram"))?,
            );
        } else if !arg.starts_with('-') {
            if name.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pass exactly one AVD name",
                ));
            }
            name = Some(arg);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid start argument: {arg}"),
            ));
        }
    }
    Ok((
        name.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pass an AVD name"))?,
        ram,
        no_slim,
    ))
}

fn default_adb_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037)
}

fn doctor(args: Vec<String>) -> io::Result<()> {
    let mut selected_avd = None;
    let mut selected_serial = None;
    for arg in args {
        if let Some(serial) = arg.strip_prefix("--serial=") {
            if serial.is_empty() || selected_serial.replace(serial.to_owned()).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid --serial",
                ));
            }
        } else if !arg.starts_with('-') {
            if selected_avd.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pass at most one AVD name",
                ));
            }
            selected_avd = Some(arg);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid doctor argument: {arg}"),
            ));
        }
    }

    let mut failures = 0usize;
    let sdk = match avd::sdk_dir() {
        Ok(path) => {
            report_check("PASS", &format!("Android SDK: {}", path.display()));
            Some(path)
        }
        Err(error) => {
            report_check("FAIL", &format!("Android SDK: {error}"));
            failures += 1;
            None
        }
    };
    if let Some(sdk) = &sdk {
        let emulator = sdk.join("emulator").join("emulator.exe");
        if emulator.is_file() {
            report_check("PASS", &format!("emulator.exe: {}", emulator.display()));
            match Command::new(&emulator).arg("-version").output() {
                Ok(output) if output.status.success() => {
                    let version_output = if output.stdout.is_empty() {
                        &output.stderr
                    } else {
                        &output.stdout
                    };
                    let version = String::from_utf8_lossy(version_output)
                        .lines()
                        .next()
                        .unwrap_or("version unavailable")
                        .trim()
                        .to_owned();
                    report_check("PASS", &format!("emulator version: {version}"));
                }
                _ => report_check("WARN", "emulator version query failed"),
            }
        } else {
            report_check("FAIL", "emulator.exe missing");
            failures += 1;
        }
    }
    let adb_devices = match devices(default_adb_addr()) {
        Ok(devices) => {
            report_check(
                "PASS",
                &format!(
                    "ADB smart socket: reachable; {} transport(s)",
                    devices.len()
                ),
            );
            Some(devices)
        }
        Err(error) => {
            report_check("FAIL", &format!("ADB smart socket: {error}"));
            failures += 1;
            None
        }
    };
    match avd::list() {
        Ok(avds) => report_check("PASS", &format!("installed AVDs: {}", avds.len())),
        Err(error) => {
            report_check("FAIL", &format!("installed AVDs: {error}"));
            failures += 1;
        }
    }

    if let Some(name) = selected_avd {
        match avd::inspect(&name) {
            Ok(info) => {
                report_check("PASS", &format!("AVD config: {}", name));
                if info.image.is_dir() {
                    report_check("PASS", &format!("system image: {}", info.image.display()));
                } else {
                    report_check(
                        "FAIL",
                        &format!("system image missing: {}", info.image.display()),
                    );
                    failures += 1;
                }
                report_check(
                    "PASS",
                    &format!(
                        "image: API {} / ABI {} / tag {} / pages {}",
                        info.api,
                        info.abi,
                        info.tag,
                        if info.is_16k {
                            "16 KB (detected)"
                        } else {
                            "not tagged 16 KB"
                        }
                    ),
                );
                match avd::validate_ram(&info, info.ram_mb) {
                    Ok(()) => report_check("PASS", &format!("RAM: {} MB compatible", info.ram_mb)),
                    Err(error) => {
                        report_check(
                            "FAIL",
                            &format!("RAM: {} MB incompatible: {error}", info.ram_mb),
                        );
                        failures += 1;
                    }
                }
                report_check(
                    "PASS",
                    &format!("GPU: {} / {}", info.gpu_mode, info.gpu_enabled),
                );
                report_check(
                    if info.backup_exists { "PASS" } else { "WARN" },
                    if info.backup_exists {
                        "EmuTrim config backup exists"
                    } else {
                        "no EmuTrim config backup"
                    },
                );
            }
            Err(error) => {
                report_check("FAIL", &format!("AVD {name}: {error}"));
                failures += 1;
            }
        }
    }

    if let Some(serial) = selected_serial {
        match adb_devices.as_deref() {
            Some(devices) => inspect_running_target(&serial, devices, &mut failures)?,
            None => report_check("FAIL", "target checks skipped: ADB server unavailable"),
        }
    }
    doctor_exit(failures)
}

fn doctor_exit(failures: usize) -> io::Result<()> {
    if failures == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "doctor found {failures} material failure(s)"
        )))
    }
}

fn report_check(status: &str, message: &str) {
    println!("{status} {message}");
}

fn inspect_running_target(
    serial: &str,
    devices: &[DeviceState],
    failures: &mut usize,
) -> io::Result<()> {
    let found = devices
        .iter()
        .find(|device| device.serial == serial)
        .cloned();
    let Some(device) = found else {
        match avd::console_port(serial).and_then(|port| {
            platform::console_owner_pid(port)
                .ok()
                .flatten()
                .map(|pid| (port, pid))
        }) {
            Some((port, pid)) => match avd::console::avd_name(port) {
                Ok(name) => report_check(
                    "FAIL",
                    &format!(
                        "transport {serial}: absent; authenticated emulator console for {name} is still listening (PID {pid}); it may be stuck; guest was not modified"
                    ),
                ),
                Err(_) => report_check(
                    "FAIL",
                    &format!(
                        "transport {serial}: absent; emulator console remains live (PID {pid}), but its AVD name could not be verified"
                    ),
                ),
            },
            None => report_check("FAIL", &format!("transport {serial}: not found")),
        }
        *failures += 1;
        return Ok(());
    };
    if device.state != "device" {
        report_check("WARN", &format!("transport {serial}: {}", device.state));
        return Ok(());
    }
    if !serial.starts_with("emulator-") {
        report_check(
            "FAIL",
            &format!("transport {serial}: physical or unsupported target; no guest inspection"),
        );
        *failures += 1;
        return Ok(());
    }
    match shell_v2(default_adb_addr(), serial, "getprop ro.kernel.qemu") {
        Ok(qemu) if qemu.status == 0 && slim::is_emulator(serial, &qemu.stdout) => {
            report_check("PASS", &format!("emulator identity: {serial}"));
        }
        Ok(_) => {
            report_check("FAIL", &format!("emulator identity: {serial} not verified"));
            *failures += 1;
            return Ok(());
        }
        Err(error) => {
            report_check("FAIL", &format!("emulator identity: {error}"));
            *failures += 1;
            return Ok(());
        }
    }
    let boot = match boot_completed(default_adb_addr(), serial) {
        Ok(true) => {
            report_check("PASS", &format!("Android boot: complete on {serial}"));
            true
        }
        Ok(false) => {
            report_check("WARN", &format!("Android boot: incomplete on {serial}"));
            false
        }
        Err(error) => {
            report_check("WARN", &format!("Android boot: unavailable ({error})"));
            false
        }
    };
    match slim::inspect_state(default_adb_addr(), serial) {
        Ok(None) => report_check("PASS", "EmuTrim state: absent"),
        Ok(Some(state)) => {
            report_check(
                "PASS",
                &format!(
                    "EmuTrim state: valid ({} package(s), {} setting(s))",
                    state.disabled.len(),
                    state.settings.len()
                ),
            );
            if boot {
                match slim::already_applied(default_adb_addr(), serial, &Options::default()) {
                    Ok(true) => report_check("PASS", "EmuTrim state: applied"),
                    Ok(false) => report_check("WARN", "EmuTrim state: present but guest differs"),
                    Err(error) => {
                        report_check("FAIL", &format!("EmuTrim applied-state check: {error}"));
                        *failures += 1;
                    }
                }
            }
        }
        Err(error) => {
            report_check(
                "FAIL",
                &format!("EmuTrim state: invalid or unreadable ({error})"),
            );
            *failures += 1;
        }
    }
    Ok(())
}

fn stats(args: Vec<String>) -> io::Result<()> {
    let (serial, seconds) = stats_args(args)?;
    let port = avd::console_port(&serial).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "stats requires emulator-<even console port>",
        )
    })?;
    let avd_name = avd::console::avd_name(port)?;
    let pid = platform::console_owner_pid(port)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no process owns emulator console port",
        )
    })?;
    let before = platform::process_stats(pid)?;
    thread::sleep(Duration::from_secs(seconds));
    let after = platform::process_stats(pid)?;
    let delta = cpu_seconds_delta(before.cpu_seconds, after.cpu_seconds);
    println!("AVD: {avd_name}  serial: {serial}  process: emulator console");
    println!("PID: {}", after.pid);
    println!(
        "Working Set: {:.1} MB",
        bytes_to_mb(after.working_set_bytes)
    );
    println!("Private memory: {:.1} MB", bytes_to_mb(after.private_bytes));
    println!(
        "CPU total: {:.2} s  sample delta ({seconds}s): {:.3} s",
        after.cpu_seconds, delta
    );
    println!("Threads: {}", after.thread_count);
    if let Some(handles) = after.handle_count {
        println!("Handles: {handles}");
    }
    Ok(())
}

fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn cpu_seconds_delta(start: f64, end: f64) -> f64 {
    (end - start).max(0.0)
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn stats_arguments_and_memory_conversion_are_bounded() {
        assert_eq!(
            stats_args(vec!["emulator-5556".into()]).unwrap(),
            ("emulator-5556".into(), 1)
        );
        assert_eq!(
            stats_args(vec!["emulator-5556".into(), "--seconds=30".into()]).unwrap(),
            ("emulator-5556".into(), 30)
        );
        assert!(stats_args(vec!["emulator-5556".into(), "--seconds=301".into()]).is_err());
        assert_eq!(bytes_to_mb(1024 * 1024), 1.0);
        assert_eq!(cpu_seconds_delta(4.0, 6.5), 2.5);
        assert_eq!(cpu_seconds_delta(6.5, 4.0), 0.0);
    }

    #[test]
    fn doctor_warn_only_succeeds_but_material_failure_fails() {
        assert!(doctor_exit(0).is_ok());
        assert!(doctor_exit(1).is_err());
    }
}

fn stats_args(args: Vec<String>) -> io::Result<(String, u64)> {
    let mut serial = None;
    let mut seconds = 1;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--seconds=") {
            seconds = value
                .parse::<u64>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --seconds"))?;
            if !(1..=300).contains(&seconds) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--seconds must be in 1..=300",
                ));
            }
        } else if !arg.starts_with('-') {
            if serial.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pass exactly one serial",
                ));
            }
            serial = Some(arg);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stats argument: {arg}"),
            ));
        }
    }
    Ok((
        serial.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pass a serial"))?,
        seconds,
    ))
}

#[derive(Default)]
struct TransportObservation {
    state: Option<String>,
    error: Option<String>,
}

fn wait_for_transport_with(
    serial: &str,
    attempts: usize,
    mut snapshot: impl FnMut() -> io::Result<Vec<DeviceState>>,
    mut owns_port: impl FnMut() -> io::Result<Option<bool>>,
    mut process_running: impl FnMut() -> io::Result<bool>,
    mut pause: impl FnMut(),
    observation: &mut TransportObservation,
) -> io::Result<bool> {
    for _ in 0..attempts {
        if !process_running()? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "emulator process exited before ADB transport became ready",
            ));
        }
        match snapshot() {
            Ok(devices) => {
                observation.error = None;
                if let Some(state) = devices
                    .iter()
                    .find(|device| device.serial == serial)
                    .map(|device| device.state.clone())
                {
                    observation.state = Some(state);
                }
                if observation.state.as_deref() == Some("device") {
                    match owns_port()? {
                        Some(true) => return Ok(true),
                        Some(false) => {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                format!("ADB serial {serial} is not owned by launched emulator"),
                            ));
                        }
                        None => {}
                    }
                }
            }
            Err(error) => observation.error = Some(error.to_string()),
        }
        pause();
    }
    Ok(false)
}

fn transport_timeout_error(
    serial: &str,
    process_alive: bool,
    state: Option<&str>,
    last_error: Option<&str>,
) -> io::Error {
    let detail = if !process_alive {
        "emulator process exited before ADB transport became ready".to_owned()
    } else if state == Some("offline") {
        "emulator process is still running, but its ADB transport stayed offline or unavailable; guest was not modified; try Android Studio Cold Boot or inspect emulator logs".to_owned()
    } else if let Some(error) = last_error {
        format!("ADB server unavailable ({error}); guest was not modified")
    } else {
        "emulator process is still running, but its ADB transport is absent and it may be stuck; guest was not modified; inspect emulator logs or try Android Studio Cold Boot".to_owned()
    };
    io::Error::new(io::ErrorKind::TimedOut, format!("{serial}: {detail}"))
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
    check: impl FnMut() -> io::Result<bool>,
    pause: impl FnMut(),
    on_ready: impl FnOnce(),
) -> bool {
    matches!(
        wait_for_boot_checked(serial, attempts, check, || Ok(true), pause, on_ready),
        Ok(BootWait::Ready)
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootWait {
    Ready,
    TimedOut,
    ProcessExited,
}

fn wait_for_boot_checked(
    serial: &str,
    attempts: usize,
    mut check: impl FnMut() -> io::Result<bool>,
    mut process_running: impl FnMut() -> io::Result<bool>,
    mut pause: impl FnMut(),
    on_ready: impl FnOnce(),
) -> io::Result<BootWait> {
    let mut first_error = None;
    for _ in 0..attempts {
        if !process_running()? {
            return Ok(BootWait::ProcessExited);
        }
        pause();
        match check() {
            Ok(true) => {
                on_ready();
                return Ok(BootWait::Ready);
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
    Ok(BootWait::TimedOut)
}
fn watch_target_matches(target: Option<&str>, serial: &str) -> bool {
    target.is_none_or(|target| target == serial)
}
fn print_help() {
    println!("emutrim\n\nUsage:\n  emutrim doctor [AVD] [--serial=SERIAL]\n  emutrim start <AVD> [--ram=N] [--no-slim]\n  emutrim watch [--serial=SERIAL] [--dry-run]\n  emutrim slim [SERIAL] [--dry-run] [--keep=PACKAGE] [--skip=GROUP]\n  emutrim restore [SERIAL]\n  emutrim off [SERIAL]\n  emutrim stats <SERIAL> [--seconds=N]\n  emutrim tune-avd [AVD] [--ram=N]\n  emutrim list-avds\n\nstart launches, waits for Android boot, then slims. --no-slim launches only.\nGuest mutation requires verified emulator identity and completed boot. Runtime ADB uses the smart socket; no adb.exe subprocess.");
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
    fn integrated_boot_wait_reports_early_process_exit() {
        let mut action_called = false;
        let result = wait_for_boot_checked(
            "emulator-5556",
            3,
            || Ok(true),
            || Ok(false),
            || panic!("must not wait after process exit"),
            || action_called = true,
        )
        .unwrap();
        assert_eq!(result, BootWait::ProcessExited);
        assert!(!action_called);
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

#[cfg(test)]
mod start_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn device(serial: &str, state: &str) -> DeviceState {
        DeviceState {
            serial: serial.into(),
            state: state.into(),
        }
    }

    #[test]
    fn start_defaults_ram_to_avd_config_and_accepts_no_slim() {
        assert_eq!(
            start_args(vec!["Test_AVD".into(), "--no-slim".into()]).unwrap(),
            ("Test_AVD".into(), None, true)
        );
        assert_eq!(
            start_args(vec!["Test_AVD".into(), "--ram=4096".into()]).unwrap(),
            ("Test_AVD".into(), Some(4096), false)
        );
        assert!(start_args(vec!["Test_AVD".into(), "Other".into()]).is_err());
    }

    #[test]
    fn transport_wait_stays_bound_to_launched_serial_with_other_emulators_online() {
        let snapshots = Arc::new(Mutex::new(VecDeque::from([
            vec![
                device("emulator-5554", "device"),
                device("emulator-5556", "offline"),
                device("emulator-5558", "offline"),
            ],
            vec![
                device("emulator-5554", "device"),
                device("emulator-5556", "device"),
                device("emulator-5558", "device"),
            ],
            vec![
                device("emulator-5554", "device"),
                device("emulator-5556", "device"),
                device("emulator-5558", "device"),
            ],
        ])));
        let source = snapshots.clone();
        let owners = Arc::new(Mutex::new(VecDeque::from([None, Some(true)])));
        let owner_source = owners.clone();
        let mut observation = TransportObservation::default();
        assert!(wait_for_transport_with(
            "emulator-5558",
            3,
            move || Ok(source.lock().unwrap().pop_front().unwrap_or_default()),
            move || Ok(owner_source.lock().unwrap().pop_front().unwrap_or(None)),
            || Ok(true),
            || {},
            &mut observation,
        )
        .unwrap());
        assert_eq!(observation.state.as_deref(), Some("device"));
        assert_eq!(owners.lock().unwrap().len(), 0);
    }

    #[test]
    fn transport_timeout_and_owner_collision_fail_closed() {
        let mut observation = TransportObservation::default();
        assert!(!wait_for_transport_with(
            "emulator-5558",
            2,
            || Ok(Vec::new()),
            || Ok(None),
            || Ok(true),
            || {},
            &mut observation,
        )
        .unwrap());
        assert!(wait_for_transport_with(
            "emulator-5558",
            1,
            || Ok(vec![device("emulator-5558", "device")]),
            || Ok(Some(false)),
            || Ok(true),
            || {},
            &mut observation,
        )
        .is_err());
        assert!(wait_for_transport_with(
            "emulator-5558",
            1,
            || Ok(Vec::new()),
            || Ok(None),
            || Ok(false),
            || {},
            &mut observation,
        )
        .is_err());
    }

    #[test]
    fn transport_timeout_diagnostic_distinguishes_offline_and_dead_process() {
        assert!(
            transport_timeout_error("emulator-5558", true, Some("offline"), None)
                .to_string()
                .contains("Cold Boot")
        );
        assert!(transport_timeout_error("emulator-5558", false, None, None)
            .to_string()
            .contains("process exited"));
        assert!(transport_timeout_error("emulator-5558", true, None, None)
            .to_string()
            .contains("transport is absent"));
    }
}
