mod adb;
mod avd;
mod cli;
mod commands;
mod managed;
mod output;
mod platform;
mod slim;

use adb::protocol::remaining_until;
use adb::shell::{
    boot_completed, boot_completed_with_timeout, metadata, shell_v2, shell_v2_with_timeout,
    DeviceMetadata,
};
use adb::track::{as_map, devices, devices_with_timeout, DeviceState, Tracker};
use slim::Options;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

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
    if let Some(help) = cli::command_help(&command, &args) {
        println!("{help}");
        return Ok(());
    }
    match command.as_str() {
        "--version" | "-V" => {
            println!("emutrim {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
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
        "managed" => managed::run(args),
        "start" => start(args),
        "status" => {
            if args.iter().any(|arg| arg == "--json") {
                commands::status::print_json_result(
                    &args
                        .into_iter()
                        .filter(|arg| arg != "--json")
                        .collect::<Vec<_>>(),
                )
            } else {
                commands::status::run(args)
            }
        }
        "stop" => commands::stop::run(args),
        "clear" => commands::clear::run(args),
        "doctor" => doctor(args),
        "stats" => stats(args),
        "list-avds" => {
            let managed = args.iter().any(|arg| arg == "--managed");
            let json = args.iter().any(|arg| arg == "--json");
            if args.iter().any(|arg| arg != "--managed" && arg != "--json") {
                let error = io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: list-avds [--managed] [--json]",
                );
                if json {
                    output::failure("invalid_arguments", &error.to_string());
                }
                return Err(error);
            }
            let names = match avd::list_mode(managed) {
                Ok(names) => names,
                Err(error) if json => {
                    output::failure(output::code_for(&error), &error.to_string());
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            if json {
                #[derive(serde::Serialize)]
                struct Avd {
                    name: String,
                    ownership: String,
                }
                output::success(
                    names
                        .into_iter()
                        .map(|name| Avd {
                            ownership: commands::status::ownership(&name),
                            name,
                        })
                        .collect::<Vec<_>>(),
                );
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            cli::print_help();
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
fn avd_args(
    args: Vec<String>,
    verb: &str,
    require_name: bool,
    managed: bool,
) -> io::Result<(String, Option<u32>)> {
    let mut name = None;
    let mut ram = None;
    for arg in args {
        if let Some(v) = arg.strip_prefix("--ram=") {
            ram = Some(
                v.parse()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid --ram"))?,
            );
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
                avd::list_mode(managed).ok().and_then(|v| {
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
    let managed = args.iter().any(|arg| arg == "--managed");
    let args: Vec<String> = args.into_iter().filter(|arg| arg != "--managed").collect();
    let (name, ram) = avd_args(args, "tune-avd", false, managed)?;
    avd::tune_mode(&name, ram, managed)?;
    println!("tuned {name}; backup config.ini.emutrim.bak preserved");
    Ok(())
}
fn start(args: Vec<String>) -> io::Result<()> {
    let managed = args.iter().any(|arg| arg == "--managed");
    let args: Vec<String> = args.into_iter().filter(|arg| arg != "--managed").collect();
    let (name, requested_ram, no_slim, timings, cold_boot, headless) = start_args(args)?;
    let info = avd::inspect_mode(&name, managed)?;
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
    if !no_slim && !platform::supports_verified_integrated_start() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "integrated start requires verified process-to-console identity support",
        ));
    }
    let port = avd::available_console_port()?;
    let serial = format!("emulator-{port}");
    let mut child = avd::start_mode(&name, ram, port, cold_boot, headless, managed)?;
    let mut timing = StartupTiming {
        capture: timings,
        launched: Some(Instant::now()),
        ..StartupTiming::default()
    };
    let launch_pid = child.id();
    println!("started {name} as {serial}");
    println!("waiting for ADB transport...");
    let transport_deadline = Instant::now() + Duration::from_secs(120);
    let ready = wait_for_transport_with(
        &serial,
        transport_deadline,
        |deadline| devices_with_timeout(default_adb_addr(), deadline),
        || {
            if no_slim {
                Ok(Some(true))
            } else {
                match platform::console_owner_pid(port)? {
                    Some(owner) => Ok(Some(platform::belongs_to_launch(owner, launch_pid)?)),
                    None => Ok(None),
                }
            }
        },
        |deadline| {
            let running = child.try_wait().map(|status| status.is_none())?;
            let console = remaining_until(deadline).is_ok()
                && avd::console::avd_name_until(port, deadline).is_ok_and(|actual| actual == name);
            Ok((running, console))
        },
        |remaining| thread::sleep(remaining.min(Duration::from_millis(500))),
        &mut timing,
    )?;
    if timing.console.is_none() {
        if timings {
            eprintln!("startup timing: {}", timing.format());
        }
        return Err(console_timeout_error(&name));
    }
    if !ready {
        if timings {
            eprintln!("startup timing: {}", timing.format());
        }
        return Err(transport_timeout_error(
            &serial,
            child.try_wait()?.is_none(),
            timing.state.as_deref(),
            timing.error.as_deref(),
        ));
    }
    let qemu = shell_v2_with_timeout(
        default_adb_addr(),
        &serial,
        "getprop ro.kernel.qemu",
        Instant::now() + Duration::from_secs(3),
    )?;
    if qemu.status != 0 || !slim::is_emulator(&serial, &qemu.stdout) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("requested launch did not verify as emulator transport {serial}"),
        ));
    }

    if timings {
        println!("ADB transport ready: {serial} device; waiting for Android boot...");
    } else {
        println!("waiting for Android boot...");
    }
    let boot_deadline = Instant::now() + Duration::from_secs(120);
    let mut action = None;
    let boot_result = wait_for_boot_until(
        &serial,
        boot_deadline,
        |deadline| {
            let result = boot_completed_with_timeout(default_adb_addr(), &serial, deadline);
            if matches!(&result, Ok(true)) {
                timing.boot = Some(Instant::now());
            }
            result
        },
        || child.try_wait().map(|status| status.is_none()),
        |remaining| thread::sleep(remaining.min(Duration::from_millis(500))),
        || {
            action = Some(if no_slim {
                Ok(())
            } else {
                match slim_after_boot(default_adb_addr(), &serial) {
                    Ok(SlimResult::AlreadyApplied) => {
                        println!("already slimmed; no guest changes needed");
                        Ok(())
                    }
                    Ok(SlimResult::Slimmed(count)) => {
                        println!("slimmed {count} package(s)");
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            });
        },
    );
    match boot_result? {
        BootWait::Ready => {}
        BootWait::TimedOut => {
            if timings {
                eprintln!("startup timing: {}", timing.format());
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Android boot timed out on {serial}; last state: {} (boot incomplete); guest was not modified",
                    timing.state.as_deref().unwrap_or("absent")
                ),
            ));
        }
        BootWait::ProcessExited => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("emulator process exited before Android boot completed on {serial}"),
            ));
        }
    }
    let result =
        action.unwrap_or_else(|| Err(io::Error::other("boot completed without start action")));
    if no_slim && result.is_ok() {
        println!("guest was not modified");
    }
    if result.is_ok() && timings {
        timing.ready = Some(Instant::now());
        println!("startup timing: {}", timing.format());
    }
    result
}

fn console_timeout_error(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for authenticated console for {name} (launch→console phase)"),
    )
}

#[derive(Default)]
struct StartupTiming {
    capture: bool,
    launched: Option<Instant>,
    console: Option<Instant>,
    transport: Option<Instant>,
    device: Option<Instant>,
    boot: Option<Instant>,
    ready: Option<Instant>,
    state: Option<String>,
    error: Option<String>,
}

impl StartupTiming {
    fn elapsed(&self, start: Option<Instant>, end: Option<Instant>) -> String {
        match (start, end) {
            (Some(start), Some(end)) => {
                if end >= start {
                    format!("{:.1}s", end.duration_since(start).as_secs_f64())
                } else {
                    "overlap".into()
                }
            }
            _ => "n/a".into(),
        }
    }

    fn format(&self) -> String {
        let launched = self.launched;
        let ended = self.ready.or_else(|| (self.capture).then(Instant::now));
        format!(
            "launch→console {}; console→ADB {}; ADB→device {}; device→boot {}; boot→ready {}; total {}",
            self.elapsed(launched, self.console),
            self.elapsed(self.console, self.transport),
            self.elapsed(self.transport, self.device),
            self.elapsed(self.device, self.boot),
            if self.ready.is_some() {
                self.elapsed(self.boot, self.ready)
            } else {
                "n/a".into()
            },
            match (launched, ended) {
                (Some(start), Some(end)) => format!("{:.1}s", end.duration_since(start).as_secs_f64()),
                _ => "n/a".into(),
            }
        )
    }
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

fn start_args(args: Vec<String>) -> io::Result<(String, Option<u32>, bool, bool, bool, bool)> {
    let mut name = None;
    let mut ram = None;
    let mut no_slim = false;
    let mut timings = false;
    let mut cold_boot = false;
    let mut headless = false;
    for arg in args {
        if arg == "--no-slim" {
            no_slim = true;
        } else if arg == "--timings" {
            timings = true;
        } else if arg == "--cold-boot" {
            cold_boot = true;
        } else if arg == "--headless" {
            headless = true;
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
        timings,
        cold_boot,
        headless,
    ))
}

fn default_adb_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037)
}

fn doctor(args: Vec<String>) -> io::Result<()> {
    let managed = args.iter().any(|arg| arg == "--managed");
    let json = args.iter().any(|arg| arg == "--json");
    output::begin_doctor_json(json);
    let args: Vec<String> = args
        .into_iter()
        .filter(|arg| arg != "--managed" && arg != "--json")
        .collect();
    let mut selected_avd = None;
    let mut selected_serial = None;
    for arg in args {
        if let Some(serial) = arg.strip_prefix("--serial=") {
            if serial.is_empty() || selected_serial.replace(serial.to_owned()).is_some() {
                let error = io::Error::new(io::ErrorKind::InvalidInput, "invalid --serial");
                return doctor_arg_error(json, error);
            }
        } else if !arg.starts_with('-') {
            if selected_avd.is_some() {
                let error =
                    io::Error::new(io::ErrorKind::InvalidInput, "pass at most one AVD name");
                return doctor_arg_error(json, error);
            }
            selected_avd = Some(arg);
        } else {
            let error = io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid doctor argument: {arg}"),
            );
            return doctor_arg_error(json, error);
        }
    }

    let mut failures = 0usize;
    let sdk = match avd::sdk_dir_mode(managed) {
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
        let emulator = avd::emulator_path(sdk);
        if emulator.is_file() {
            report_check("PASS", &format!("Emulator binary: {}", emulator.display()));
            match std::fs::read_to_string(sdk.join("emulator").join("package.xml")) {
                Ok(metadata) if emulator_uses_preview_license(&metadata) => report_check(
                    "WARN",
                    "emulator package metadata references preview license; stable channel not confirmed",
                ),
                Ok(_) => report_check(
                    "INFO",
                    "emulator channel is not stated by local package metadata",
                ),
                Err(error) => report_check(
                    "WARN",
                    &format!("emulator package metadata unavailable: {error}"),
                ),
            }
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
            report_check(
                "FAIL",
                &format!("Emulator binary missing: {}", emulator.display()),
            );
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
            match adb::protocol::host_protocol_version(default_adb_addr()) {
                Ok(version) => report_check(
                    "PASS",
                    &format!("ADB server protocol: {version} (0x{version:04x})"),
                ),
                Err(error) => report_check("WARN", &format!("ADB server version: {error}")),
            }
            if let Some(sdk) = &sdk {
                match platform::tcp_listener_image(5037) {
                    Ok(Some((pid, path))) => {
                        let expected = sdk.join("platform-tools").join("adb.exe");
                        if path
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&expected.to_string_lossy())
                        {
                            report_check(
                                "PASS",
                                &format!("ADB server listener: PID {pid}; {}", path.display()),
                            );
                        } else {
                            report_check(
                                "WARN",
                                &format!(
                                    "ADB server listener: PID {pid}; {} (differs from SDK adb {})",
                                    path.display(),
                                    expected.display()
                                ),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => report_check("WARN", &format!("ADB listener identity: {error}")),
                }
            }
            Some(devices)
        }
        Err(error) => {
            report_check("FAIL", &format!("ADB smart socket: {error}"));
            failures += 1;
            None
        }
    };
    match avd::list_mode(managed) {
        Ok(avds) => report_check("PASS", &format!("installed AVDs: {}", avds.len())),
        Err(error) => {
            report_check("FAIL", &format!("installed AVDs: {error}"));
            failures += 1;
        }
    }

    if let Some(name) = selected_avd {
        match avd::inspect_mode(&name, managed) {
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
    if json {
        let report = output::finish_doctor_json(failures);
        if failures == 0 {
            output::success(report);
            Ok(())
        } else {
            let message = format!("doctor found {failures} material failure(s)");
            output::failure_with_data("doctor_failed", &message, report);
            doctor_exit(failures)
        }
    } else {
        doctor_exit(failures)
    }
}

fn doctor_arg_error(json: bool, error: io::Error) -> io::Result<()> {
    if json {
        output::failure("invalid_arguments", &error.to_string());
    }
    Err(error)
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

fn emulator_uses_preview_license(metadata: &str) -> bool {
    let Some(start) = metadata.find("<localPackage path=\"emulator\"") else {
        return false;
    };
    let package = &metadata[start..];
    let Some(end) = package.find("</localPackage>") else {
        return false;
    };
    package[..end].contains("android-sdk-preview-license")
}

fn report_check(status: &str, message: &str) {
    if !output::report_doctor_check(status, message) {
        println!("{status} {message}");
    }
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
    fn tune_avd_arguments_leave_default_selection_to_avd_config() {
        assert_eq!(
            avd_args(vec!["Test_AVD".into()], "tune-avd", false, false).unwrap(),
            ("Test_AVD".into(), None)
        );
        assert_eq!(
            avd_args(
                vec!["Test_AVD".into(), "--ram=5120".into()],
                "tune-avd",
                false,
                false
            )
            .unwrap(),
            ("Test_AVD".into(), Some(5120))
        );
    }

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

    #[test]
    fn detects_preview_license_only_on_emulator_package() {
        assert!(emulator_uses_preview_license(
            "<localPackage path=\"emulator\"><uses-license ref=\"android-sdk-preview-license\"/></localPackage>"
        ));
        assert!(!emulator_uses_preview_license(
            "<localPackage path=\"other\"><uses-license ref=\"android-sdk-preview-license\"/></localPackage>"
        ));
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

fn wait_for_transport_with(
    serial: &str,
    deadline: Instant,
    mut snapshot: impl FnMut(Instant) -> io::Result<Vec<DeviceState>>,
    mut owns_port: impl FnMut() -> io::Result<Option<bool>>,
    mut process_and_console: impl FnMut(Instant) -> io::Result<(bool, bool)>,
    mut pause: impl FnMut(Duration),
    timing: &mut StartupTiming,
) -> io::Result<bool> {
    loop {
        match remaining_until(deadline) {
            Ok(_) => {}
            Err(_) => return Ok(false),
        }
        match snapshot(deadline) {
            Ok(devices) => {
                timing.error = None;
                let state = devices
                    .iter()
                    .find(|device| device.serial == serial)
                    .map(|device| device.state.clone());
                timing.state = state.clone();
                if state.is_some() && timing.capture {
                    timing.transport.get_or_insert_with(Instant::now);
                    if timing.state.as_deref() == Some("device") {
                        timing.device.get_or_insert_with(Instant::now);
                    }
                }
            }
            Err(error) => timing.error = Some(error.to_string()),
        }
        let (process_running, console_ready) = process_and_console(deadline)?;
        if !process_running {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "emulator process exited before ADB transport became ready",
            ));
        }
        if timing.console.is_none() && console_ready {
            timing.console = Some(Instant::now());
        }
        if timing.state.as_deref() == Some("device") {
            if remaining_until(deadline).is_err() {
                return Ok(false);
            }
            match owns_port()? {
                Some(true) if timing.console.is_some() => {
                    if remaining_until(deadline).is_err() {
                        return Ok(false);
                    }
                    return Ok(true);
                }
                Some(true) => {}
                Some(false) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("ADB serial {serial} is not owned by launched emulator"),
                    ));
                }
                None => {}
            }
        }
        let remaining = match remaining_until(deadline) {
            Ok(remaining) => remaining,
            Err(_) => return Ok(false),
        };
        pause(remaining.min(Duration::from_millis(500)));
    }
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

fn wait_for_boot_until(
    serial: &str,
    deadline: Instant,
    mut check: impl FnMut(Instant) -> io::Result<bool>,
    mut process_running: impl FnMut() -> io::Result<bool>,
    mut pause: impl FnMut(Duration),
    on_ready: impl FnOnce(),
) -> io::Result<BootWait> {
    let mut first_error = None;
    loop {
        if remaining_until(deadline).is_err() {
            break;
        }
        if !process_running()? {
            return Ok(BootWait::ProcessExited);
        }
        let result = check(deadline);
        if remaining_until(deadline).is_err() {
            break;
        }
        match result {
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
        let remaining = match remaining_until(deadline) {
            Ok(remaining) => remaining,
            Err(_) => break,
        };
        pause(remaining.min(Duration::from_millis(500)));
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
            ("Test_AVD".into(), None, true, false, false, false)
        );
        assert_eq!(
            start_args(vec!["Test_AVD".into(), "--ram=4096".into()]).unwrap(),
            ("Test_AVD".into(), Some(4096), false, false, false, false)
        );
        assert_eq!(
            start_args(vec!["Test_AVD".into(), "--timings".into()]).unwrap(),
            ("Test_AVD".into(), None, false, true, false, false)
        );
        assert_eq!(
            start_args(vec!["Test_AVD".into(), "--cold-boot".into()]).unwrap(),
            ("Test_AVD".into(), None, false, false, true, false)
        );
        assert!(start_args(vec!["Test_AVD".into(), "Other".into()]).is_err());
    }

    #[test]
    fn start_flags_compose_independently_and_in_any_order() {
        for (flags, no_slim, timings, cold_boot) in [
            (vec![], false, false, false),
            (vec!["--timings"], false, true, false),
            (vec!["--no-slim"], true, false, false),
            (vec!["--no-slim", "--timings"], true, true, false),
            (vec!["--cold-boot"], false, false, true),
            (vec!["--cold-boot", "--timings"], false, true, true),
            (vec!["--no-slim", "--cold-boot"], true, false, true),
            (
                vec!["--no-slim", "--cold-boot", "--timings"],
                true,
                true,
                true,
            ),
        ] {
            let mut args = vec!["Test_AVD".to_owned()];
            args.extend(flags.into_iter().map(str::to_owned));
            let parsed = start_args(args).unwrap();
            assert_eq!(
                (parsed.2, parsed.3, parsed.4),
                (no_slim, timings, cold_boot)
            );
        }
        let parsed = start_args(vec![
            "Test_AVD".into(),
            "--timings".into(),
            "--cold-boot".into(),
            "--no-slim".into(),
        ])
        .unwrap();
        assert_eq!((parsed.2, parsed.3, parsed.4), (true, true, true));
    }

    #[test]
    fn startup_timing_formats_deterministic_phases() {
        let base = Instant::now();
        let timing = StartupTiming {
            launched: Some(base),
            console: Some(base + Duration::from_secs(2)),
            transport: Some(base + Duration::from_secs(8)),
            device: Some(base + Duration::from_secs(10)),
            boot: Some(base + Duration::from_secs(20)),
            ready: Some(base + Duration::from_secs(25)),
            ..StartupTiming::default()
        };
        assert_eq!(
            timing.format(),
            "launch→console 2.0s; console→ADB 6.0s; ADB→device 2.0s; device→boot 10.0s; boot→ready 5.0s; total 25.0s"
        );
        let timing = StartupTiming {
            transport: Some(base + Duration::from_secs(1)),
            console: Some(base + Duration::from_secs(2)),
            ..timing
        };
        assert!(timing.format().contains("console→ADB overlap"));
        assert!(timing.format().contains("total 25.0s"));
        let timing = StartupTiming {
            launched: Some(base),
            console: Some(base),
            transport: Some(base),
            ..StartupTiming::default()
        };
        assert!(timing
            .format()
            .contains("launch→console 0.0s; console→ADB 0.0s"));
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
        let mut timing = StartupTiming::default();
        assert!(wait_for_transport_with(
            "emulator-5558",
            Instant::now() + Duration::from_secs(5),
            move |_| Ok(source.lock().unwrap().pop_front().unwrap_or_default()),
            move || Ok(owner_source.lock().unwrap().pop_front().unwrap_or(None)),
            |_| Ok((true, true)),
            |_| {},
            &mut timing,
        )
        .unwrap());
        assert_eq!(timing.state.as_deref(), Some("device"));
        assert_eq!(owners.lock().unwrap().len(), 0);
    }

    #[test]
    fn timed_transport_wait_observes_console_concurrently() {
        let consoles = Arc::new(Mutex::new(VecDeque::from([false, false, true])));
        let probe = consoles.clone();
        let mut timing = StartupTiming {
            capture: true,
            ..StartupTiming::default()
        };
        assert!(wait_for_transport_with(
            "emulator-5558",
            Instant::now() + Duration::from_secs(5),
            |_| Ok(vec![device("emulator-5558", "device")]),
            || Ok(Some(true)),
            move |_| Ok((true, probe.lock().unwrap().pop_front().unwrap())),
            |_| {},
            &mut timing,
        )
        .unwrap());
        assert!(timing.console.is_some());
        assert!(timing.transport.is_some());
        assert!(timing.device.is_some());
        assert!(consoles.lock().unwrap().is_empty());
    }

    #[test]
    fn transport_wait_requires_console_match_without_timings() {
        let mut timing = StartupTiming::default();
        assert!(!wait_for_transport_with(
            "emulator-5558",
            Instant::now() + Duration::from_millis(2),
            |_| Ok(vec![device("emulator-5558", "device")]),
            || Ok(Some(true)),
            |_| Ok((true, false)),
            |_| {},
            &mut timing,
        )
        .unwrap());
        assert!(timing.console.is_none());
    }

    #[test]
    fn console_timeout_identifies_startup_phase() {
        let error = console_timeout_error("Fresh_API37");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("launch→console phase"));
    }

    #[test]
    fn transport_timeout_and_owner_collision_fail_closed() {
        let mut timing = StartupTiming::default();
        assert!(!wait_for_transport_with(
            "emulator-5558",
            Instant::now(),
            |_| Ok(Vec::new()),
            || Ok(None),
            |_| Ok((true, false)),
            |_| {},
            &mut timing,
        )
        .unwrap());
        assert!(wait_for_transport_with(
            "emulator-5558",
            Instant::now() + Duration::from_secs(5),
            |_| Ok(vec![device("emulator-5558", "device")]),
            || Ok(Some(false)),
            |_| Ok((true, false)),
            |_| {},
            &mut timing,
        )
        .is_err());
        assert!(wait_for_transport_with(
            "emulator-5558",
            Instant::now() + Duration::from_secs(5),
            |_| Ok(Vec::new()),
            || Ok(None),
            |_| Ok((false, false)),
            |_| {},
            &mut timing,
        )
        .is_err());
    }

    #[test]
    fn transport_timeout_diagnostic_distinguishes_state_and_server() {
        assert!(transport_timeout_error("emulator-5554", true, None, None)
            .to_string()
            .contains("transport is absent"));
        assert!(
            transport_timeout_error("emulator-5554", true, Some("offline"), None)
                .to_string()
                .contains("stayed offline")
        );
        assert!(
            transport_timeout_error("emulator-5554", true, None, Some("refused"))
                .to_string()
                .contains("ADB server unavailable")
        );
    }

    #[test]
    fn deadline_boot_wait_checks_immediately_and_respects_expiry() {
        let mut ready_called = false;
        let result = wait_for_boot_until(
            "emulator-5554",
            Instant::now() + Duration::from_secs(1),
            |_| Ok(true),
            || Ok(true),
            |_| panic!("ready must not sleep"),
            || ready_called = true,
        )
        .unwrap();
        assert_eq!(result, BootWait::Ready);
        assert!(ready_called);

        assert_eq!(
            wait_for_boot_until(
                "emulator-5554",
                Instant::now(),
                |_| panic!("expired deadline must not check"),
                || Ok(true),
                |_| {},
                || {},
            )
            .unwrap(),
            BootWait::TimedOut
        );
    }

    #[test]
    fn transport_wait_handles_late_server_and_offline_to_device() {
        let snapshots = Arc::new(Mutex::new(VecDeque::from([
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "server not started",
            )),
            Ok(Vec::new()),
            Ok(vec![device("emulator-5554", "offline")]),
            Ok(vec![device("emulator-5554", "device")]),
        ])));
        let source = snapshots.clone();
        let mut timing = StartupTiming {
            capture: true,
            ..StartupTiming::default()
        };
        assert!(wait_for_transport_with(
            "emulator-5554",
            Instant::now() + Duration::from_secs(1),
            move |_| source.lock().unwrap().pop_front().unwrap(),
            || Ok(Some(true)),
            |_| Ok((true, true)),
            |_| {},
            &mut timing,
        )
        .unwrap());
        assert_eq!(timing.state.as_deref(), Some("device"));
        assert!(timing.transport.is_some());
        assert!(timing.device.is_some());
    }

    #[test]
    fn boot_wait_handles_incomplete_then_ready() {
        let checks = Arc::new(Mutex::new(VecDeque::from([Ok(false), Ok(true)])));
        let source = checks.clone();
        let mut ready_called = false;
        assert_eq!(
            wait_for_boot_until(
                "emulator-5554",
                Instant::now() + Duration::from_secs(1),
                move |_| source.lock().unwrap().pop_front().unwrap(),
                || Ok(true),
                |_| {},
                || ready_called = true,
            )
            .unwrap(),
            BootWait::Ready
        );
        assert!(ready_called);
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
