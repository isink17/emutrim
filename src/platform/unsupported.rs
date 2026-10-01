use super::ProcessStats;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub fn tcp_listener_image(_port: u16) -> io::Result<Option<(u32, PathBuf)>> {
    Ok(None)
}

pub fn console_owner_pid(_port: u16) -> io::Result<Option<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-to-console identity verification is unsupported on this host",
    ))
}

pub struct ProcessWatch;

impl ProcessWatch {
    pub fn open(_pid: u32) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process liveness checks are unsupported on this host",
        ))
    }

    pub fn is_alive(&self) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process liveness checks are unsupported on this host",
        ))
    }
}

pub fn process_parent_pid(_pid: u32) -> io::Result<Option<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process parent lookup is unsupported on this host",
    ))
}

pub fn shutdown_helper_pids(_pid: u32) -> io::Result<Vec<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Emulator shutdown helper lookup is unsupported on this host",
    ))
}

pub fn terminate_launch_descendants(_launch_pid: u32, _timeout: Duration) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch descendant cleanup is unsupported on this host",
    ))
}

pub fn belongs_to_launch(_owner_pid: u32, _launch_pid: u32) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-to-console identity verification is unsupported on this host",
    ))
}

pub fn process_stats(_pid: u32) -> io::Result<ProcessStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native process stats are currently supported on Windows only",
    ))
}
