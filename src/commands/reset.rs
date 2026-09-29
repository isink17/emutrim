use crate::avd::{self, SnapshotMode};
use crate::lifecycle::{self, VerifiedEmulator};
use crate::managed::{self, ClearTarget, Layout};
use crate::{platform, slim};
use std::io;

const USAGE: &str = "usage: emutrim reset <AVD_NAME> [--yes]";

pub fn run(args: Vec<String>) -> io::Result<()> {
    let (name, yes) = parse_args(&args)?;
    let layout = Layout::resolve()?;
    let pending = managed::reset::validate_named(&layout, name)?;
    let target = managed::resolve_managed_avd(&layout, name).map_err(|error| {
        if pending {
            io::Error::new(
                error.kind(),
                format!("pending reset for {name:?} cannot be resumed because managed ownership no longer validates: {error}; use emutrim managed clean --yes only if abandoning this environment is intended"),
            )
        } else {
            error
        }
    })?;
    let pending = managed::reset::validate_existing(&layout, &target)?;
    managed::check_target_not_running(&target, &layout, &managed::SystemClearOps)?;
    let info = avd::inspect_mode(name, true)?;
    avd::validate_ram(&info, info.ram_mb)?;
    if !info.image.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("managed system image not found: {}", info.image.display()),
        ));
    }
    let emulator = avd::emulator_path(&layout.sdk);
    if !emulator.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Emulator binary not found: {}", emulator.display()),
        ));
    }
    if !platform::supports_verified_integrated_start() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reset requires verified process-to-console identity support",
        ));
    }
    avd::available_console_port()?;

    print_plan(name);
    if !yes {
        println!("No changes made. Re-run with --yes to reset.");
        return Ok(());
    }
    if pending {
        println!("retrying pending reset for {name}");
    }
    execute_reset(
        &target,
        &SystemResetOps {
            layout: &layout,
            target: &target,
        },
    )
}

fn parse_args(args: &[String]) -> io::Result<(&str, bool)> {
    match args {
        [name] if safe_cli_name(name) => Ok((name, false)),
        [name, yes] if safe_cli_name(name) && yes == "--yes" => Ok((name, true)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    }
}

fn safe_cli_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn print_plan(name: &str) {
    println!("Reset plan for managed AVD {name:?}:");
    println!("  preserve AVD definition, managed SDK/system image, and SD-card image");
    println!("  launch with -wipe-data -no-snapshot-load");
    println!("  wait for verified boot; prove EmuTrim guest state is absent");
    println!("  stop exact emulator and leave AVD stopped");
}

trait ResetOps {
    fn persist_marker(&self) -> io::Result<()>;
    fn launch(&self) -> io::Result<VerifiedEmulator>;
    fn guest_state_absent(&self, serial: &str) -> io::Result<bool>;
    fn stop(&self, launched: &VerifiedEmulator) -> io::Result<()>;
    fn clear_marker(&self) -> io::Result<()>;
}

fn execute_reset(target: &ClearTarget, ops: &impl ResetOps) -> io::Result<()> {
    ops.persist_marker()?;
    let launched = ops.launch()?;
    if launched.name != target.name() || avd::console_port(&launched.serial) != Some(launched.port)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reset launch identity does not match managed target",
        ));
    }
    if !ops.guest_state_absent(&launched.serial)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "EmuTrim guest state remains after wipe; pending reset retained",
        ));
    }
    ops.stop(&launched)?;
    ops.clear_marker()?;
    println!("reset complete; {name} is stopped", name = launched.name);
    Ok(())
}

struct SystemResetOps<'a> {
    layout: &'a Layout,
    target: &'a ClearTarget,
}

impl ResetOps for SystemResetOps<'_> {
    fn persist_marker(&self) -> io::Result<()> {
        managed::reset::create_or_verify(self.layout, self.target)
    }
    fn launch(&self) -> io::Result<VerifiedEmulator> {
        lifecycle::launch_and_wait(
            self.target.name().as_str(),
            None,
            SnapshotMode::Reset,
            false,
            true,
            true,
            false,
        )
    }
    fn guest_state_absent(&self, serial: &str) -> io::Result<bool> {
        state_is_absent(slim::inspect_state(crate::default_adb_addr(), serial)?)
    }
    fn stop(&self, launched: &VerifiedEmulator) -> io::Result<()> {
        crate::commands::stop::stop_launched(
            &launched.serial,
            &launched.name,
            launched.launch_pid,
        )?;
        if crate::platform::console_owner_pid(avd::console_port(&launched.serial).ok_or_else(
            || io::Error::new(io::ErrorKind::InvalidInput, "invalid reset emulator serial"),
        )?)?
        .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "reset emulator console remains after shutdown",
            ));
        }
        Ok(())
    }
    fn clear_marker(&self) -> io::Result<()> {
        managed::reset::remove_after_success(self.layout, self.target)
    }
}

fn state_is_absent(state: Option<slim::state::State>) -> io::Result<bool> {
    if state.is_some() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "EmuTrim guest state remains after wipe; pending reset retained",
        ))
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture() -> (Layout, ClearTarget) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "emutrim-reset-command-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let managed = root.join("managed");
        let avd = managed.join("avd");
        let sdk = managed.join("sdk");
        fs::create_dir_all(avd.join("Alpha.avd")).unwrap();
        fs::create_dir_all(sdk.join("system-images/image")).unwrap();
        fs::write(
            avd.join("Alpha.ini"),
            format!("path={}\n", avd.join("Alpha.avd").display()),
        )
        .unwrap();
        let layout = Layout {
            root,
            managed: managed.clone(),
            sdk,
            avd,
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
        };
        fs::write(&layout.manifest, br#"{"schema":1,"avds":["Alpha"]}"#).unwrap();
        let target = managed::resolve_managed_avd(&layout, "Alpha").unwrap();
        (layout, target)
    }

    struct FakeReset {
        fail: Option<&'static str>,
        state_absent: bool,
        launch_name: &'static str,
        marker: Cell<bool>,
        events: RefCell<Vec<&'static str>>,
    }

    impl FakeReset {
        fn result(&self, stage: &'static str) -> io::Result<()> {
            if self.fail == Some(stage) {
                Err(io::Error::other(format!("injected {stage} failure")))
            } else {
                Ok(())
            }
        }
    }

    impl ResetOps for FakeReset {
        fn persist_marker(&self) -> io::Result<()> {
            self.events.borrow_mut().push("marker");
            self.result("marker")?;
            self.marker.set(true);
            Ok(())
        }
        fn launch(&self) -> io::Result<VerifiedEmulator> {
            self.events.borrow_mut().push("launch");
            self.result("launch")?;
            Ok(VerifiedEmulator {
                name: self.launch_name.into(),
                serial: "emulator-5554".into(),
                port: 5554,
                launch_pid: 42,
                timing: crate::StartupTiming::default(),
            })
        }
        fn guest_state_absent(&self, _: &str) -> io::Result<bool> {
            self.events.borrow_mut().push("state");
            self.result("state")?;
            Ok(self.state_absent)
        }
        fn stop(&self, _: &VerifiedEmulator) -> io::Result<()> {
            self.events.borrow_mut().push("stop");
            self.result("stop")
        }
        fn clear_marker(&self) -> io::Result<()> {
            self.events.borrow_mut().push("clear");
            self.result("clear")?;
            self.marker.set(false);
            Ok(())
        }
    }

    fn fake(fail: Option<&'static str>, state_absent: bool) -> FakeReset {
        FakeReset {
            fail,
            state_absent,
            launch_name: "Alpha",
            marker: Cell::new(false),
            events: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn reset_arguments_require_one_safe_name_and_optional_yes() {
        assert_eq!(parse_args(&["Pixel_9".into()]).unwrap(), ("Pixel_9", false));
        assert_eq!(
            parse_args(&["Pixel_9".into(), "--yes".into()]).unwrap(),
            ("Pixel_9", true)
        );
        for args in [
            vec![],
            vec!["../external".into(), "--yes".into()],
            vec!["Pixel_9".into(), "--force".into()],
            vec!["A".into(), "B".into()],
        ] {
            assert!(parse_args(&args).is_err());
        }
    }

    #[test]
    fn marker_failure_prevents_any_launch() {
        let (layout, target) = fixture();
        let ops = fake(Some("marker"), true);
        assert!(execute_reset(&target, &ops).is_err());
        assert_eq!(*ops.events.borrow(), ["marker"]);
        assert!(!ops.marker.get());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn reset_clears_marker_only_after_state_proof_and_exact_stop() {
        let (layout, target) = fixture();
        let ops = fake(None, true);
        assert!(execute_reset(&target, &ops).is_ok());
        assert_eq!(
            *ops.events.borrow(),
            ["marker", "launch", "state", "stop", "clear"]
        );
        assert!(!ops.marker.get());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn mismatched_launch_identity_retains_marker_before_guest_inspection() {
        let (layout, target) = fixture();
        let mut ops = fake(None, true);
        ops.launch_name = "Other";
        assert!(execute_reset(&target, &ops).is_err());
        assert_eq!(*ops.events.borrow(), ["marker", "launch"]);
        assert!(ops.marker.get());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn only_absent_guest_state_is_reset_proof() {
        assert!(state_is_absent(None).unwrap());
        assert!(state_is_absent(Some(slim::state::State::default())).is_err());
    }

    #[test]
    fn launch_state_stop_and_marker_failures_keep_pending_marker() {
        for (fail, state_absent, expected) in [
            (Some("launch"), true, ["marker", "launch"].as_slice()),
            (
                Some("state"),
                true,
                ["marker", "launch", "state"].as_slice(),
            ),
            (None, false, ["marker", "launch", "state"].as_slice()),
            (
                Some("stop"),
                true,
                ["marker", "launch", "state", "stop"].as_slice(),
            ),
            (
                Some("clear"),
                true,
                ["marker", "launch", "state", "stop", "clear"].as_slice(),
            ),
        ] {
            let (layout, target) = fixture();
            let ops = fake(fail, state_absent);
            assert!(execute_reset(&target, &ops).is_err());
            assert_eq!(*ops.events.borrow(), expected);
            assert!(ops.marker.get());
            fs::remove_dir_all(layout.root).unwrap();
        }
    }
}
