use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    connect_with_timeout(addr, Duration::from_secs(3))
}

pub fn connect_with_timeout(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "ADB deadline expired",
        ));
    }
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

pub fn connect_until(addr: SocketAddr, deadline: Instant) -> io::Result<TcpStream> {
    connect_with_timeout(addr, remaining_until(deadline)?)
}

pub fn remaining_until(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "startup deadline expired",
        ))
    } else {
        Ok(remaining)
    }
}

pub fn encode_request(service: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", service.len()).into_bytes();
    out.extend_from_slice(service.as_bytes());
    out
}

pub fn send_service(stream: &mut TcpStream, service: &str) -> io::Result<()> {
    stream.write_all(&encode_request(service))?;
    stream.flush()?;
    read_status(stream)
}

pub fn send_service_until(
    stream: &mut TcpStream,
    service: &str,
    deadline: Instant,
) -> io::Result<()> {
    stream.set_write_timeout(Some(remaining_until(deadline)?))?;
    stream.write_all(&encode_request(service))?;
    stream.flush()?;
    stream.set_read_timeout(Some(remaining_until(deadline)?))?;
    read_status(stream)
}

pub fn host_protocol_version(addr: SocketAddr) -> io::Result<u32> {
    let mut stream = connect(addr)?;
    send_service(&mut stream, "host:version")?;
    parse_host_protocol_version(&read_length_prefixed(&mut stream)?)
}

fn parse_host_protocol_version(payload: &[u8]) -> io::Result<u32> {
    let version = std::str::from_utf8(payload)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB version is not UTF-8"))?;
    u32::from_str_radix(version, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB version is not hex"))
}

pub fn read_status<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut status = [0u8; 4];
    reader.read_exact(&mut status)?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let message = read_length_prefixed(reader)?;
            Err(io::Error::other(format!(
                "ADB server rejected request: {}",
                String::from_utf8_lossy(&message)
            )))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected ADB status: {:?}",
                String::from_utf8_lossy(other)
            ),
        )),
    }
}

pub fn read_length_prefixed<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB length is not UTF-8 hex"))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB length is not valid hex"))?;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub fn read_length_prefixed_until(
    stream: &mut TcpStream,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(remaining_until(deadline)?))?;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB length is not UTF-8 hex"))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ADB length is not valid hex"))?;
    let mut payload = vec![0u8; len];
    stream.set_read_timeout(Some(remaining_until(deadline)?))?;
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_is_hex_length_prefixed() {
        assert_eq!(
            encode_request("host:track-devices"),
            b"0012host:track-devices"
        );
    }

    #[test]
    fn parses_length_prefixed_payload() {
        let mut input = Cursor::new(b"0005hello".to_vec());
        assert_eq!(read_length_prefixed(&mut input).unwrap(), b"hello");
    }

    #[test]
    fn accepts_okay() {
        let mut input = Cursor::new(b"OKAY".to_vec());
        assert!(read_status(&mut input).is_ok());
    }

    #[test]
    fn reports_fail_body() {
        let mut input = Cursor::new(b"FAIL0004nope".to_vec());
        let err = read_status(&mut input).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn parses_host_protocol_version() {
        assert_eq!(parse_host_protocol_version(b"0029").unwrap(), 41);
        assert!(parse_host_protocol_version(b"bad!").is_err());
    }

    #[test]
    fn deadline_remaining_is_bounded_and_expiry_is_reported() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let remaining = remaining_until(deadline).unwrap();
        assert!(!remaining.is_zero());
        assert!(remaining <= Duration::from_secs(2));
        assert_eq!(
            remaining_until(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            connect_with_timeout("127.0.0.1:5037".parse().unwrap(), Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
