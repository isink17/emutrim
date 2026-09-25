use super::*;
use crate::adb::test_support::{frame, FakeAdb};
use crate::slim::profile::STATE_PATH;
use crate::slim::state::State;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Guest {
    commands: Vec<String>,
    files: HashMap<String, String>,
    settings: BTreeMap<(String, String), String>,
    disabled: BTreeSet<String>,
    failures: HashMap<String, usize>,
    failures_after: HashMap<String, usize>,
    disconnects: HashMap<String, usize>,
    disconnect_before: HashMap<String, usize>,
    fail_readback: bool,
    fail_canonical_readback: bool,
    qemu: String,
    boot: String,
}

impl Guest {
    fn new() -> Self {
        Self {
            qemu: "1".into(),
            boot: "1".into(),
            settings: BTreeMap::from([
                (("global".into(), "x".into()), "old-x".into()),
                (("global".into(), "y".into()), "old-y".into()),
            ]),
            ..Self::default()
        }
    }

    fn handle(&mut self, command: &str) -> Vec<u8> {
        self.commands.push(command.into());
        if let Some((_, remaining)) = self
            .disconnect_before
            .iter_mut()
            .find(|(key, count)| **count > 0 && command.contains(key.as_str()))
        {
            *remaining -= 1;
            return Vec::new();
        }
        if let Some((_, remaining)) = self
            .failures
            .iter_mut()
            .find(|(key, count)| **count > 0 && command.contains(key.as_str()))
        {
            *remaining -= 1;
            return [frame(2, b"injected failure"), frame(3, &[1])].concat();
        }
        let stdout = if command == "getprop ro.kernel.qemu" {
            self.qemu.clone()
        } else if command == "getprop sys.boot_completed" {
            self.boot.clone()
        } else if command == "pm list packages" {
            "package:com.google.android.apps.maps\npackage:com.google.android.youtube\n".into()
        } else if command == "pm list packages -d" {
            self.disabled
                .iter()
                .map(|package| format!("package:{package}\n"))
                .collect()
        } else if command.starts_with("if [ -e ") {
            let path = command
                .split_once("if [ -e ")
                .unwrap()
                .1
                .split_once("]; then")
                .unwrap()
                .0
                .trim();
            match self.files.get(path) {
                Some(value) => format!("present\n{value}"),
                None => "missing\n".into(),
            }
        } else if let Some(path) = command
            .strip_prefix("cat ")
            .and_then(|path| path.strip_suffix(" 2>/dev/null || true"))
            .or_else(|| command.strip_prefix("cat "))
        {
            if path.contains(".tmp.") && self.fail_readback {
                self.fail_readback = false;
                "mismatch".into()
            } else if path == STATE_PATH && self.fail_canonical_readback {
                self.fail_canonical_readback = false;
                "mismatch".into()
            } else {
                self.files.get(path).cloned().unwrap_or_default()
            }
        } else if let Some(write) = command.strip_prefix("printf %s '") {
            if let Some((value, path)) = write.split_once("' > ") {
                self.files.insert(path.into(), value.into());
            }
            String::new()
        } else if let Some(paths) = command.strip_prefix("mv ") {
            if let Some((from, to)) = paths.split_once(' ') {
                if let Some(value) = self.files.remove(from) {
                    self.files.insert(to.into(), value);
                }
            }
            String::new()
        } else if let Some(path) = command.strip_prefix("rm -f ") {
            self.files.remove(path);
            String::new()
        } else if let Some(args) = command.strip_prefix("settings get ") {
            let mut args = args.split_whitespace();
            self.settings
                .get(&(args.next().unwrap().into(), args.next().unwrap().into()))
                .cloned()
                .unwrap_or_else(|| "null".into())
        } else if let Some(args) = command.strip_prefix("settings put ") {
            let mut args = args.splitn(3, ' ');
            self.settings.insert(
                (args.next().unwrap().into(), args.next().unwrap().into()),
                args.next().unwrap_or_default().into(),
            );
            String::new()
        } else if let Some(args) = command.strip_prefix("settings delete ") {
            let mut args = args.split_whitespace();
            self.settings
                .remove(&(args.next().unwrap().into(), args.next().unwrap().into()));
            String::new()
        } else if command.starts_with("pm disable-user ") {
            self.disabled
                .insert(command.split_whitespace().last().unwrap().into());
            "new state: disabled-user".into()
        } else if command.starts_with("pm enable ") {
            self.disabled
                .remove(command.split_whitespace().last().unwrap());
            "new state: enabled".into()
        } else {
            String::new()
        };
        if let Some((_, remaining)) = self
            .failures_after
            .iter_mut()
            .find(|(key, count)| **count > 0 && command.contains(key.as_str()))
        {
            *remaining -= 1;
            return [
                frame(2, b"response lost after guest operation"),
                frame(3, &[1]),
            ]
            .concat();
        }
        if let Some((_, remaining)) = self
            .disconnects
            .iter_mut()
            .find(|(key, count)| **count > 0 && command.contains(key.as_str()))
        {
            *remaining -= 1;
            return Vec::new();
        }
        [frame(1, stdout.as_bytes()), frame(3, &[0])].concat()
    }

    fn state(&self) -> Option<State> {
        self.files.get(STATE_PATH).and_then(|text| decode(text))
    }
}

fn server(guest: Guest) -> (FakeAdb, Arc<Mutex<Guest>>) {
    let guest = Arc::new(Mutex::new(guest));
    let shared = guest.clone();
    let server = FakeAdb::start(move |command| shared.lock().unwrap().handle(command));
    (server, guest)
}

fn mutating(command: &str) -> bool {
    command.starts_with("pm disable-user ")
        || command.starts_with("pm enable ")
        || command.starts_with("settings put ")
        || command.starts_with("settings delete ")
        || command.starts_with("cmd bluetooth_manager ")
        || command.starts_with("am kill-all")
        || command.starts_with("am trim-memory")
}

fn options(dry_run: bool) -> Options {
    Options {
        dry_run,
        ..Options::default()
    }
}

#[test]
fn state_write_failure_prevents_all_guest_mutation() {
    let mut guest = Guest::new();
    guest.failures.insert("printf %s '".into(), 1);
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));
}

#[test]
fn state_readback_mismatch_prevents_all_guest_mutation() {
    let mut guest = Guest::new();
    guest.fail_readback = true;
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));
}

#[test]
fn canonical_state_verification_failure_prevents_all_guest_mutation() {
    let mut guest = Guest::new();
    guest.fail_canonical_readback = true;
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));
}

#[test]
fn unknown_existing_state_is_not_overwritten() {
    let mut guest = Guest::new();
    guest
        .files
        .insert(STATE_PATH.into(), "future-version\n".into());
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    let guest = guest.lock().unwrap();
    assert_eq!(guest.files.get(STATE_PATH).unwrap(), "future-version\n");
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
}

#[test]
fn pre_disabled_target_without_emutrim_state_is_preserved_and_refused() {
    let mut guest = Guest::new();
    guest.disabled.insert("com.google.android.apps.maps".into());
    let (server, guest) = server(guest);

    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    let guest = guest.lock().unwrap();
    assert_eq!(
        guest.disabled,
        BTreeSet::from(["com.google.android.apps.maps".into()])
    );
    assert!(!guest.files.contains_key(STATE_PATH));
    assert!(!guest.commands.iter().any(|command| mutating(command)));
}

#[test]
fn restore_without_state_is_successful_noop() {
    let (server, guest) = server(Guest::new());
    assert_eq!(restore(server.addr(), "emulator-5554").unwrap(), None);
    let guest = guest.lock().unwrap();
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn state_inspection_is_read_only_and_malformed_state_fails_closed() {
    let (server, guest) = server(Guest::new());
    assert!(inspect_state(server.addr(), "emulator-5554")
        .unwrap()
        .is_none());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));

    guest.lock().unwrap().files.insert(
        STATE_PATH.into(),
        crate::slim::state::encode(&State {
            disabled: vec!["pkg.a".into()],
            settings: BTreeMap::new(),
        }),
    );
    assert_eq!(
        inspect_state(server.addr(), "emulator-5554")
            .unwrap()
            .unwrap()
            .disabled,
        ["pkg.a"]
    );

    guest
        .lock()
        .unwrap()
        .files
        .insert(STATE_PATH.into(), "not-an-emutrim-state\n".into());
    assert!(inspect_state(server.addr(), "emulator-5554").is_err());
    let guest = guest.lock().unwrap();
    assert_eq!(
        guest.files.get(STATE_PATH).unwrap(),
        "not-an-emutrim-state\n"
    );
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
}

#[test]
fn restore_state_read_failure_remains_error() {
    let mut guest = Guest::new();
    guest.failures.insert("if [ -e ".into(), 1);
    let (server, guest) = server(guest);
    assert!(restore(server.addr(), "emulator-5554").is_err());
    let guest = guest.lock().unwrap();
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
}

#[test]
fn restore_unknown_state_remains_error_and_preserves_record() {
    let mut guest = Guest::new();
    guest
        .files
        .insert(STATE_PATH.into(), "future-version\n".into());
    let (server, guest) = server(guest);
    assert!(restore(server.addr(), "emulator-5554").is_err());
    let guest = guest.lock().unwrap();
    assert_eq!(guest.files.get(STATE_PATH).unwrap(), "future-version\n");
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
}

#[test]
fn state_lookup_failure_prevents_mutation() {
    let mut guest = Guest::new();
    guest.failures.insert("if [ -e ".into(), 1);
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));
}

#[test]
fn verified_state_precedes_first_mutation() {
    let (server, guest) = server(Guest::new());
    slim(server.addr(), "emulator-5554", &options(false)).unwrap();
    let guest = guest.lock().unwrap();
    let verified = guest
        .commands
        .iter()
        .position(|cmd| cmd == &format!("cat {STATE_PATH}"))
        .unwrap();
    let first_mutation = guest.commands.iter().position(|cmd| mutating(cmd)).unwrap();
    assert!(verified < first_mutation);
}

#[test]
fn dry_run_has_no_persistent_or_guest_mutation() {
    let (server, guest) = server(Guest::new());
    slim(server.addr(), "emulator-5554", &options(true)).unwrap();
    let guest = guest.lock().unwrap();
    assert!(!guest.commands.iter().any(|cmd| mutating(cmd)));
    assert!(!guest.commands.iter().any(|cmd| cmd.starts_with("printf ")));
    assert!(!guest.files.contains_key(STATE_PATH));
    assert!(guest.commands.iter().any(|cmd| cmd == "pm list packages"));
}

#[test]
fn slim_skips_removed_android_am_trim_memory_command() {
    let (server, guest) = server(Guest::new());
    assert_eq!(
        slim(server.addr(), "emulator-5554", &options(false)).unwrap(),
        2
    );
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|command| command == "am trim-memory --all COMPLETE"));
}

#[test]
fn already_applied_state_skips_duplicate_slim_and_detects_reverted_setting() {
    let (server, guest) = server(Guest::new());
    slim(server.addr(), "emulator-5554", &options(false)).unwrap();
    assert!(already_applied(server.addr(), "emulator-5554", &options(false)).unwrap());
    assert_eq!(
        crate::slim_after_boot(server.addr(), "emulator-5554").unwrap(),
        crate::SlimResult::AlreadyApplied
    );
    let before = guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|cmd| mutating(cmd))
        .count();
    assert!(already_applied(server.addr(), "emulator-5554", &options(false)).unwrap());
    assert_eq!(
        guest
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|cmd| mutating(cmd))
            .count(),
        before
    );
    guest.lock().unwrap().settings.insert(
        ("global".into(), "window_animation_scale".into()),
        "1".into(),
    );
    assert!(!already_applied(server.addr(), "emulator-5554", &options(false)).unwrap());
}

#[test]
fn physical_or_suspicious_emulator_identity_is_never_mutated() {
    for (serial, qemu) in [("0123ABC", "0"), ("emulator-5554", "0")] {
        let mut guest = Guest::new();
        guest.qemu = qemu.into();
        let (server, guest) = server(guest);
        assert!(slim(server.addr(), serial, &options(false)).is_err());
        assert!(!guest
            .lock()
            .unwrap()
            .commands
            .iter()
            .any(|cmd| mutating(cmd)));
    }
}

#[test]
fn partial_slim_stops_at_first_failed_package_and_keeps_recovery_record() {
    let mut guest = Guest::new();
    guest.failures.insert(
        "pm disable-user --user 0 com.google.android.apps.maps".into(),
        1,
    );
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    let snapshot = guest.lock().unwrap();
    let state = snapshot.state().unwrap();
    assert!(state
        .disabled
        .contains(&"com.google.android.apps.maps".into()));
    assert!(state
        .disabled
        .contains(&"com.google.android.youtube".into()));
    assert!(snapshot
        .commands
        .iter()
        .any(|cmd| cmd == "pm disable-user --user 0 com.google.android.youtube"));
    assert!(!snapshot
        .commands
        .iter()
        .any(|cmd| cmd.starts_with("settings put ") || cmd.starts_with("am ")));
    drop(snapshot);

    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn interrupted_slim_after_persist_before_mutation_clears_noop_record() {
    let mut guest = Guest::new();
    guest.disconnect_before.insert("pm disable-user".into(), 1);
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    {
        let guest = guest.lock().unwrap();
        assert!(guest.disabled.is_empty());
        assert!(guest.state().is_some());
    }
    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn interrupted_slim_after_packages_before_settings_restores_recorded_plan() {
    let mut guest = Guest::new();
    guest.failures_after.insert(
        "pm disable-user --user 0 com.google.android.apps.maps".into(),
        1,
    );
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert_eq!(guest.lock().unwrap().disabled.len(), 2);
    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn interrupted_slim_before_settings_restores_recorded_packages_and_settings() {
    let mut guest = Guest::new();
    guest.disconnects.insert("settings put".into(), 1);
    let (server, guest) = server(guest);
    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    {
        let guest = guest.lock().unwrap();
        assert_eq!(guest.disabled.len(), 2);
        assert_eq!(
            guest.settings.get(&("global".into(), "x".into())).unwrap(),
            "old-x"
        );
    }
    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert_eq!(
        guest.settings.get(&("global".into(), "x".into())).unwrap(),
        "old-x"
    );
    assert_eq!(
        guest.settings.get(&("global".into(), "y".into())).unwrap(),
        "old-y"
    );
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn restore_post_operation_failure_keeps_retryable_checkpoint() {
    let mut guest = Guest::new();
    let state = State {
        disabled: vec!["pkg.a".into(), "pkg.b".into()],
        settings: BTreeMap::from([(("global".into(), "x".into()), Some("old-x".into()))]),
    };
    guest
        .files
        .insert(STATE_PATH.into(), crate::slim::state::encode(&state));
    guest.disabled.extend(["pkg.a".into(), "pkg.b".into()]);
    guest.failures_after.insert("pm enable pkg.a".into(), 1);
    let (server, guest) = server(guest);

    assert!(restore(server.addr(), "emulator-5554").is_err());
    {
        let guest = guest.lock().unwrap();
        assert!(!guest.disabled.contains("pkg.a"));
        assert_eq!(guest.state().unwrap().disabled, ["pkg.a"]);
    }
    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert_eq!(
        guest.settings.get(&("global".into(), "x".into())).unwrap(),
        "old-x"
    );
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn interrupted_slim_after_all_guest_changes_keeps_valid_applied_state() {
    let mut guest = Guest::new();
    guest.disconnects.insert("am kill-all".into(), 1);
    let (server, guest) = server(guest);

    assert!(slim(server.addr(), "emulator-5554", &options(false)).is_err());
    assert!(already_applied(server.addr(), "emulator-5554", &options(false)).unwrap());
    restore(server.addr(), "emulator-5554").unwrap();
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert_eq!(
        guest.settings.get(&("global".into(), "x".into())).unwrap(),
        "old-x"
    );
    assert_eq!(
        guest.settings.get(&("global".into(), "y".into())).unwrap(),
        "old-y"
    );
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn restore_retries_cleanup_after_all_items_were_checkpointed() {
    let mut guest = Guest::new();
    guest.files.insert(
        STATE_PATH.into(),
        crate::slim::state::encode(&State::default()),
    );
    guest.disconnect_before.insert("rm -f".into(), 1);
    let (server, guest) = server(guest);

    assert!(restore(server.addr(), "emulator-5554").is_err());
    assert_eq!(guest.lock().unwrap().state(), Some(State::default()));
    assert_eq!(restore(server.addr(), "emulator-5554").unwrap(), Some(0));
    let guest = guest.lock().unwrap();
    assert!(guest.disabled.is_empty());
    assert!(!guest.files.contains_key(STATE_PATH));
}

#[test]
fn partial_restore_persists_only_unresolved_items_and_retry_skips_successes() {
    let mut guest = Guest::new();
    let state = State {
        disabled: vec!["pkg.a".into(), "pkg.b".into()],
        settings: BTreeMap::from([
            (("global".into(), "x".into()), Some("old-x".into())),
            (("global".into(), "y".into()), Some("old-y".into())),
        ]),
    };
    guest
        .files
        .insert(STATE_PATH.into(), crate::slim::state::encode(&state));
    guest.failures.insert("pm enable pkg.b".into(), 1);
    guest
        .failures
        .insert("settings put global y old-y".into(), 1);
    let (server, guest) = server(guest);

    assert!(restore(server.addr(), "emulator-5554").is_err());
    {
        let guest = guest.lock().unwrap();
        let pending = guest.state().unwrap();
        assert_eq!(pending.disabled, ["pkg.b"]);
        assert_eq!(
            pending.settings.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([("global".into(), "y".into())])
        );
    }

    restore(server.addr(), "emulator-5554").unwrap();
    let before_noop = guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|cmd| mutating(cmd))
        .count();
    assert_eq!(restore(server.addr(), "emulator-5554").unwrap(), None);
    let guest = guest.lock().unwrap();
    assert!(!guest.files.contains_key(STATE_PATH));
    assert_eq!(
        guest.commands.iter().filter(|cmd| mutating(cmd)).count(),
        before_noop
    );
    assert_eq!(
        guest
            .commands
            .iter()
            .filter(|cmd| *cmd == "pm enable pkg.a")
            .count(),
        1
    );
    assert_eq!(
        guest
            .commands
            .iter()
            .filter(|cmd| *cmd == "settings put global x old-x")
            .count(),
        1
    );
    assert_eq!(
        guest
            .commands
            .iter()
            .filter(|cmd| *cmd == "pm enable pkg.b")
            .count(),
        2
    );
    assert_eq!(
        guest
            .commands
            .iter()
            .filter(|cmd| *cmd == "settings put global y old-y")
            .count(),
        2
    );
}

#[test]
fn restore_refuses_mutation_before_boot_completion() {
    let mut guest = Guest::new();
    guest.boot = "0".into();
    let (server, guest) = server(guest);
    assert!(restore(server.addr(), "emulator-5554").is_err());
    assert!(!guest
        .lock()
        .unwrap()
        .commands
        .iter()
        .any(|cmd| mutating(cmd)));
}
