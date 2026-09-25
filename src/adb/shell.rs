use crate::adb::protocol::{connect, send_service};
use std::io::{self, Read};
use std::net::SocketAddr;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceMetadata {
    pub model: String,
    pub android_version: String,
    pub api_level: String,
}

pub fn shell(addr: SocketAddr, serial: &str, command: &str) -> io::Result<String> {
    let mut stream = connect(addr)?;
    send_service(&mut stream, &format!("host:transport:{serial}"))?;
    send_service(&mut stream, &format!("shell:{command}"))?;

    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

#[derive(Debug, Default)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: u32,
}

// shell,v2 reports the remote exit status. Mutations must not use legacy
// shell: it closes the socket without exposing whether the command failed.
pub fn shell_v2(addr: SocketAddr, serial: &str, command: &str) -> io::Result<ShellOutput> {
    let mut stream = connect(addr)?;
    send_service(&mut stream, &format!("host:transport:{serial}"))?;
    send_service(&mut stream, &format!("shell,v2,raw:{command}"))?;
    let mut out = ShellOutput::default();
    loop {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload)?;
        match header[0] {
            1 => out.stdout.push_str(&String::from_utf8_lossy(&payload)),
            2 => out.stderr.push_str(&String::from_utf8_lossy(&payload)),
            3 if len == 1 => {
                out.status = u32::from(payload[0]);
                return Ok(out);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid ADB shell-v2 packet",
                ))
            }
        }
    }
}

pub fn getprop(addr: SocketAddr, serial: &str, key: &str) -> io::Result<String> {
    shell(addr, serial, &format!("getprop {key}"))
}

pub fn metadata(addr: SocketAddr, serial: &str) -> io::Result<DeviceMetadata> {
    Ok(DeviceMetadata {
        model: getprop(addr, serial, "ro.product.model")?,
        android_version: getprop(addr, serial, "ro.build.version.release")?,
        api_level: getprop(addr, serial, "ro.build.version.sdk")?,
    })
}

pub fn boot_completed(addr: SocketAddr, serial: &str) -> io::Result<bool> {
    Ok(getprop(addr, serial, "sys.boot_completed")? == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_output_defaults_to_success() {
        assert_eq!(ShellOutput::default().status, 0);
    }
}
