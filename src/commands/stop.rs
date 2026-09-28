use crate::{adb, avd, platform};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

const FIRST_PORT: u16 = 5554;
const LAST_PORT: u16 = 5682;

pub fn run(args: Vec<String>) -> io::Result<()> {
    if args.len() != 1 || args[0].starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: stop <SERIAL|AVD>",
        ));
    }
    let target = &args[0];
    let by_serial = avd::console_port(target).is_some();
    if !by_serial
        && adb::track::devices(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037))?
            .iter()
            .any(|device| device.serial == *target)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{target} is an ADB device serial, not an emulator console serial; refusing stop"
            ),
        ));
    }
    let candidates = if by_serial {
        let port = avd::console_port(target).unwrap();
        let owner = platform::console_owner_pid(port)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no emulator console owns {target}"),
            )
        })?;
        let name = avd::console::avd_name(port)?;
        verify_identity(port, target)?;
        vec![(port, owner, name)]
    } else {
        let mut matches = Vec::new();
        for port in (FIRST_PORT..=LAST_PORT).step_by(2) {
            let Some(owner) = platform::console_owner_pid(port)? else {
                continue;
            };
            let name = avd::console::avd_name(port).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot authenticate console on {port}: {error}"),
                )
            })?;
            if name == *target {
                verify_identity(port, &format!("emulator-{port}"))?;
                matches.push((port, owner, name));
            }
        }
        matches
    };
    let selected = match candidates.as_slice() {
        [] if by_serial => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{target} is not a running emulator"),
            ))
        }
        [] => {
            if avd::config_path_mode(target, false).is_err()
                && avd::config_path_mode(target, true).is_err()
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("AVD {target:?} not found"),
                ));
            }
            println!("{target} is already stopped.");
            return Ok(());
        }
        [one] => one.clone(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                "multiple running emulators report AVD name {target:?}; refusing ambiguous stop"
            ),
            ))
        }
    };
    let (port, _owner, name) = selected;
    avd::console::shutdown_until(port, Instant::now() + Duration::from_secs(3))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if platform::console_owner_pid(port)?.is_none() {
            println!("Stopped emulator-{port} ({name}).");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("emulator-{port} ({name}) console still owns port after shutdown timeout"),
    ))
}

fn verify_identity(port: u16, serial: &str) -> io::Result<()> {
    if avd::console_port(serial) != Some(port) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("console port {port} does not match exact serial {serial}"),
        ));
    }
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037);
    let devices = adb::track::devices(addr)?;
    if let Some(device) = devices.iter().find(|device| device.serial == serial) {
        if device.state == "device" {
            let qemu = adb::shell::shell_v2(addr, serial, "getprop ro.kernel.qemu")?;
            if qemu.status != 0 || qemu.stdout.trim() != "1" {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{serial} does not prove emulator identity (ro.kernel.qemu != 1)"),
                ));
            }
        } else if device.state != "offline" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{serial} transport state {:?} does not prove a stoppable emulator",
                    device.state
                ),
            ));
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("exact ADB transport {serial} not found; refusing console-only stop"),
        ));
    }
    Ok(())
}
