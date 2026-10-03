use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const LSOF: &str = "/usr/sbin/lsof";
const PS: &str = "/bin/ps";
const MAX_ANCESTRY_DEPTH: usize = 256;

pub fn console_owner_pid(port: u16) -> io::Result<Option<u32>> {
    let port_arg = format!("-iTCP:{port}");
    let output = Command::new(LSOF)
        .args(["-nP", "-a", &port_arg, "-sTCP:LISTEN", "-t"])
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("required system utility {LSOF} is unavailable"),
                )
            } else {
                error
            }
        })?;
    if output.stderr.is_empty()
        && (output.status.success()
            || (output.status.code() == Some(1) && output.stdout.is_empty()))
    {
        return parse_lsof_pids(&output.stdout);
    }
    Err(io::Error::other(format!(
        "{LSOF} failed with status {}",
        output.status
    )))
}

pub struct ProcessWatch {
    pid: u32,
    start_time: String,
}

impl ProcessWatch {
    pub fn open(pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            start_time: process_start_time(pid)?,
        })
    }

    pub fn is_alive(&self) -> io::Result<bool> {
        match process_start_time(self.pid) {
            Ok(start_time) => Ok(start_time == self.start_time),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn process_start_time(pid: u32) -> io::Result<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process PID",
        ));
    }
    let pid_arg = pid.to_string();
    let output = Command::new(PS)
        .args(["-o", "lstart=", "-p", &pid_arg])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} unavailable"),
        ));
    }
    parse_process_start_time(&output.stdout, pid)
}

fn parse_process_start_time(output: &[u8], pid: u32) -> io::Result<String> {
    let text = std::str::from_utf8(output)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ps start time is not UTF-8"))?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let start_time = lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} unavailable"),
        )
    })?;
    if lines.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed ps start time output",
        ));
    }
    Ok(start_time.to_owned())
}

fn parse_lsof_pids(output: &[u8]) -> io::Result<Option<u32>> {
    let text = std::str::from_utf8(output)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "lsof output is not UTF-8"))?;
    let mut pids = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed lsof PID output",
            ));
        }
        let pid = line
            .parse::<u32>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid lsof PID"))?;
        if pid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "lsof returned invalid PID 0",
            ));
        }
        pids.push(pid);
    }
    pids.sort_unstable();
    pids.dedup();
    match pids.as_slice() {
        [] => Ok(None),
        [pid] => Ok(Some(*pid)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TCP listener has multiple process owners",
        )),
    }
}

fn parent_pid(pid: u32) -> io::Result<u32> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process PID",
        ));
    }
    let pid_arg = pid.to_string();
    let output = Command::new(PS)
        .args(["-o", "ppid=", "-p", &pid_arg])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} unavailable"),
        ));
    }
    parse_parent_pid(&output.stdout, pid)
}

pub fn process_parent_pid(pid: u32) -> io::Result<Option<u32>> {
    let parent = parent_pid(pid)?;
    Ok((parent != 0 && parent != pid).then_some(parent))
}

pub fn shutdown_helper_pids(pid: u32) -> io::Result<Vec<u32>> {
    let output = Command::new(PS).args(["-axo", "pid=,command="]).output()?;
    if !output.status.success() {
        return Err(io::Error::other("failed to inspect Emulator processes"));
    }
    parse_shutdown_helper_pids(&output.stdout, pid)
}

fn parse_shutdown_helper_pids(output: &[u8], target_pid: u32) -> io::Result<Vec<u32>> {
    let text = std::str::from_utf8(output)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ps output is not UTF-8"))?;
    let target = target_pid.to_string();
    let mut pids = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Some((pid, command)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let pid = pid
            .parse::<u32>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process PID"))?;
        if pid != 0
            && PathBuf::from(command.split_whitespace().next().unwrap_or_default())
                .file_name()
                .is_some_and(|name| name == "emulator")
            && command
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|args| args == ["-kill", target.as_str()])
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn parse_parent_pid(output: &[u8], pid: u32) -> io::Result<u32> {
    let text = std::str::from_utf8(output)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ps parent PID is not UTF-8"))?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let value = lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} unavailable"),
        )
    })?;
    if lines.next().is_some() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed ps parent PID output",
        ));
    }
    value
        .parse::<u32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ps parent PID"))
}

pub fn terminate_launch_descendants(_launch_pid: u32, _timeout: Duration) -> io::Result<()> {
    // Unix reaping can release the launch PID, so descendants cannot be pinned safely here.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch descendant cleanup is unsupported on macOS",
    ))
}

pub fn belongs_to_launch(owner_pid: u32, launch_pid: u32) -> io::Result<bool> {
    belongs_to_launch_with(owner_pid, launch_pid, parent_pid)
}

fn belongs_to_launch_with(
    owner_pid: u32,
    launch_pid: u32,
    mut parent: impl FnMut(u32) -> io::Result<u32>,
) -> io::Result<bool> {
    if owner_pid == 0 || launch_pid == 0 {
        return Ok(false);
    }
    let mut current = owner_pid;
    let mut visited = HashSet::new();
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current == launch_pid {
            return Ok(true);
        }
        if !visited.insert(current) {
            return Ok(false);
        }
        let next = match parent(current) {
            Ok(parent) if parent != 0 && parent != current => parent,
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        current = next;
    }
    Ok(false)
}

pub fn tcp_listener_image(_port: u16) -> io::Result<Option<(u32, PathBuf)>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "listener executable path lookup is unavailable on macOS",
    ))
}

pub fn process_stats(_pid: u32) -> io::Result<super::ProcessStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process stats are not implemented on macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_pid_parser_is_strict_and_deduplicates_same_owner() {
        assert_eq!(parse_lsof_pids(b"123\n123\n").unwrap(), Some(123));
        assert_eq!(parse_lsof_pids(b"").unwrap(), None);
        assert!(parse_lsof_pids(b"123\n456\n").is_err());
        assert!(parse_lsof_pids(b"PID\n123\n").is_err());
        assert!(parse_lsof_pids(b"123x\n").is_err());
        assert!(parse_lsof_pids(b"0\n").is_err());
    }

    #[test]
    fn parent_pid_parser_requires_one_numeric_row() {
        assert_eq!(parse_parent_pid(b" 42\n", 99).unwrap(), 42);
        assert_eq!(parse_parent_pid(b"0\n", 99).unwrap(), 0);
        assert!(parse_parent_pid(b"", 99).is_err());
        assert!(parse_parent_pid(b"42\n43\n", 99).is_err());
        assert!(parse_parent_pid(b"PPID\n", 99).is_err());
        assert!(parse_parent_pid(b"99999999999999999999\n", 99).is_err());
    }

    #[test]
    fn process_start_time_parser_requires_one_nonempty_row() {
        assert_eq!(
            parse_process_start_time(b"Mon Sep 29 18:20:49 2026\n", 99).unwrap(),
            "Mon Sep 29 18:20:49 2026"
        );
        assert!(parse_process_start_time(b"", 99).is_err());
        assert!(parse_process_start_time(b"first\nsecond\n", 99).is_err());
        assert!(parse_process_start_time(b"\xff", 99).is_err());
    }

    #[test]
    fn shutdown_helper_parser_matches_exact_emulator_kill_target() {
        let ps = b" 101 /sdk/emulator -kill 77 -sleep 20\n102 /sdk/emulator -kill 770 -sleep 20\n103 /other/tool -kill 77\n";
        assert_eq!(parse_shutdown_helper_pids(ps, 77).unwrap(), [101]);
        assert!(parse_shutdown_helper_pids(b"\xff", 77).is_err());
    }

    #[test]
    fn ancestry_is_bounded_and_fails_closed_on_missing_or_cycles() {
        assert!(!belongs_to_launch(0, 1).unwrap());
        let parents: std::collections::HashMap<u32, u32> =
            [(3, 2), (2, 1), (4, 9), (9, 4), (8, 0)].into();
        assert!(belongs_to_launch_with(1, 1, |_| unreachable!()).unwrap());
        assert!(belongs_to_launch_with(2, 1, |pid| Ok(parents[&pid])).unwrap());
        assert!(belongs_to_launch_with(3, 1, |pid| Ok(parents[&pid])).unwrap());
        assert!(!belongs_to_launch_with(4, 1, |pid| Ok(parents[&pid])).unwrap());
        assert!(!belongs_to_launch_with(8, 1, |pid| Ok(parents[&pid])).unwrap());
        assert!(!belongs_to_launch_with(7, 1, |_| Err(io::ErrorKind::NotFound.into())).unwrap());
        assert!(!belongs_to_launch_with(3, 1, |_| Ok(2)).unwrap());
        assert!(!belongs_to_launch_with(3, 1, |_| Ok(3)).unwrap());
        assert!(!belongs_to_launch_with(3, 99, |_| Ok(3)).unwrap());
        assert!(!belongs_to_launch_with(300, 1, |pid| Ok(pid - 1)).unwrap());
    }
}
