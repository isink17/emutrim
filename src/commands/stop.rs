use crate::{adb, avd, platform};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

const FIRST_PORT: u16 = 5554;
const LAST_PORT: u16 = 5682;
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

trait ProcessLiveness {
    fn is_alive(&mut self) -> io::Result<bool>;
}

struct WatchedProcess<'a> {
    pid: u32,
    watch: Box<dyn ProcessLiveness + 'a>,
}

struct ProcessWatches<'a>(Vec<WatchedProcess<'a>>);

impl ProcessLiveness for ProcessWatches<'_> {
    fn is_alive(&mut self) -> io::Result<bool> {
        let mut alive = false;
        for process in &mut self.0 {
            alive |= process.watch.is_alive()?;
        }
        Ok(alive)
    }
}

impl ProcessLiveness for platform::ProcessWatch {
    fn is_alive(&mut self) -> io::Result<bool> {
        platform::ProcessWatch::is_alive(self)
    }
}

trait StopOps {
    fn devices(&self) -> io::Result<Vec<adb::track::DeviceState>>;
    fn shell_v2(&self, serial: &str, command: &str) -> io::Result<adb::shell::ShellOutput>;
    fn avd_name(&self, port: u16) -> io::Result<String>;
    fn shutdown(&self, port: u16, deadline: Instant) -> io::Result<()>;
    fn console_owner_pid(&self, port: u16) -> io::Result<Option<u32>>;
    fn process_parent_pid(&self, pid: u32) -> io::Result<Option<u32>>;
    fn shutdown_helper_pids(&self, pid: u32) -> io::Result<Vec<u32>>;
    fn watch_process(&self, pid: u32) -> io::Result<Box<dyn ProcessLiveness + '_>>;
    fn avd_exists(&self, name: &str) -> bool;
}

struct SystemStopOps;

impl StopOps for SystemStopOps {
    fn devices(&self) -> io::Result<Vec<adb::track::DeviceState>> {
        adb::track::devices(adb_addr())
    }
    fn shell_v2(&self, serial: &str, command: &str) -> io::Result<adb::shell::ShellOutput> {
        adb::shell::shell_v2(adb_addr(), serial, command)
    }
    fn avd_name(&self, port: u16) -> io::Result<String> {
        avd::console::avd_name(port)
    }
    fn shutdown(&self, port: u16, deadline: Instant) -> io::Result<()> {
        avd::console::shutdown_until(port, deadline)
    }
    fn console_owner_pid(&self, port: u16) -> io::Result<Option<u32>> {
        platform::console_owner_pid(port)
    }
    fn process_parent_pid(&self, pid: u32) -> io::Result<Option<u32>> {
        platform::process_parent_pid(pid)
    }
    fn shutdown_helper_pids(&self, pid: u32) -> io::Result<Vec<u32>> {
        platform::shutdown_helper_pids(pid)
    }
    fn watch_process(&self, pid: u32) -> io::Result<Box<dyn ProcessLiveness + '_>> {
        Ok(Box::new(platform::ProcessWatch::open(pid)?))
    }
    fn avd_exists(&self, name: &str) -> bool {
        avd::config_path_mode(name, false).is_ok() || avd::config_path_mode(name, true).is_ok()
    }
}

fn adb_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037)
}

#[derive(Debug, Eq, PartialEq)]
enum StopOutcome {
    Stopped { port: u16, name: String },
    AlreadyStopped { name: String },
}

pub fn run(args: Vec<String>) -> io::Result<()> {
    if args.len() != 1 || args[0].starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: stop <SERIAL|AVD>",
        ));
    }
    match stop_target(&args[0], &SystemStopOps, STOP_TIMEOUT)? {
        StopOutcome::Stopped { port, name } => println!("Stopped emulator-{port} ({name})."),
        StopOutcome::AlreadyStopped { name } => println!("{name} is already stopped."),
    }
    Ok(())
}

pub(crate) fn stop_launched(serial: &str, expected_name: &str, launch_pid: u32) -> io::Result<()> {
    let port = avd::console_port(serial).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid launched emulator serial",
        )
    })?;
    let ops = SystemStopOps;
    let owner = ops.console_owner_pid(port)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "launched emulator console disappeared",
        )
    })?;
    if !platform::belongs_to_launch(owner, launch_pid)? || ops.avd_name(port)? != expected_name {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "emulator console no longer matches reset launch identity",
        ));
    }
    verify_identity(port, serial, &ops)?;
    stop_authenticated(port, serial, owner, expected_name, &ops, STOP_TIMEOUT)
}

fn stop_target(target: &str, ops: &impl StopOps, timeout: Duration) -> io::Result<StopOutcome> {
    let by_serial = avd::console_port(target).is_some();
    let devices = ops.devices()?;
    if known_physical_serial(target, by_serial, &devices) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{target} is an ADB device serial, not an emulator console serial; refusing stop"
            ),
        ));
    }
    let candidates = if by_serial {
        let port = avd::console_port(target).unwrap();
        let owner = ops.console_owner_pid(port)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no emulator console owns {target}"),
            )
        })?;
        let name = ops.avd_name(port)?;
        verify_identity(port, target, ops)?;
        vec![(port, owner, name)]
    } else {
        let mut matches = Vec::new();
        for port in (FIRST_PORT..=LAST_PORT).step_by(2) {
            let Some(owner) = ops.console_owner_pid(port)? else {
                continue;
            };
            let name = ops.avd_name(port).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot authenticate console on {port}: {error}"),
                )
            })?;
            if same_avd_name(&name, target) {
                verify_identity(port, &format!("emulator-{port}"), ops)?;
                matches.push((port, owner, name));
            }
        }
        matches
    };
    let selected = match select_candidate(candidates, by_serial, target)? {
        Some(selected) => selected,
        None => {
            if !ops.avd_exists(target) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("AVD {target:?} not found"),
                ));
            }
            return Ok(StopOutcome::AlreadyStopped {
                name: target.into(),
            });
        }
    };
    let (port, owner, name) = selected;
    stop_authenticated(
        port,
        &format!("emulator-{port}"),
        owner,
        &name,
        ops,
        timeout,
    )?;
    Ok(StopOutcome::Stopped { port, name })
}

fn stop_authenticated(
    port: u16,
    serial: &str,
    owner_pid: u32,
    name: &str,
    ops: &impl StopOps,
    timeout: Duration,
) -> io::Result<()> {
    let owner_process = ops.watch_process(owner_pid)?;
    let mut processes = ProcessWatches(vec![WatchedProcess {
        pid: owner_pid,
        watch: owner_process,
    }]);
    if !processes.0[0].watch.is_alive()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "authenticated emulator process exited before shutdown; refusing stop",
        ));
    }
    if let Some(parent_pid) = ops.process_parent_pid(owner_pid)? {
        let parent = ops.watch_process(parent_pid)?;
        if !processes.0[0].watch.is_alive()?
            || ops.process_parent_pid(owner_pid)? != Some(parent_pid)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "emulator process ancestry changed before shutdown; refusing stop",
            ));
        }
        processes.0.push(WatchedProcess {
            pid: parent_pid,
            watch: parent,
        });
    }
    if ops.console_owner_pid(port)? != Some(owner_pid) || ops.avd_name(port)? != name {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "emulator console identity changed before shutdown; refusing stop",
        ));
    }
    let deadline = Instant::now() + timeout;
    ops.shutdown(port, Instant::now() + Duration::from_secs(3))?;
    wait_until_stopped(port, serial, owner_pid, deadline, ops, &mut processes)
}

type Candidate = (u16, u32, String);

fn known_physical_serial(
    target: &str,
    emulator_serial: bool,
    devices: &[adb::track::DeviceState],
) -> bool {
    !emulator_serial && devices.iter().any(|device| device.serial == target)
}

fn same_avd_name(actual: &str, requested: &str) -> bool {
    actual == requested
}

fn select_candidate(
    candidates: Vec<Candidate>,
    by_serial: bool,
    target: &str,
) -> io::Result<Option<Candidate>> {
    match candidates.as_slice() {
        [] if by_serial => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{target} is not a running emulator"),
        )),
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "multiple running emulators report AVD name {target:?}; refusing ambiguous stop"
            ),
        )),
    }
}

fn wait_until_stopped<'a>(
    port: u16,
    serial: &str,
    owner_pid: u32,
    deadline: Instant,
    ops: &'a impl StopOps,
    processes: &mut ProcessWatches<'a>,
) -> io::Result<()> {
    let mut process_exit_at = None;
    let mut next_helper_check = Instant::now();
    let mut shutdown_helpers = Vec::new();
    let mut helpers_clear_since = None;
    loop {
        let now = Instant::now();
        if now >= next_helper_check {
            shutdown_helpers = ops.shutdown_helper_pids(owner_pid)?;
            for pid in &shutdown_helpers {
                if !processes.0.iter().any(|process| process.pid == *pid) {
                    match ops.watch_process(*pid) {
                        Ok(watch) => processes.0.push(WatchedProcess { pid: *pid, watch }),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            next_helper_check = now + Duration::from_millis(500);
        }
        let process_alive = processes.is_alive()?;
        if process_alive {
            process_exit_at = None;
        } else {
            process_exit_at.get_or_insert_with(Instant::now);
        }
        let console_owner = ops.console_owner_pid(port)?;
        if let Some(pid) = console_owner.filter(|pid| *pid != owner_pid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "console port {port} was reused by PID {pid} after shutdown began; refusing to follow it"
                ),
            ));
        }
        let devices = ops.devices()?;
        let adb_state = devices
            .iter()
            .find(|device| device.serial == serial)
            .map(|device| device.state.as_str());
        if !process_alive
            && console_owner == Some(owner_pid)
            && process_exit_at.is_some_and(|exited| exited.elapsed() >= Duration::from_secs(1))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "original process {owner_pid} exited but console port {port} still reports its PID; refusing ambiguous shutdown completion"
                ),
            ));
        }
        if process_alive || !shutdown_helpers.is_empty() {
            helpers_clear_since = None;
        } else {
            helpers_clear_since.get_or_insert_with(Instant::now);
        }
        if !process_alive
            && console_owner.is_none()
            && adb_state.is_none()
            && helpers_clear_since.is_some_and(|clear| clear.elapsed() >= Duration::from_secs(1))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "emulator-{port} shutdown timed out: original process {}; console listener {}; exact ADB transport {}; shutdown helper {}",
                    if process_alive { "still alive" } else { "exited" },
                    if console_owner.is_some() { "still present" } else { "absent" },
                    adb_state.unwrap_or("absent"),
                    if shutdown_helpers.is_empty() { "absent" } else { "still active" }
                ),
            ));
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn verify_identity(port: u16, serial: &str, ops: &impl StopOps) -> io::Result<()> {
    if avd::console_port(serial) != Some(port) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("console port {port} does not match exact serial {serial}"),
        ));
    }
    let devices = ops.devices()?;
    if let Some(device) = devices.iter().find(|device| device.serial == serial) {
        if device.state == "device" {
            let qemu = ops.shell_v2(serial, "getprop ro.kernel.qemu")?;
            if qemu.status != 0 || qemu.stdout.trim() != "1" {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{serial} does not prove emulator identity (ro.kernel.qemu != 1)"),
                ));
            }
        } else if device.state != "offline" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{serial} transport state {:?} does not prove a stoppable emulator",
                    device.state
                ),
            ));
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("exact ADB transport {serial} not found; refusing console-only stop"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet, VecDeque};

    #[derive(Default)]
    struct FakeOps {
        devices: Vec<adb::track::DeviceState>,
        qemu: HashMap<String, (String, u32)>,
        names: HashMap<u16, String>,
        auth_fail: HashSet<u16>,
        owners: RefCell<HashMap<u16, VecDeque<Option<u32>>>>,
        post_kill_devices: RefCell<VecDeque<Vec<adb::track::DeviceState>>>,
        process_states: RefCell<VecDeque<bool>>,
        shutdown_helpers: RefCell<VecDeque<Vec<u32>>>,
        parent_pid: Option<u32>,
        parent_process_states: RefCell<VecDeque<bool>>,
        watched_pids: RefCell<Vec<u32>>,
        exists: HashSet<String>,
        kill_fails: bool,
        kills: RefCell<Vec<u16>>,
        shells: RefCell<Vec<(String, String)>>,
        auth_calls: RefCell<Vec<u16>>,
        owner_events: RefCell<Vec<(u16, Option<u32>)>>,
        events: RefCell<Vec<&'static str>>,
    }

    impl StopOps for FakeOps {
        fn devices(&self) -> io::Result<Vec<adb::track::DeviceState>> {
            self.events.borrow_mut().push("devices");
            if !self.kills.borrow().is_empty() {
                return Ok(self
                    .post_kill_devices
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_default());
            }
            Ok(self.devices.clone())
        }

        fn shell_v2(&self, serial: &str, command: &str) -> io::Result<adb::shell::ShellOutput> {
            self.shells
                .borrow_mut()
                .push((serial.into(), command.into()));
            let (stdout, status) = self.qemu.get(serial).cloned().unwrap_or_default();
            Ok(adb::shell::ShellOutput {
                stdout,
                status,
                ..Default::default()
            })
        }

        fn avd_name(&self, port: u16) -> io::Result<String> {
            self.auth_calls.borrow_mut().push(port);
            if self.auth_fail.contains(&port) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fake console authentication failed",
                ));
            }
            self.names.get(&port).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fake console authentication failed",
                )
            })
        }

        fn shutdown(&self, port: u16, _deadline: Instant) -> io::Result<()> {
            self.events.borrow_mut().push("kill");
            self.kills.borrow_mut().push(port);
            if self.kill_fails {
                Err(io::Error::other("fake console kill failed"))
            } else {
                Ok(())
            }
        }

        fn console_owner_pid(&self, port: u16) -> io::Result<Option<u32>> {
            let owner = self
                .owners
                .borrow_mut()
                .get_mut(&port)
                .and_then(VecDeque::pop_front)
                .unwrap_or(None);
            self.owner_events.borrow_mut().push((port, owner));
            self.events.borrow_mut().push("owner");
            Ok(owner)
        }

        fn process_parent_pid(&self, _pid: u32) -> io::Result<Option<u32>> {
            Ok(self.parent_pid)
        }

        fn shutdown_helper_pids(&self, _pid: u32) -> io::Result<Vec<u32>> {
            self.events.borrow_mut().push("helper");
            Ok(self
                .shutdown_helpers
                .borrow_mut()
                .pop_front()
                .unwrap_or_default())
        }

        fn watch_process(&self, pid: u32) -> io::Result<Box<dyn ProcessLiveness + '_>> {
            self.events.borrow_mut().push("watch");
            self.watched_pids.borrow_mut().push(pid);
            Ok(Box::new(FakeProcessLiveness { ops: self, pid }))
        }

        fn avd_exists(&self, name: &str) -> bool {
            self.exists.contains(name)
        }
    }

    struct FakeProcessLiveness<'a> {
        ops: &'a FakeOps,
        pid: u32,
    }

    impl ProcessLiveness for FakeProcessLiveness<'_> {
        fn is_alive(&mut self) -> io::Result<bool> {
            self.ops.events.borrow_mut().push("process");
            let states = if self.pid == 42 {
                &self.ops.process_states
            } else {
                &self.ops.parent_process_states
            };
            Ok(states
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.ops.kills.borrow().is_empty()))
        }
    }

    fn device(serial: &str, state: &str) -> adb::track::DeviceState {
        adb::track::DeviceState {
            serial: serial.into(),
            state: state.into(),
        }
    }

    fn fake_shell(stdout: &str, status: u32) -> (String, u32) {
        (stdout.into(), status)
    }

    fn serial_fake(name: &str, state: &str) -> FakeOps {
        let ops = FakeOps {
            devices: vec![device("emulator-5554", state)],
            names: HashMap::from([(5554, name.into())]),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(42), Some(42), None]);
        ops
    }

    fn process_watches(ops: &FakeOps, pid: u32) -> ProcessWatches<'_> {
        ProcessWatches(vec![WatchedProcess {
            pid,
            watch: Box::new(FakeProcessLiveness { ops, pid }),
        }])
    }

    fn owners(ops: &FakeOps, port: u16, sequence: impl IntoIterator<Item = Option<u32>>) {
        ops.owners
            .borrow_mut()
            .insert(port, sequence.into_iter().collect());
    }

    fn no_kill(ops: &FakeOps) {
        assert!(
            ops.kills.borrow().is_empty(),
            "unexpected kill: {:?}",
            ops.kills.borrow()
        );
    }

    #[test]
    fn refuses_known_physical_device_serial() {
        let devices = [adb::track::DeviceState {
            serial: "R58M123456A".into(),
            state: "device".into(),
        }];
        assert!(known_physical_serial("R58M123456A", false, &devices));
        assert!(!known_physical_serial("emulator-5554", true, &devices));
        assert!(!known_physical_serial("other", false, &devices));
    }

    #[test]
    fn avd_match_is_exact_and_unique() {
        assert!(same_avd_name("Pixel_9_API_36", "Pixel_9_API_36"));
        assert!(!same_avd_name("Pixel_9_API_36_copy", "Pixel_9_API_36"));
        let one = (5554, 12, "Pixel".to_owned());
        assert_eq!(
            select_candidate(vec![one.clone()], false, "Pixel").unwrap(),
            Some(one.clone())
        );
        assert!(select_candidate(vec![], true, "emulator-5554").is_err());
        assert!(select_candidate(vec![], false, "Pixel").unwrap().is_none());
        assert!(select_candidate(
            vec![one.clone(), (5556, 13, "Pixel".into())],
            false,
            "Pixel"
        )
        .is_err());
    }

    #[test]
    fn console_and_adb_disappearance_do_not_override_live_original_process() {
        let ops = FakeOps {
            process_states: RefCell::new([true, false].into()),
            post_kill_devices: RefCell::new(
                [vec![device("emulator-5554", "device")], vec![]].into(),
            ),
            ..Default::default()
        };
        ops.kills.borrow_mut().push(5554);
        owners(&ops, 5554, [None, None]);
        let mut processes = process_watches(&ops, 42);
        wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            Instant::now() + Duration::from_secs(3),
            &ops,
            &mut processes,
        )
        .unwrap();
        assert_eq!(ops.process_states.borrow().len(), 0);
    }

    #[test]
    fn shutdown_over_ten_seconds_can_complete_within_thirty_second_bound() {
        let ops = FakeOps {
            process_states: RefCell::new(std::iter::repeat_n(true, 102).chain([false]).collect()),
            ..Default::default()
        };
        ops.kills.borrow_mut().push(5554);
        owners(&ops, 5554, std::iter::repeat_n(Some(42), 102).chain([None]));
        let mut processes = process_watches(&ops, 42);
        let start = Instant::now();
        wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            start + STOP_TIMEOUT,
            &ops,
            &mut processes,
        )
        .unwrap();
        assert!(start.elapsed() > Duration::from_secs(10));
    }

    #[test]
    fn adb_offline_must_disappear_and_stale_entry_times_out() {
        let ops = FakeOps {
            post_kill_devices: RefCell::new(
                [vec![device("emulator-5554", "offline")], vec![]].into(),
            ),
            ..Default::default()
        };
        ops.kills.borrow_mut().push(5554);
        owners(&ops, 5554, [None, None]);
        let mut processes = process_watches(&ops, 42);
        wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            Instant::now() + Duration::from_secs(3),
            &ops,
            &mut processes,
        )
        .unwrap();

        let stale = FakeOps {
            post_kill_devices: RefCell::new(vec![vec![device("emulator-5554", "offline")]].into()),
            ..Default::default()
        };
        stale.kills.borrow_mut().push(5554);
        let mut processes = process_watches(&stale, 42);
        let error = wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            Instant::now(),
            &stale,
            &mut processes,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("exact ADB transport offline"));
    }

    #[test]
    fn different_listener_after_shutdown_is_never_adopted() {
        let ops = serial_fake("Pixel", "offline");
        owners(&ops, 5554, [Some(42), Some(42), Some(99)]);
        let error = stop_target("emulator-5554", &ops, Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("reused by PID 99"));
        assert_eq!(&*ops.kills.borrow(), &[5554]);
    }

    #[test]
    fn waits_for_original_emulator_parent_after_qemu_exits() {
        let ops = FakeOps {
            devices: vec![device("emulator-5554", "offline")],
            names: HashMap::from([(5554, "Pixel".into())]),
            parent_pid: Some(84),
            process_states: RefCell::new([true, true].into()),
            parent_process_states: RefCell::new([true, false].into()),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(42), Some(42), None, None]);
        let result = stop_target("emulator-5554", &ops, Duration::from_secs(3));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(&*ops.watched_pids.borrow(), &[42, 84]);
    }

    #[test]
    fn waits_for_emulator_shutdown_helper_bound_to_captured_qemu_pid() {
        let ops = FakeOps {
            parent_process_states: RefCell::new([true, false].into()),
            shutdown_helpers: RefCell::new([vec![], vec![88], vec![88], vec![]].into()),
            ..Default::default()
        };
        ops.kills.borrow_mut().push(5554);
        owners(&ops, 5554, [None]);
        let mut processes = ProcessWatches(vec![WatchedProcess {
            pid: 42,
            watch: Box::new(FakeProcessLiveness { ops: &ops, pid: 42 }),
        }]);
        let started = Instant::now();
        wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            started + STOP_TIMEOUT,
            &ops,
            &mut processes,
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert_eq!(&*ops.watched_pids.borrow(), &[88]);
    }

    #[test]
    fn same_pid_listener_after_captured_process_exit_is_ambiguous() {
        let ops = FakeOps::default();
        ops.kills.borrow_mut().push(5554);
        owners(&ops, 5554, std::iter::repeat_n(Some(42), 20));
        let mut processes = process_watches(&ops, 42);
        ops.process_states.borrow_mut().push_back(false);
        let error = wait_until_stopped(
            5554,
            "emulator-5554",
            42,
            Instant::now() + Duration::from_secs(2),
            &ops,
            &mut processes,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("refusing ambiguous shutdown completion"));
    }

    #[test]
    fn serial_console_auth_failure_never_kills_or_falls_back() {
        let ops = FakeOps {
            devices: vec![device("emulator-5554", "device")],
            ..Default::default()
        };
        owners(&ops, 5554, [Some(1)]);
        let error = stop_target("emulator-5554", &ops, Duration::ZERO).unwrap_err();
        assert!(error
            .to_string()
            .contains("fake console authentication failed"));
        assert_eq!(&*ops.auth_calls.borrow(), &[5554]);
        no_kill(&ops);
    }

    #[test]
    fn process_that_exits_before_kill_is_not_stopped() {
        let ops = serial_fake("Pixel", "offline");
        ops.process_states.borrow_mut().push_back(false);
        assert_eq!(
            stop_target("emulator-5554", &ops, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        no_kill(&ops);
    }

    #[test]
    fn absent_host_listener_owner_refuses_before_console_or_kill() {
        let ops = FakeOps {
            devices: vec![device("emulator-5554", "offline")],
            ..Default::default()
        };
        let error = stop_target("emulator-5554", &ops, Duration::ZERO).unwrap_err();
        assert!(error
            .to_string()
            .contains("no emulator console owns emulator-5554"));
        assert!(ops.auth_calls.borrow().is_empty());
        no_kill(&ops);
    }

    #[test]
    fn serial_qemu_contradiction_or_shell_failure_never_kills() {
        for evidence in [fake_shell("0\n", 0), fake_shell("1\n", 1)] {
            let mut ops = serial_fake("Pixel", "device");
            ops.qemu.insert("emulator-5554".into(), evidence);
            let error = stop_target("emulator-5554", &ops, Duration::ZERO).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                &*ops.shells.borrow(),
                &[("emulator-5554".into(), "getprop ro.kernel.qemu".into())]
            );
            no_kill(&ops);
        }
    }

    #[test]
    fn offline_exact_transport_skips_qemu_then_kills_and_waits_for_disappearance() {
        let ops = serial_fake("Pixel", "offline");
        let outcome = stop_target("emulator-5554", &ops, Duration::from_secs(3)).unwrap();
        assert_eq!(
            outcome,
            StopOutcome::Stopped {
                port: 5554,
                name: "Pixel".into()
            }
        );
        assert!(ops.shells.borrow().is_empty(), "offline path queried QEMU");
        assert_eq!(&*ops.kills.borrow(), &[5554]);
        assert_eq!(ops.owner_events.borrow().last(), Some(&(5554, None)));
        let events = ops.events.borrow();
        let kill = events.iter().position(|event| *event == "kill").unwrap();
        let disappearance = events.iter().rposition(|event| *event == "owner").unwrap();
        assert!(kill < disappearance);
    }

    #[test]
    fn missing_exact_transport_does_not_borrow_unrelated_emulator_evidence() {
        let ops = FakeOps {
            devices: vec![device("emulator-5556", "device")],
            names: HashMap::from([(5554, "Pixel".into())]),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(1)]);
        let error = stop_target("emulator-5554", &ops, Duration::ZERO).unwrap_err();
        assert!(error
            .to_string()
            .contains("exact ADB transport emulator-5554 not found"));
        assert!(!ops
            .shells
            .borrow()
            .iter()
            .any(|call| call.0 == "emulator-5556"));
        no_kill(&ops);
    }

    #[test]
    fn unauthorized_transport_refuses_without_qemu_or_kill() {
        let ops = serial_fake("Pixel", "unauthorized");
        assert_eq!(
            stop_target("emulator-5554", &ops, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(ops.shells.borrow().is_empty());
        no_kill(&ops);
    }

    #[test]
    fn physical_serial_refuses_before_console_scan() {
        let ops = FakeOps {
            devices: vec![device("R58M123456A", "device")],
            ..Default::default()
        };
        assert_eq!(
            stop_target("R58M123456A", &ops, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(ops.auth_calls.borrow().is_empty());
        assert!(ops.owner_events.borrow().is_empty());
        no_kill(&ops);
    }

    #[test]
    fn name_mismatch_survives_and_exact_name_selects_only_matching_console() {
        let ops = FakeOps {
            devices: vec![
                device("emulator-5554", "device"),
                device("emulator-5556", "device"),
            ],
            names: HashMap::from([(5554, "Other_AVD".into()), (5556, "Pixel_9_API_36".into())]),
            qemu: HashMap::from([("emulator-5556".into(), fake_shell("1\n", 0))]),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(10)]);
        owners(&ops, 5556, [Some(11), Some(11), None]);
        assert_eq!(
            stop_target("Pixel_9_API_36", &ops, Duration::from_secs(3)).unwrap(),
            StopOutcome::Stopped {
                port: 5556,
                name: "Pixel_9_API_36".into()
            }
        );
        assert_eq!(&*ops.kills.borrow(), &[5556]);
        assert_eq!(
            &*ops.shells.borrow(),
            &[("emulator-5556".into(), "getprop ro.kernel.qemu".into())]
        );
    }

    #[test]
    fn name_scan_auth_failure_fails_closed_without_kill() {
        let mut ops = FakeOps::default();
        ops.auth_fail.insert(5554);
        owners(&ops, 5554, [Some(10)]);
        let error = stop_target("Pixel", &ops, Duration::ZERO).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot authenticate console on 5554"));
        no_kill(&ops);
    }

    #[test]
    fn name_match_with_qemu_contradiction_never_kills() {
        let mut ops = serial_fake("Pixel_9_API_36", "device");
        ops.qemu.insert("emulator-5554".into(), fake_shell("0", 0));
        assert_eq!(
            stop_target("Pixel_9_API_36", &ops, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        no_kill(&ops);
    }

    #[test]
    fn shutdown_error_and_timeout_never_report_success() {
        let mut failed = serial_fake("Pixel", "offline");
        failed.kill_fails = true;
        assert!(stop_target("emulator-5554", &failed, Duration::ZERO).is_err());
        assert_eq!(&*failed.kills.borrow(), &[5554]);
        assert_eq!(
            failed.owner_events.borrow().len(),
            2,
            "wait ran after failed kill"
        );

        let timeout = FakeOps {
            devices: vec![device("emulator-5554", "offline")],
            process_states: RefCell::new([true, true].into()),
            names: HashMap::from([(5554, "Pixel".into())]),
            ..Default::default()
        };
        owners(&timeout, 5554, [Some(42), Some(42)]);
        assert_eq!(
            stop_target("emulator-5554", &timeout, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(&*timeout.kills.borrow(), &[5554]);
    }

    #[test]
    fn existing_avd_with_only_mismatched_console_is_already_stopped() {
        let ops = FakeOps {
            exists: HashSet::from(["Pixel".into()]),
            names: HashMap::from([(5554, "Other".into())]),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(10)]);
        assert_eq!(
            stop_target("Pixel", &ops, Duration::ZERO).unwrap(),
            StopOutcome::AlreadyStopped {
                name: "Pixel".into()
            }
        );
        no_kill(&ops);
    }

    #[test]
    fn multiple_exact_name_matches_refuse_without_kill() {
        let ops = FakeOps {
            devices: vec![
                device("emulator-5554", "offline"),
                device("emulator-5556", "offline"),
            ],
            names: HashMap::from([(5554, "Pixel".into()), (5556, "Pixel".into())]),
            ..Default::default()
        };
        owners(&ops, 5554, [Some(10)]);
        owners(&ops, 5556, [Some(11)]);
        let error = stop_target("Pixel", &ops, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("multiple running emulators"));
        no_kill(&ops);
    }
}
