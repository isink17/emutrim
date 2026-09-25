use crate::adb::protocol::{connect, read_length_prefixed, send_service};
use std::collections::BTreeMap;
use std::io;
use std::net::{SocketAddr, TcpStream};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceState {
    pub serial: String,
    pub state: String,
}

pub struct Tracker {
    stream: TcpStream,
}

impl Tracker {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let mut stream = connect(addr)?;
        send_service(&mut stream, "host:track-devices")?;
        Ok(Self { stream })
    }

    pub fn next_snapshot(&mut self) -> io::Result<Vec<DeviceState>> {
        let payload = read_length_prefixed(&mut self.stream)?;
        parse_snapshot(&payload)
    }
}

pub fn devices(addr: SocketAddr) -> io::Result<Vec<DeviceState>> {
    let mut stream = connect(addr)?;
    send_service(&mut stream, "host:devices-l")?;
    parse_snapshot(&read_length_prefixed(&mut stream)?)
}

pub fn parse_snapshot(payload: &[u8]) -> io::Result<Vec<DeviceState>> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "device list is not UTF-8"))?;

    let mut devices = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let serial = parts.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "device line has no serial")
        })?;
        let state = parts.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "device line has no state")
        })?;
        devices.push(DeviceState {
            serial: serial.to_owned(),
            state: state.to_owned(),
        });
    }
    Ok(devices)
}

pub fn as_map(snapshot: &[DeviceState]) -> BTreeMap<String, String> {
    snapshot
        .iter()
        .map(|d| (d.serial.clone(), d.state.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::test_support::FakeAdb;

    #[test]
    fn parses_emulator_snapshot() {
        let got = parse_snapshot(b"emulator-5554\tdevice\n0123ABC\toffline\n").unwrap();
        assert_eq!(
            got,
            vec![
                DeviceState {
                    serial: "emulator-5554".into(),
                    state: "device".into()
                },
                DeviceState {
                    serial: "0123ABC".into(),
                    state: "offline".into()
                },
            ]
        );
    }

    #[test]
    fn empty_snapshot_means_no_devices() {
        assert!(parse_snapshot(b"").unwrap().is_empty());
    }

    #[test]
    fn tracker_uses_loopback_smart_socket_service() {
        let server = FakeAdb::start(|_| Vec::new());
        let mut tracker = Tracker::connect(server.addr()).unwrap();
        assert!(tracker.next_snapshot().unwrap().is_empty());
    }

    #[test]
    fn devices_reads_single_smart_socket_snapshot() {
        let server = FakeAdb::start_with_devices("emulator-5556\toffline\n", |_, _| Vec::new());
        assert_eq!(
            devices(server.addr()).unwrap(),
            vec![DeviceState {
                serial: "emulator-5556".into(),
                state: "offline".into()
            }]
        );
    }
}
