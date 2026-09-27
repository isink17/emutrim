use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Layout {
    pub root: PathBuf,
    pub managed: PathBuf,
    pub sdk: PathBuf,
    pub avd: PathBuf,
    pub tmp: PathBuf,
    pub manifest: PathBuf,
}

impl Layout {
    pub fn resolve() -> io::Result<Self> {
        Self::resolve_from(|key| env::var_os(key), env::current_dir()?, cfg!(windows))
    }

    fn resolve_from(
        mut get_env: impl FnMut(&str) -> Option<std::ffi::OsString>,
        cwd: PathBuf,
        windows: bool,
    ) -> io::Result<Self> {
        let root = match get_env("EMUTRIM_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => {
                let key = if windows { "USERPROFILE" } else { "HOME" };
                get_env(key)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("{key} unavailable; set EMUTRIM_HOME"),
                        )
                    })?
                    .join(".emutrim")
            }
        };
        let root = if root.is_absolute() {
            root
        } else {
            cwd.join(root)
        };
        let managed = root.join("managed");
        Ok(Self {
            root,
            sdk: managed.join("sdk"),
            avd: managed.join("avd"),
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
            managed,
        })
    }
}

pub fn run(args: Vec<String>) -> io::Result<()> {
    let action = args.first().map(String::as_str).unwrap_or("help");
    let layout = Layout::resolve()?;
    match action {
        "root" if args.len() == 1 => {
            println!("root {}", layout.root.display());
            println!("managed {}", layout.managed.display());
            println!("sdk {}", layout.sdk.display());
            println!("avd {}", layout.avd.display());
            println!("tmp {}", layout.tmp.display());
            println!("manifest {}", layout.manifest.display());
            Ok(())
        }
        "status" if args.len() == 1 => status(&layout),
        "clean" => {
            if args.iter().skip(1).any(|arg| arg != "--yes") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: emutrim managed clean [--yes]",
                ));
            }
            clean(&layout, args.iter().skip(1).any(|arg| arg == "--yes"))
        }
        "setup" if args.len() == 1 => setup(&layout),
        "help" if args.len() == 1 => {
            println!("managed root|status|setup|clean [--yes]\nDefault EmuTrim managed root: ~/.emutrim\nOverride: EMUTRIM_HOME");
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: emutrim managed root|status|setup|clean [--yes]",
        )),
    }
}

fn status(layout: &Layout) -> io::Result<()> {
    println!("managed root: {}", layout.root.display());
    println!("managed SDK: {}", layout.sdk.display());
    println!("managed AVD home: {}", layout.avd.display());
    println!(
        "Emulator: {}",
        crate::avd::emulator_path(&layout.sdk).is_file()
    );
    let adb = layout
        .sdk
        .join("platform-tools")
        .join(if cfg!(windows) { "adb.exe" } else { "adb" });
    println!("Platform-Tools: {}", adb.is_file());
    for entry in read_dirs(&layout.sdk.join("system-images"))? {
        println!("system image: {}", entry.display());
    }
    for entry in read_dirs(&layout.avd)? {
        if entry.extension().is_some_and(|ext| ext == "avd") {
            println!(
                "AVD: {}",
                entry.file_stem().unwrap_or_default().to_string_lossy()
            );
        }
    }
    println!("logical disk usage: {} bytes", disk_usage(&layout.managed)?);
    match fs::read_to_string(&layout.manifest) {
        Ok(data) => println!(
            "manifest: {}",
            data.lines()
                .find(|line| line.contains("\"schema\""))
                .unwrap_or("schema unavailable")
                .trim()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("manifest: absent"),
        Err(e) => return Err(e),
    }
    Ok(())
}

fn read_dirs(path: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(path) {
        Ok(entries) => entries.map(|e| e.map(|entry| entry.path())).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn disk_usage(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed tree contains symlink; refusing unsafe traversal",
        ));
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    read_dirs(path)?.iter().try_fold(0u64, |n, child| {
        n.checked_add(disk_usage(child)?)
            .ok_or_else(|| io::Error::other("disk usage overflow"))
    })
}

fn clean(layout: &Layout, yes: bool) -> io::Result<()> {
    let size = disk_usage(&layout.managed)?;
    println!("would remove {} ({size} bytes)", layout.managed.display());
    if !yes {
        return Ok(());
    }
    let active = running_managed_emulators(layout)?;
    if !active.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("managed Emulator running; PID(s): {}", active.join(", ")),
        ));
    }
    if layout.managed.exists() {
        let canonical_root = layout.root.canonicalize()?;
        if layout.managed.exists() && !layout.managed.canonicalize()?.starts_with(&canonical_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed path escapes EmuTrim root",
            ));
        }
        fs::remove_dir_all(&layout.managed)?;
    }
    println!("managed payloads removed");
    Ok(())
}

fn emulator_pids(processes: &str, emulator_dir: &Path) -> Vec<String> {
    let root = emulator_dir.to_string_lossy();
    processes
        .lines()
        .filter_map(|line| {
            let pid = line.split_whitespace().next()?;
            (pid.bytes().all(|byte| byte.is_ascii_digit()) && line.contains(root.as_ref()))
                .then(|| pid.to_owned())
        })
        .collect()
}

#[cfg(windows)]
fn running_managed_emulators(layout: &Layout) -> io::Result<Vec<String>> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-CimInstance Win32_Process | ForEach-Object { if ($_.ExecutablePath) { \"$($_.ProcessId)|$($_.ExecutablePath)\" } }"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "failed to inspect running Windows processes",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, path) = line.split_once('|')?;
            Path::new(path)
                .starts_with(layout.sdk.join("emulator"))
                .then(|| pid.to_owned())
        })
        .collect())
}

#[cfg(unix)]
fn running_managed_emulators(layout: &Layout) -> io::Result<Vec<String>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "failed to inspect running managed Emulator processes",
        ));
    }
    Ok(emulator_pids(
        &String::from_utf8_lossy(&output.stdout),
        &layout.sdk.join("emulator"),
    ))
}

fn setup(layout: &Layout) -> io::Result<()> {
    if !cfg!(target_os = "macos") || !cfg!(target_arch = "aarch64") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed setup currently supports macOS Apple Silicon only",
        ));
    }
    fs::create_dir_all(&layout.tmp)?;
    fs::create_dir_all(&layout.avd)?;
    let external_sdk = crate::avd::sdk_dir()?;
    if external_sdk
        .canonicalize()?
        .starts_with(layout.managed.canonicalize()?)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bootstrap sdkmanager must come from an external user SDK",
        ));
    }
    let tools = external_sdk.join("cmdline-tools");
    let bootstrap = read_dirs(&tools)?
        .into_iter()
        .map(|version| version.join("bin/sdkmanager"))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("bootstrap sdkmanager not found under {}", tools.display()),
            )
        })?;
    let listing = Command::new(&bootstrap)
        .arg(format!("--sdk_root={}", layout.sdk.display()))
        .arg("--list")
        .env("ANDROID_HOME", &layout.sdk)
        .env("ANDROID_SDK_ROOT", &layout.sdk)
        .env("ANDROID_AVD_HOME", &layout.avd)
        .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
        .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
        .env("TMPDIR", &layout.tmp)
        .output()?;
    if !listing.status.success() {
        return Err(io::Error::other(format!(
            "sdkmanager --list failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&listing.stdout);
    let image = text
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let package = line.split_whitespace().next()?;
            (package.starts_with("system-images;android-")
                && package.contains(";google_apis;arm64-v8a")
                && !package.to_ascii_lowercase().contains("preview"))
            .then_some(package)
        })
        .max_by_key(|line| {
            line.split(';')
                .nth(1)
                .and_then(|api| api.strip_prefix("android-"))
                .and_then(|api| api.parse::<u32>().ok())
                .unwrap_or(0)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no stable google_apis arm64-v8a system image listed by sdkmanager",
            )
        })?;
    let packages = ["cmdline-tools;latest", "platform-tools", "emulator", image];
    for package in packages {
        let result = Command::new(&bootstrap)
            .arg(format!("--sdk_root={}", layout.sdk.display()))
            .arg(package)
            .env("ANDROID_HOME", &layout.sdk)
            .env("ANDROID_SDK_ROOT", &layout.sdk)
            .env("ANDROID_AVD_HOME", &layout.avd)
            .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
            .env("TMPDIR", &layout.tmp)
            .status()?;
        if !result.success() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("sdkmanager failed for {package}; accept required licenses explicitly with `{} --sdk_root={} --licenses`", bootstrap.display(), layout.sdk.display())));
        }
        let installed = match package {
            "cmdline-tools;latest" => layout
                .sdk
                .join("cmdline-tools/latest/bin/avdmanager")
                .is_file(),
            "platform-tools" => layout.sdk.join("platform-tools/adb").is_file(),
            "emulator" => crate::avd::emulator_path(&layout.sdk).is_file(),
            _ => layout
                .sdk
                .join(package.replace(';', "/"))
                .join("package.xml")
                .is_file(),
        };
        if !installed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("SDK package {package} is absent; if sdkmanager reported an unaccepted license, accept it with `{} --sdk_root={} --licenses`", bootstrap.display(), layout.sdk.display()),
            ));
        }
    }
    let name = "EmuTrim_Mac_Acceptance_20260927";
    let avd_path = layout.avd.join(format!("{name}.avd"));
    let avd_ini = layout.avd.join(format!("{name}.ini"));
    if avd_path.exists() || avd_ini.exists() {
        let manifest = fs::read_to_string(&layout.manifest).unwrap_or_default();
        let config = avd_path.join("config.ini");
        if !manifest.lines().any(|line| line.trim() == "\"schema\": 1,")
            || !manifest
                .lines()
                .any(|line| line.trim() == format!("\"avds\": [\"{name}\"]"))
            || !manifest
                .lines()
                .any(|line| line.trim() == format!("\"system_image\": \"{image}\","))
            || !config.is_file()
            || !fs::read_to_string(&config)?.contains(&image.replace(';', "/"))
            || crate::avd::config_path_mode(name, true).is_err()
        {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("managed AVD {name} exists without matching EmuTrim manifest and config; refusing overwrite")));
        }
    } else {
        let manager = layout.sdk.join("cmdline-tools/latest/bin/avdmanager");
        if !manager.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("avdmanager missing: {}", manager.display()),
            ));
        }
        let output = Command::new(manager)
            .args([
                "create",
                "avd",
                "--name",
                name,
                "--package",
                image,
                "--path",
            ])
            .arg(&avd_path)
            .arg("--device")
            .arg("pixel_2")
            .env("ANDROID_HOME", &layout.sdk)
            .env("ANDROID_SDK_ROOT", &layout.sdk)
            .env("ANDROID_AVD_HOME", &layout.avd)
            .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
            .env("TMPDIR", &layout.tmp)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "avdmanager failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if !avd_path.join("config.ini").is_file()
            || !avd_ini.is_file()
            || crate::avd::config_path_mode(name, true).is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "AVD creation did not produce config.ini and managed .ini under managed AVD home",
            ));
        }
    }
    let manifest = format!("{{\n  \"schema\": 1,\n  \"sdk_packages\": [\"cmdline-tools;latest\", \"platform-tools\", \"emulator\", \"{image}\"],\n  \"system_image\": \"{image}\",\n  \"arch\": \"arm64-v8a\",\n  \"host\": \"macos-aarch64\",\n  \"avds\": [\"{name}\"]\n}}\n");
    fs::write(&layout.manifest, manifest)?;
    println!("managed AVD ready: {}", avd_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_uses_override_and_platform_defaults() {
        let override_layout = Layout::resolve_from(
            |key| (key == "EMUTRIM_HOME").then(|| "/tmp/custom".into()),
            PathBuf::from("/cwd"),
            false,
        )
        .unwrap();
        assert_eq!(override_layout.root, PathBuf::from("/tmp/custom"));
        assert_eq!(
            override_layout.sdk,
            PathBuf::from("/tmp/custom/managed/sdk")
        );
        assert_eq!(
            Layout::resolve_from(
                |key| (key == "HOME").then(|| "/Users/test".into()),
                PathBuf::from("/cwd"),
                false,
            )
            .unwrap()
            .root,
            PathBuf::from("/Users/test/.emutrim")
        );
        assert_eq!(
            Layout::resolve_from(
                |key| (key == "USERPROFILE").then(|| "/Users/test".into()),
                PathBuf::from("/cwd"),
                true,
            )
            .unwrap()
            .root,
            PathBuf::from("/Users/test/.emutrim")
        );
        assert!(Layout::resolve_from(|_| None, PathBuf::from("/cwd"), false).is_err());
    }

    #[test]
    fn clean_dry_run_preserves_tree_and_symlink_is_rejected() {
        let root = env::temp_dir().join(format!("emutrim-managed-{}", std::process::id()));
        let managed = root.join("managed");
        fs::create_dir_all(&managed).unwrap();
        fs::write(managed.join("payload"), "test").unwrap();
        let layout = Layout {
            root: root.clone(),
            managed: managed.clone(),
            sdk: managed.join("sdk"),
            avd: managed.join("avd"),
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
        };
        clean(&layout, false).unwrap();
        assert!(managed.join("payload").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = root.with_extension("outside");
            fs::create_dir_all(&outside).unwrap();
            symlink(&outside, managed.join("escape")).unwrap();
            assert!(clean(&layout, true).is_err());
            assert!(outside.exists());
            fs::remove_dir_all(outside).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_scan_finds_qemu_under_managed_emulator_package() {
        let processes = "14454 1 /Users/test/.emutrim/managed/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64 -avd Test\n14500 1 /Applications/Android Studio.app/emulator/qemu-system-aarch64\n";
        assert_eq!(
            emulator_pids(
                processes,
                Path::new("/Users/test/.emutrim/managed/sdk/emulator")
            ),
            ["14454"]
        );
    }
}
