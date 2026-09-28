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
    let devices = adb::track::devices(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037))?;
    if known_physical_serial(target, by_serial, &devices) {
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
            if same_avd_name(&name, target) {
                verify_identity(port, &format!("emulator-{port}"))?;
                matches.push((port, owner, name));
            }
        }
        matches
    };
    let selected = match select_candidate(candidates, by_serial, target)? {
        Some(selected) => selected,
        None => {
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
    };
    let (port, _owner, name) = selected;
    avd::console::shutdown_until(port, Instant::now() + Duration::from_secs(3))?;
    wait_until_closed(port, Instant::now() + Duration::from_secs(10), || {
        platform::console_owner_pid(port)
    })?;
    println!("Stopped emulator-{port} ({name}).");
    Ok(())
}

type Candidate = (u16, u32, String);

fn known_physical_serial(
    target: &str,
    emulator_serial: bool,
    devices: &[adb::track::DeviceState],
) -> bool {
    !emulator_serial && devices.iter().any(|device| device.serial == target)
}

fn same_avd_name(actual: &str, requested: &str) -> bool {
    actual == requested
}

fn select_candidate(
    candidates: Vec<Candidate>,
    by_serial: bool,
    target: &str,
) -> io::Result<Option<Candidate>> {
    match candidates.as_slice() {
        [] if by_serial => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{target} is not a running emulator"),
        )),
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "multiple running emulators report AVD name {target:?}; refusing ambiguous stop"
            ),
        )),
    }
}

fn wait_until_closed(
    port: u16,
    deadline: Instant,
    mut owner: impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<()> {
    loop {
        if owner()?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("emulator-{port} console still owns port after shutdown timeout"),
            ));
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_known_physical_device_serial() {
        let devices = [adb::track::DeviceState {
            serial: "R58M123456A".into(),
            state: "device".into(),
        }];
        assert!(known_physical_serial("R58M123456A", false, &devices));
        assert!(!known_physical_serial("emulator-5554", true, &devices));
        assert!(!known_physical_serial("other", false, &devices));
    }

    #[test]
    fn avd_match_is_exact_and_unique() {
        assert!(same_avd_name("Pixel_9_API_36", "Pixel_9_API_36"));
        assert!(!same_avd_name("Pixel_9_API_36_copy", "Pixel_9_API_36"));
        let one = (5554, 12, "Pixel".to_owned());
        assert_eq!(
            select_candidate(vec![one.clone()], false, "Pixel").unwrap(),
            Some(one.clone())
        );
        assert!(select_candidate(vec![], true, "emulator-5554").is_err());
        assert!(select_candidate(vec![], false, "Pixel").unwrap().is_none());
        assert!(select_candidate(
            vec![one.clone(), (5556, 13, "Pixel".into())],
            false,
            "Pixel"
        )
        .is_err());
    }

    #[test]
    fn shutdown_wait_requires_console_disappearance() {
        let mut owners = [Some(12), None].into_iter();
        wait_until_closed(5554, Instant::now() + Duration::from_secs(1), || {
            Ok(owners.next().flatten())
        })
        .unwrap();
        let error = wait_until_closed(5554, Instant::now() + Duration::from_millis(1), || {
            Ok(Some(12))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
