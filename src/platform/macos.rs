use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::process::Command;

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
