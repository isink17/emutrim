use crate::adb::protocol::{
    connect, connect_until, read_length_prefixed, read_length_prefixed_until, send_service,
    send_service_until,
};
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

pub fn devices_with_timeout(
    addr: SocketAddr,
    deadline: std::time::Instant,
) -> io::Result<Vec<DeviceState>> {
    let mut stream = connect_until(addr, deadline)?;
    send_service_until(&mut stream, "host:devices-l", deadline)?;
    parse_snapshot(&read_length_prefixed_until(&mut stream, deadline)?)
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
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

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

    #[test]
    fn device_snapshot_read_obeys_startup_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let len = usize::from_str_radix(std::str::from_utf8(&header).unwrap(), 16).unwrap();
            let mut request = vec![0; len];
            stream.read_exact(&mut request).unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        let result = devices_with_timeout(addr, Instant::now() + Duration::from_millis(50));
        assert!(matches!(
            result.unwrap_err().kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        server.join().unwrap();
    }
}
