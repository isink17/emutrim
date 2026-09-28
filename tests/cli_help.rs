use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn emutrim(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_emutrim"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn command_help_succeeds_and_describes_supported_syntax() {
    for (args, expected) in [
        (&["start", "--help"][..], "--cold-boot"),
        (&["doctor", "--help"], "--serial=SERIAL"),
        (&["slim", "--help"], "--dry-run"),
        (&["restore", "--help"], "off is an alias"),
        (&["watch", "--help"], "--skip=GROUP"),
        (&["stats", "--help"], "--seconds=N"),
        (&["tune-avd", "--help"], "4096 MB for detected 16 KB images"),
        (&["list-avds", "--help"], "Usage: emutrim list-avds"),
        (&["managed", "--help"], "root|status|setup|clean"),
        (&["managed", "root", "--help"], "managed root"),
        (&["managed", "status", "--help"], "managed status"),
        (&["managed", "setup", "--help"], "Apple Silicon macOS"),
        (
            &["managed", "clean", "--help"],
            "previews managed payload cleanup",
        ),
    ] {
        let output = emutrim(args);
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
}

#[test]
fn help_does_not_clean_managed_files_and_invalid_arguments_still_fail() {
    let root = std::env::temp_dir().join(format!(
        "emutrim-help-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let marker = root.join("managed/keep");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, "keep").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_emutrim"))
        .args(["managed", "clean", "--yes", "--help"])
        .env("EMUTRIM_HOME", &root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(marker.is_file());

    assert!(!emutrim(&["start", "--unknown"]).status.success());
    fs::remove_dir_all(root).unwrap();
}
