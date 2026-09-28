use crate::{adb, avd, managed, slim};
use serde::Serialize;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Serialize)]
pub struct Status {
    serial: String,
    transport: String,
    identity: String,
    avd_name: Option<String>,
    qemu: Option<bool>,
    boot_completed: Option<bool>,
    slim_state: String,
    ownership: String,
}

pub fn inspect(serial: &str) -> io::Result<Status> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037);
    let devices = adb::track::devices(addr)?;
    let state = devices
        .iter()
        .find(|device| device.serial == serial)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("ADB target {serial} not found"),
            )
        })?;
    let online = state.state == "device";
    let emulator_serial = avd::console_port(serial).is_some();
    let mut status = Status {
        serial: serial.into(),
        transport: match state.state.as_str() {
            "device" | "offline" | "unauthorized" => state.state.clone(),
            _ => "other".into(),
        },
        identity: if emulator_serial {
            "unknown"
        } else {
            "physical"
        }
        .into(),
        avd_name: None,
        qemu: None,
        boot_completed: None,
        slim_state: "unavailable".into(),
        ownership: "unknown".into(),
    };
    if !online {
        if emulator_serial && state.state == "offline" {
            let port = avd::console_port(serial).expect("validated emulator serial");
            if platform_owner(port).is_some() {
                if let Ok(name) = avd::console::avd_name(port) {
                    status.identity = "emulator".into();
                    status.avd_name = Some(name.clone());
                    status.ownership = ownership(&name);
                }
            }
        }
        return Ok(status);
    }
    if !emulator_serial {
        status.identity = "physical".into();
        return Ok(status);
    }
    let qemu = adb::shell::shell_v2(addr, serial, "getprop ro.kernel.qemu")
        .ok()
        .filter(|output| output.status == 0)
        .map(|output| output.stdout.trim() == "1");
    status.qemu = qemu;
    if qemu != Some(true) {
        status.identity = "unknown".into();
        return Ok(status);
    }
    status.identity = "emulator".into();
    let boot = adb::shell::boot_completed(addr, serial).ok();
    status.boot_completed = boot;
    let port = avd::console_port(serial).expect("validated emulator serial");
    if let Ok(name) = avd::console::avd_name(port) {
        status.avd_name = Some(name.clone());
        status.ownership = ownership(&name);
    }
    status.slim_state = match slim::inspect_state(addr, serial) {
        Ok(None) => "absent",
        Ok(Some(_))
            if boot == Some(true)
                && slim::already_applied(addr, serial, &slim::Options::default())
                    .unwrap_or(false) =>
        {
            "applied"
        }
        Ok(Some(_)) => "partial",
        Err(error) if error.kind() == io::ErrorKind::InvalidData => "malformed",
        Err(_) => "unavailable",
    }
    .into();
    Ok(status)
}

fn platform_owner(port: u16) -> Option<u32> {
    crate::platform::console_owner_pid(port).ok().flatten()
}

pub fn ownership(name: &str) -> String {
    let Ok(layout) = managed::Layout::resolve() else {
        return "unknown".into();
    };
    let Ok(managed_config) = avd::config_path_mode(name, true) else {
        return if avd::config_path_mode(name, false).is_ok() {
            "external"
        } else {
            "unknown"
        }
        .into();
    };
    let (Ok(base), Ok(path)) = (layout.avd.canonicalize(), managed_config.canonicalize()) else {
        return "unknown".into();
    };
    if !path.starts_with(base) {
        return "unknown".into();
    }
    let manifest = std::fs::read_to_string(layout.manifest);
    let Ok(manifest) = manifest else {
        return "unknown".into();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) else {
        return "unknown".into();
    };
    if value
        .get("avds")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|names| names.iter().any(|item| item.as_str() == Some(name)))
    {
        "managed".into()
    } else {
        "unknown".into()
    }
}

pub fn run(args: Vec<String>) -> io::Result<()> {
    if args.len() != 1 || args[0].starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: status SERIAL",
        ));
    }
    let status = inspect(&args[0])?;
    println!(
        "Serial: {}\nTransport: {}\nIdentity: {}",
        status.serial, status.transport, status.identity
    );
    if let Some(name) = &status.avd_name {
        println!("AVD: {name}");
    }
    if let Some(qemu) = status.qemu {
        println!("QEMU: {}", if qemu { "yes" } else { "no" });
    }
    if let Some(boot) = status.boot_completed {
        println!("Boot: {}", if boot { "complete" } else { "incomplete" });
    }
    println!(
        "Slim state: {}\nOwnership: {}",
        status.slim_state, status.ownership
    );
    Ok(())
}

pub fn print_json_result(args: &[String]) -> io::Result<()> {
    if args.len() != 1 || args[0].starts_with('-') {
        let error = io::Error::new(io::ErrorKind::InvalidInput, "usage: status SERIAL [--json]");
        crate::output::failure("invalid_arguments", &error.to_string());
        return Err(error);
    }
    match inspect(&args[0]) {
        Ok(status) => crate::output::success(status),
        Err(error) => {
            crate::output::failure(crate::output::code_for(&error), &error.to_string());
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_status_fields_serialize_as_null() {
        let value = serde_json::to_value(Status {
            serial: "emulator-5554".into(),
            transport: "offline".into(),
            identity: "unknown".into(),
            avd_name: None,
            qemu: None,
            boot_completed: None,
            slim_state: "unavailable".into(),
            ownership: "unknown".into(),
        })
        .unwrap();
        assert!(value["avd_name"].is_null());
        assert!(value["qemu"].is_null());
        assert!(value["boot_completed"].is_null());
    }
}
