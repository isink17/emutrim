use crate::adb::protocol::remaining_until;
use crate::adb::shell::shell_v2_with_timeout;
use crate::{adb, avd, platform, slim};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct VerifiedEmulator {
    pub name: String,
    pub serial: String,
    pub port: u16,
    pub launch_pid: u32,
    pub timing: crate::StartupTiming,
}

pub(crate) fn launch_and_wait(
    name: &str,
    requested_ram: Option<u32>,
    snapshot: avd::SnapshotMode,
    headless: bool,
    managed: bool,
    verify_process: bool,
    timings: bool,
) -> io::Result<VerifiedEmulator> {
    let info = avd::inspect_mode(name, managed)?;
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
    if verify_process && !platform::supports_verified_integrated_start() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "integrated launch requires verified process-to-console identity support",
        ));
    }
    let port = avd::available_console_port()?;
    let serial = format!("emulator-{port}");
    let mut child = avd::start_mode(name, ram, port, snapshot, headless, managed)?;
    let launch_pid = child.id();
    let mut timing = crate::StartupTiming {
        capture: timings,
        launched: Some(Instant::now()),
        ..crate::StartupTiming::default()
    };
    println!("started {name} as {serial}");
    println!("waiting for ADB transport...");
    let deadline = Instant::now() + Duration::from_secs(120);
    let ready = crate::wait_for_transport_with(
        &serial,
        deadline,
        |deadline| adb::track::devices_with_timeout(crate::default_adb_addr(), deadline),
        || {
            if !verify_process {
                return Ok(Some(true));
            }
            match platform::console_owner_pid(port)? {
                Some(owner) => Ok(Some(platform::belongs_to_launch(owner, launch_pid)?)),
                None => Ok(None),
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
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for authenticated console for {name}"),
        ));
    }
    if !ready {
        if timings {
            eprintln!("startup timing: {}", timing.format());
        }
        return Err(crate::transport_timeout_error(
            &serial,
            child.try_wait()?.is_none(),
            timing.state.as_deref(),
            timing.error.as_deref(),
        ));
    }
    let qemu = shell_v2_with_timeout(
        crate::default_adb_addr(),
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

    println!("waiting for Android boot...");
    let deadline = Instant::now() + Duration::from_secs(120);
    let boot = crate::wait_for_boot_until(
        &serial,
        deadline,
        |deadline| {
            let ready = crate::adb::shell::boot_completed_with_timeout(
                crate::default_adb_addr(),
                &serial,
                deadline,
            );
            if matches!(&ready, Ok(true)) {
                timing.boot = Some(Instant::now());
            }
            ready
        },
        || child.try_wait().map(|status| status.is_none()),
        |remaining| thread::sleep(remaining.min(Duration::from_millis(500))),
        || {},
    )?;
    match boot {
        crate::BootWait::Ready => {}
        crate::BootWait::TimedOut => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Android boot timed out on {serial}; guest was not modified"),
            ));
        }
        crate::BootWait::ProcessExited => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("emulator process exited before Android boot completed on {serial}"),
            ));
        }
    }
    Ok(VerifiedEmulator {
        name: name.into(),
        serial,
        port,
        launch_pid,
        timing,
    })
}
