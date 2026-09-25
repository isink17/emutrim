use crate::adb::protocol::{connect, send_service};
use std::io::{self, Read};
use std::net::SocketAddr;

const MAX_SHELL_OUTPUT: usize = 16 * 1024 * 1024;

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
    let mut received = 0usize;
    loop {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        if received
            .checked_add(len)
            .is_none_or(|size| size > MAX_SHELL_OUTPUT)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ADB shell-v2 output exceeds 16 MiB",
            ));
        }
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload)?;
        received += len;
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
    let output = shell_v2(addr, serial, "getprop sys.boot_completed")?;
    if output.status != 0 {
        return Err(io::Error::other(format!(
            "boot check failed ({}): {}",
            output.status,
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::test_support::{frame, FakeAdb};

    #[test]
    fn shell_v2_reads_multiple_output_frames_and_exit_status() {
        let server =
            FakeAdb::start(|_| [frame(1, b"out"), frame(2, b"warn"), frame(3, &[0])].concat());
        let out = shell_v2(server.addr(), "emulator-5554", "true").unwrap();
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            ("out", "warn", 0)
        );
    }

    #[test]
    fn shell_v2_returns_nonzero_status_and_stderr() {
        let server = FakeAdb::start(|_| [frame(2, b"denied"), frame(3, &[7])].concat());
        let out = shell_v2(server.addr(), "emulator-5554", "false").unwrap();
        assert_eq!((out.status, out.stderr.as_str()), (7, "denied"));
    }

    #[test]
    fn boot_check_uses_shell_v2_and_preserves_transport_errors() {
        let server = FakeAdb::start(|command| {
            assert_eq!(command, "getprop sys.boot_completed");
            [frame(1, b"1\n"), frame(3, &[0])].concat()
        });
        assert!(boot_completed(server.addr(), "emulator-5556").unwrap());

        let server = FakeAdb::start(|_| [frame(2, b"offline"), frame(3, &[1])].concat());
        assert!(boot_completed(server.addr(), "emulator-5556").is_err());
    }

    #[test]
    fn shell_v2_rejects_malformed_or_truncated_frames() {
        for bytes in [
            vec![1, 0, 0],
            vec![1, 4, 0, 0, 0, b'x'],
            frame(1, b"stdout before disconnect"),
            frame(9, b""),
            frame(3, b"bad"),
            vec![1, 0, 0, 0, 1],
        ] {
            let server = FakeAdb::start(move |_| bytes.clone());
            assert!(shell_v2(server.addr(), "emulator-5554", "test").is_err());
        }
    }

    #[test]
    fn shell_v2_rejects_oversized_frame_before_allocating() {
        let server = FakeAdb::start(|_| [1, 1, 0, 0, 1].to_vec());
        let err = shell_v2(server.addr(), "emulator-5554", "test").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn shell_v2_surfaces_transport_rejection_and_disconnect() {
        for server in [
            FakeAdb::rejecting_transport(),
            FakeAdb::closing_after_transport(),
        ] {
            assert!(shell_v2(server.addr(), "emulator-5554", "test").is_err());
        }
    }
}
