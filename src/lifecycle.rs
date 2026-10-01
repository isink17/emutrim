use crate::adb::protocol::remaining_until;
use crate::adb::shell::shell_v2_with_timeout;
use crate::{adb, avd, platform, slim};
use std::io;
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct VerifiedEmulator {
    pub name: String,
    pub serial: String,
    pub port: u16,
    pub launch_pid: u32,
    pub timing: crate::StartupTiming,
}

pub(crate) fn launch_and_wait(
    name: &str,
    requested_ram: Option<u32>,
    snapshot: avd::SnapshotMode,
    headless: bool,
    managed: bool,
    verify_process: bool,
    timings: bool,
) -> io::Result<VerifiedEmulator> {
    let info = avd::inspect_mode(name, managed)?;
    let ram = requested_ram.unwrap_or(info.ram_mb);
    avd::validate_ram(&info, ram)?;
    if !info.image.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "AVD system image directory not found: {}",
                info.image.display()
            ),
        ));
    }
    if verify_process && !platform::supports_verified_integrated_start() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "integrated launch requires verified process-to-console identity support",
        ));
    }
    let port = avd::available_console_port()?;
    let serial = format!("emulator-{port}");
    run_owned_launch(
        || avd::start_mode(name, ram, port, snapshot, headless, managed),
        |child| wait_for_launch(child, name, serial, port, verify_process, timings),
    )
}

const LAUNCH_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Process tree created by one `emulator` spawn; only this launch's own processes are targeted.
trait OwnedLaunch {
    fn terminate_descendants(&mut self) -> io::Result<()>;
    fn terminate_launcher(&mut self) -> io::Result<()>;
}

impl OwnedLaunch for Child {
    fn terminate_descendants(&mut self) -> io::Result<()> {
        // The unreleased Child handle keeps the launch PID from being reused.
        platform::terminate_launch_descendants(self.id(), LAUNCH_CLEANUP_TIMEOUT)
    }

    fn terminate_launcher(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_none() {
            if let Err(error) = self.kill() {
                if self.try_wait()?.is_none() {
                    return Err(error);
                }
            }
        }
        let deadline = Instant::now() + LAUNCH_CLEANUP_TIMEOUT;
        while self.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("launched emulator process {} did not exit", self.id()),
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

/// Spawn failures own nothing; failures after spawn clean up the owned launch and keep the
/// launch error primary.
fn run_owned_launch<L: OwnedLaunch, T>(
    spawn: impl FnOnce() -> io::Result<L>,
    wait: impl FnOnce(&mut L) -> io::Result<T>,
) -> io::Result<T> {
    let mut launch = spawn()?;
    wait(&mut launch).map_err(|primary| match cleanup_owned_launch(&mut launch) {
        Ok(()) => {
            eprintln!("stopped emulator processes from failed launch");
            primary
        }
        Err(cleanup) => io::Error::new(
            primary.kind(),
            format!("{primary}; failed launch cleanup error: {cleanup}"),
        ),
    })
}

fn cleanup_owned_launch(launch: &mut impl OwnedLaunch) -> io::Result<()> {
    let descendants = launch.terminate_descendants();
    let launcher = launch.terminate_launcher();
    let stragglers = if descendants.is_ok() {
        launch.terminate_descendants()
    } else {
        Ok(())
    };
    let errors: Vec<_> = [descendants, launcher, stragglers]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    match errors.as_slice() {
        [] => Ok(()),
        [first, ..] => Err(io::Error::new(
            first.kind(),
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

fn wait_for_launch(
    child: &mut Child,
    name: &str,
    serial: String,
    port: u16,
    verify_process: bool,
    timings: bool,
) -> io::Result<VerifiedEmulator> {
    let launch_pid = child.id();
    let mut timing = crate::StartupTiming {
        capture: timings,
        launched: Some(Instant::now()),
        ..crate::StartupTiming::default()
    };
    println!("started {name} as {serial}");
    println!("waiting for ADB transport...");
    let deadline = Instant::now() + Duration::from_secs(120);
    let ready = crate::wait_for_transport_with(
        &serial,
        deadline,
        |deadline| adb::track::devices_with_timeout(crate::default_adb_addr(), deadline),
        || {
            if !verify_process {
                return Ok(Some(true));
            }
            match platform::console_owner_pid(port)? {
                Some(owner) => Ok(Some(platform::belongs_to_launch(owner, launch_pid)?)),
                None => Ok(None),
            }
        },
        |deadline| {
            let running = child.try_wait().map(|status| status.is_none())?;
            let console = remaining_until(deadline).is_ok()
                && avd::console::avd_name_until(port, deadline).is_ok_and(|actual| actual == name);
            Ok((running, console))
        },
        |remaining| thread::sleep(remaining.min(Duration::from_millis(500))),
        &mut timing,
    )?;
    if timing.console.is_none() {
        if timings {
            eprintln!("startup timing: {}", timing.format());
        }
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for authenticated console for {name}"),
        ));
    }
    if !ready {
        if timings {
            eprintln!("startup timing: {}", timing.format());
        }
        return Err(crate::transport_timeout_error(
            &serial,
            child.try_wait()?.is_none(),
            timing.state.as_deref(),
            timing.error.as_deref(),
        ));
    }
    let qemu = shell_v2_with_timeout(
        crate::default_adb_addr(),
        &serial,
        "getprop ro.kernel.qemu",
        Instant::now() + Duration::from_secs(3),
    )?;
    if qemu.status != 0 || !slim::is_emulator(&serial, &qemu.stdout) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("requested launch did not verify as emulator transport {serial}"),
        ));
    }

    println!("waiting for Android boot...");
    let deadline = Instant::now() + Duration::from_secs(120);
    let boot = crate::wait_for_boot_until(
        &serial,
        deadline,
        |deadline| {
            let ready = crate::adb::shell::boot_completed_with_timeout(
                crate::default_adb_addr(),
                &serial,
                deadline,
            );
            if matches!(&ready, Ok(true)) {
                timing.boot = Some(Instant::now());
            }
            ready
        },
        || child.try_wait().map(|status| status.is_none()),
        |remaining| thread::sleep(remaining.min(Duration::from_millis(500))),
        || {},
    )?;
    match boot {
        crate::BootWait::Ready => {}
        crate::BootWait::TimedOut => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Android boot timed out on {serial}; guest was not modified"),
            ));
        }
        crate::BootWait::ProcessExited => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("emulator process exited before Android boot completed on {serial}"),
            ));
        }
    }
    Ok(VerifiedEmulator {
        name: name.into(),
        serial,
        port,
        launch_pid,
        timing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeLaunch {
        descendants_fail: bool,
        launcher_fail: bool,
        events: RefCell<Vec<&'static str>>,
    }

    impl OwnedLaunch for &FakeLaunch {
        fn terminate_descendants(&mut self) -> io::Result<()> {
            self.events.borrow_mut().push("descendants");
            if self.descendants_fail {
                Err(io::Error::other("descendant cleanup failed"))
            } else {
                Ok(())
            }
        }

        fn terminate_launcher(&mut self) -> io::Result<()> {
            self.events.borrow_mut().push("launcher");
            if self.launcher_fail {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "launcher cleanup failed",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn console_timeout() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for authenticated console for Alpha",
        )
    }

    #[test]
    fn pre_spawn_failure_has_nothing_to_clean() {
        let waited = RefCell::new(false);
        let error = run_owned_launch::<&FakeLaunch, ()>(
            || Err(io::Error::new(io::ErrorKind::NotFound, "emulator missing")),
            |_| {
                *waited.borrow_mut() = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!*waited.borrow());
    }

    #[test]
    fn post_spawn_failure_cleans_owned_launch_once_and_keeps_primary_error() {
        let launch = FakeLaunch::default();
        let error =
            run_owned_launch(|| Ok(&launch), |_| Err::<(), _>(console_timeout())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "timed out waiting for authenticated console for Alpha"
        );
        assert_eq!(
            *launch.events.borrow(),
            ["descendants", "launcher", "descendants"]
        );
    }

    #[test]
    fn cleanup_failure_is_surfaced_after_primary_error() {
        for (descendants_fail, launcher_fail, events, cleanup) in [
            (
                true,
                false,
                ["descendants", "launcher"].as_slice(),
                "descendant cleanup failed",
            ),
            (
                false,
                true,
                ["descendants", "launcher", "descendants"].as_slice(),
                "launcher cleanup failed",
            ),
            (
                true,
                true,
                ["descendants", "launcher"].as_slice(),
                "descendant cleanup failed; launcher cleanup failed",
            ),
        ] {
            let launch = FakeLaunch {
                descendants_fail,
                launcher_fail,
                ..FakeLaunch::default()
            };
            let error = run_owned_launch(
                || Ok(&launch),
                |_| Err::<(), _>(io::Error::new(io::ErrorKind::NotFound, "process exited")),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert_eq!(
                error.to_string(),
                format!("process exited; failed launch cleanup error: {cleanup}")
            );
            assert_eq!(*launch.events.borrow(), events);
        }
    }

    #[test]
    fn successful_launch_is_not_cleaned_up() {
        let launch = FakeLaunch::default();
        assert_eq!(run_owned_launch(|| Ok(&launch), |_| Ok(7)).unwrap(), 7);
        assert!(launch.events.borrow().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn failed_launch_stops_owned_process_tree_and_spares_unrelated_process() {
        use std::process::{Command, Stdio};

        fn spawn(program: &str, args: &[&str]) -> Child {
            Command::new(program)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }

        let mut unrelated = spawn("ping", &["-n", "60", "127.0.0.1"]);
        let mut owned_descendant = None;
        let error = run_owned_launch(
            || Ok(spawn("cmd", &["/d", "/c", "ping -n 60 127.0.0.1"])),
            |child| {
                let deadline = Instant::now() + Duration::from_secs(10);
                while owned_descendant.is_none() && Instant::now() < deadline {
                    owned_descendant = platform::child_pids(child.id()).unwrap().first().copied();
                    thread::sleep(Duration::from_millis(50));
                }
                Err::<(), _>(console_timeout())
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "timed out waiting for authenticated console for Alpha"
        );
        let owned_descendant = owned_descendant.expect("launch never spawned a descendant");
        assert!(platform::ProcessWatch::open(owned_descendant)
            .map_or(true, |watch| !watch.is_alive().unwrap()));
        let unrelated_alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        assert!(unrelated_alive, "unrelated process was killed");
    }
}
