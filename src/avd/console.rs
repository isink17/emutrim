use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "emulator console deadline expired",
        ))
    } else {
        Ok(remaining)
    }
}

fn response(reader: &mut BufReader<TcpStream>, deadline: Instant) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut total = 0usize;
    loop {
        reader
            .get_mut()
            .set_read_timeout(Some(remaining(deadline)?))?;
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "emulator console closed before response completed",
            ));
        }
        total += read;
        if total > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "emulator console response exceeds 64 KiB",
            ));
        }
        let line = line.trim().to_owned();
        if line == "OK" {
            return Ok(lines);
        }
        if line.starts_with("KO:") {
            return Err(io::Error::other(line));
        }
        lines.push(line);
    }
}

fn token_path() -> io::Result<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .map(|home| home.join(".emulator_console_auth_token"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user home unavailable"))
}

fn parse_avd_name(lines: Vec<String>) -> io::Result<String> {
    lines
        .into_iter()
        .rev()
        .find(|line| !line.is_empty())
        .filter(|name| !name.contains('\r') && !name.contains('\n'))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "AVD name response is empty or malformed",
            )
        })
}

pub fn avd_name(port: u16) -> io::Result<String> {
    avd_name_until(port, Instant::now() + Duration::from_secs(2))
}

pub fn avd_name_until(port: u16, deadline: Instant) -> io::Result<String> {
    parse_avd_name(command_until(port, "avd name", deadline)?)
}

pub fn shutdown_until(port: u16, deadline: Instant) -> io::Result<()> {
    let _ = command_until(port, "kill", deadline)?;
    Ok(())
}

fn command_until(port: u16, command: &str, deadline: Instant) -> io::Result<Vec<String>> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)?;
    stream.set_read_timeout(Some(remaining(deadline)?))?;
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    let mut reader = BufReader::new(stream);
    let greeting = response(&mut reader, deadline)?;
    if greeting
        .iter()
        .any(|line| line.contains("Authentication required"))
    {
        let token = std::fs::read_to_string(token_path()?)?;
        let token = token.trim();
        if token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "emulator console authentication token is empty",
            ));
        }
        reader
            .get_mut()
            .set_write_timeout(Some(remaining(deadline)?))?;
        writeln!(reader.get_mut(), "auth {token}")?;
        let _ = response(&mut reader, deadline)?;
    }
    reader
        .get_mut()
        .set_write_timeout(Some(remaining(deadline)?))?;
    writeln!(reader.get_mut(), "{command}")?;
    response(&mut reader, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_avd_name() {
        assert_eq!(
            parse_avd_name(vec!["Pixel_API_37".into()]).unwrap(),
            "Pixel_API_37"
        );
        assert!(parse_avd_name(Vec::new()).is_err());
    }
}
