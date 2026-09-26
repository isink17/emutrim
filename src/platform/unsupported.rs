use super::ProcessStats;
use std::io;
use std::path::PathBuf;

pub fn tcp_listener_image(_port: u16) -> io::Result<Option<(u32, PathBuf)>> {
    Ok(None)
}

pub fn console_owner_pid(_port: u16) -> io::Result<Option<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native emulator process mapping is currently supported on Windows only",
    ))
}

pub fn belongs_to_launch(_owner_pid: u32, _launch_pid: u32) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native emulator process mapping is currently supported on Windows only",
    ))
}

pub fn process_stats(_pid: u32) -> io::Result<ProcessStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native process stats are currently supported on Windows only",
    ))
}
