use crate::{adb, avd, platform};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

const FIRST_PORT: u16 = 5554;
const LAST_PORT: u16 = 5682;
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

trait StopOps {
    fn devices(&self) -> io::Result<Vec<adb::track::DeviceState>>;
    fn shell_v2(&self, serial: &str, command: &str) -> io::Result<adb::shell::ShellOutput>;
    fn avd_name(&self, port: u16) -> io::Result<String>;
    fn shutdown(&self, port: u16, deadline: Instant) -> io::Result<()>;
    fn console_owner_pid(&self, port: u16) -> io::Result<Option<u32>>;
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
    ops.shutdown(port, Instant::now() + Duration::from_secs(3))?;
    wait_until_closed(port, Instant::now() + STOP_TIMEOUT, || {
        ops.console_owner_pid(port)
    })
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
    let (port, _owner, name) = selected;
    ops.shutdown(port, Instant::now() + Duration::from_secs(3))?;
    wait_until_closed(port, Instant::now() + timeout, || {
        ops.console_owner_pid(port)
    })?;
    Ok(StopOutcome::Stopped { port, name })
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

fn wait_until_closed(
    port: u16,
    deadline: Instant,
    mut owner: impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<()> {
    loop {
        if owner()?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("emulator-{port} console still owns port after shutdown timeout"),
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

        fn avd_exists(&self, name: &str) -> bool {
            self.exists.contains(name)
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
    fn shutdown_wait_requires_console_disappearance() {
        let mut owners = [Some(12), None].into_iter();
        wait_until_closed(5554, Instant::now() + Duration::from_secs(1), || {
            Ok(owners.next().flatten())
        })
        .unwrap();
        let error = wait_until_closed(5554, Instant::now() + Duration::from_millis(1), || {
            Ok(Some(12))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
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
        let outcome = stop_target("emulator-5554", &ops, Duration::from_secs(1)).unwrap();
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
            stop_target("Pixel_9_API_36", &ops, Duration::from_secs(1)).unwrap(),
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
            1,
            "wait ran after failed kill"
        );

        let timeout = FakeOps {
            devices: vec![device("emulator-5554", "offline")],
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
