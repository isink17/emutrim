use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_nodelay(true)?;
    Ok(stream)
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
}
