use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub mod console;

#[derive(Clone, Debug)]
pub struct AvdInfo {
    pub api: String,
    pub abi: String,
    pub tag: String,
    pub ram_mb: u32,
    pub image: PathBuf,
    pub is_16k: bool,
    pub gpu_mode: String,
    pub gpu_enabled: String,
    pub backup_exists: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotMode {
    Normal,
    ColdBoot,
    Reset,
}

pub fn sdk_dir() -> io::Result<PathBuf> {
    sdk_dir_from(|key| env::var_os(key), |path| path.is_dir())
}
pub fn sdk_dir_mode(managed: bool) -> io::Result<PathBuf> {
    if managed {
        Ok(crate::managed::Layout::resolve()?.sdk)
    } else {
        sdk_dir()
    }
}

fn sdk_dir_from(
    mut get_env: impl FnMut(&str) -> Option<std::ffi::OsString>,
    is_dir: impl Fn(&std::path::Path) -> bool,
) -> io::Result<PathBuf> {
    for key in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(value) = get_env(key) {
            let path = PathBuf::from(value);
            if is_dir(&path) {
                return Ok(path);
            }
        }
    }
    let fallback = if cfg!(windows) {
        get_env("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("Android").join("Sdk"))
    } else {
        get_env("HOME")
            .map(PathBuf::from)
            .map(|path| path.join("Library").join("Android").join("sdk"))
    };
    if let Some(path) = fallback.filter(|path| is_dir(path)) {
        Ok(path)
    } else {
        let checked = if cfg!(windows) {
            "%LOCALAPPDATA%\\Android\\Sdk"
        } else {
            "$HOME/Library/Android/sdk"
        };
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Android SDK not found; checked ANDROID_HOME, ANDROID_SDK_ROOT, and {checked}"),
        ))
    }
}
pub fn avd_base() -> io::Result<PathBuf> {
    avd_base_from(|key| env::var_os(key))
}
fn avd_base_mode(managed: bool) -> io::Result<PathBuf> {
    if managed {
        Ok(crate::managed::Layout::resolve()?.avd)
    } else {
        avd_base()
    }
}

fn avd_base_from(
    mut get_env: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    if let Some(path) = get_env("ANDROID_AVD_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(home) = get_env("ANDROID_USER_HOME") {
        return Ok(PathBuf::from(home).join("avd"));
    }
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    get_env(home_key)
        .map(PathBuf::from)
        .map(|home| home.join(".android").join("avd"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{home_key} unavailable")))
}

pub fn emulator_path(sdk: &std::path::Path) -> PathBuf {
    let name = if cfg!(windows) {
        "emulator.exe"
    } else {
        "emulator"
    };
    sdk.join("emulator").join(name)
}
pub(crate) fn config_path_mode(name: &str, managed: bool) -> io::Result<PathBuf> {
    if managed
        && !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid managed AVD name",
        ));
    }
    let base = avd_base_mode(managed)?;
    let avd_dir = base.join(format!("{name}.avd"));
    if managed {
        let ini = base.join(format!("{name}.ini"));
        let ini_text = fs::read_to_string(&ini)?;
        let recorded = ini_text
            .lines()
            .find_map(|line| line.strip_prefix("path="))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "managed AVD .ini has no path")
            })?;
        if PathBuf::from(recorded).canonicalize()? != avd_dir.canonicalize()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed AVD .ini path escapes managed AVD home",
            ));
        }
    }
    let path = avd_dir.join("config.ini");
    if path.is_file() {
        if managed && !path.canonicalize()?.starts_with(base.canonicalize()?) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed AVD config escapes managed AVD home",
            ));
        }
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("AVD {name:?} not found"),
        ))
    }
}

pub(crate) fn parse_numeric_version(value: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = value
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect();
    parts.filter(|parts| !parts.is_empty())
}

pub(crate) fn minimum_ram_mb(api: &str, is_16k: bool) -> Option<u32> {
    let version = parse_numeric_version(api)?;
    Some(if is_16k || version[0] >= 37 {
        4096
    } else {
        1536
    })
}
pub fn list_mode(managed: bool) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(avd_base_mode(managed)?)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".avd"))
            {
                if if managed {
                    config_path_mode(name, true).is_ok()
                } else {
                    entry.path().join("config.ini").is_file()
                } {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}
fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect()
}
pub fn is_16k(config: &BTreeMap<String, String>) -> bool {
    ["tag.id", "image.sysdir.1"]
        .iter()
        .filter_map(|k| config.get(*k))
        .any(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("ps16k") || v.contains("16kb") || v.contains("page_size_16kb")
        })
}
pub fn inspect_mode(name: &str, managed: bool) -> io::Result<AvdInfo> {
    let path = config_path_mode(name, managed)?;
    let config = parse(&fs::read_to_string(&path)?);
    let image_relative = config.get("image.sysdir.1").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD config has no system image path",
        )
    })?;
    let image_relative_path = std::path::Path::new(image_relative);
    if managed
        && (image_relative_path.is_absolute()
            || image_relative_path
                .components()
                .any(|component| component == std::path::Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed AVD system image path escapes managed SDK",
        ));
    }
    let image = sdk_dir_mode(managed)?.join(image_relative);
    let ram_mb = config
        .get("hw.ramSize")
        .and_then(|value| parse_ram_mb(value))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AVD RAM is invalid"))?;
    Ok(AvdInfo {
        api: config
            .get("target")
            .and_then(|value| value.strip_prefix("android-"))
            .unwrap_or("unknown")
            .into(),
        abi: config
            .get("abi.type")
            .cloned()
            .unwrap_or_else(|| "unknown".into()),
        tag: config
            .get("tag.id")
            .cloned()
            .unwrap_or_else(|| "unknown".into()),
        ram_mb,
        image,
        is_16k: is_16k(&config),
        gpu_mode: config
            .get("hw.gpu.mode")
            .cloned()
            .unwrap_or_else(|| "unset".into()),
        gpu_enabled: config
            .get("hw.gpu.enabled")
            .cloned()
            .unwrap_or_else(|| "unset".into()),
        backup_exists: path.with_extension("ini.emutrim.bak").is_file(),
    })
}

fn parse_ram_mb(value: &str) -> Option<u32> {
    if let Ok(mb) = value.parse() {
        return Some(mb);
    }
    let (amount, unit) = value.split_at(value.len().checked_sub(1)?);
    let amount = amount.parse::<u32>().ok()?;
    match unit.to_ascii_uppercase().as_str() {
        "G" => amount.checked_mul(1024),
        "M" => Some(amount),
        _ => None,
    }
}

pub fn validate_ram(info: &AvdInfo, ram_mb: u32) -> io::Result<()> {
    validate_ram_for_image(&info.api, info.is_16k, ram_mb)
}

fn validate_ram_for_image(api: &str, is_16k: bool, ram_mb: u32) -> io::Result<()> {
    let minimum = minimum_ram_mb(api, is_16k).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD Android API metadata is invalid",
        )
    })?;
    if ram_mb < minimum {
        let message = if is_16k {
            "16 KB system image requires at least 4096 MB; refusing incompatible --ram"
        } else if minimum == 4096 {
            "API 37+ phone AVD requires at least 4096 MB RAM"
        } else {
            "emulator RAM must be between 1536 and 8192 MB"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    }
    if !(1536..=8192).contains(&ram_mb) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "emulator RAM must be between 1536 and 8192 MB",
        ));
    }
    Ok(())
}

pub(crate) fn repair_managed_ram_config(path: &Path) -> io::Result<bool> {
    let text = fs::read_to_string(path)?;
    let config = parse(&text);
    let api = config
        .get("target")
        .and_then(|target| target.strip_prefix("android-"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "AVD Android API metadata is invalid",
            )
        })?;
    let minimum = minimum_ram_mb(api, is_16k(&config)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD Android API metadata is invalid",
        )
    })?;
    let current = config
        .get("hw.ramSize")
        .and_then(|value| parse_ram_mb(value))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AVD RAM is invalid"))?;
    if current >= minimum {
        return Ok(false);
    }

    let mut updated = String::with_capacity(text.len());
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let content = body.strip_suffix('\r').unwrap_or(body);
        if content
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "hw.ramSize")
        {
            updated.push_str("hw.ramSize=");
            updated.push_str(&minimum.to_string());
            updated.push_str(if body.ends_with('\r') {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            });
            replaced = true;
        } else {
            updated.push_str(line);
        }
    }
    if !replaced {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD RAM is invalid",
        ));
    }
    fs::write(path, updated)?;
    Ok(true)
}

pub fn available_console_port() -> io::Result<u16> {
    find_console_port(|port| {
        let console = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let adb = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port + 1));
        if let (Ok(console), Ok(adb)) = (console, adb) {
            drop((console, adb));
            true
        } else {
            false
        }
    })
}

fn find_console_port(mut pair_available: impl FnMut(u16) -> bool) -> io::Result<u16> {
    for port in (5554..=5682).step_by(2) {
        if pair_available(port) {
            return Ok(port);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "no free Android Emulator console/ADB port pair in 5554..=5682",
    ))
}

pub fn console_port(serial: &str) -> Option<u16> {
    let port = serial.strip_prefix("emulator-")?.parse::<u16>().ok()?;
    ((5554..=5682).contains(&port) && port % 2 == 0).then_some(port)
}
pub fn tune_mode(name: &str, ram: Option<u32>, managed: bool) -> io::Result<()> {
    tune_file(&config_path_mode(name, managed)?, ram)
}
fn tune_file(path: &PathBuf, requested_ram: Option<u32>) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let config = parse(&text);
    if config
        .get("image.sysdir.1")
        .is_none_or(|image| image.is_empty())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD config has no system image path",
        ));
    }
    let api = config
        .get("target")
        .and_then(|target| target.strip_prefix("android-"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "AVD Android API metadata is invalid",
            )
        })?;
    let is_16k = is_16k(&config);
    let minimum = minimum_ram_mb(api, is_16k).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD Android API metadata is invalid",
        )
    })?;
    let ram = requested_ram.unwrap_or(minimum);
    validate_ram_for_image(api, is_16k, ram)?;
    let backup = path.with_extension("ini.emutrim.bak");
    if !backup.exists() {
        fs::write(&backup, &text)?;
    }
    let updates = [
        ("hw.ramSize", ram.to_string()),
        ("hw.gpu.mode", "host".into()),
        ("hw.gpu.enabled", "yes".into()),
    ];
    let mut seen = BTreeMap::new();
    let mut out = String::new();
    for line in text.lines() {
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            if let Some((_, value)) = updates.iter().find(|(candidate, _)| *candidate == key) {
                out.push_str(&format!("{key}={value}\n"));
                seen.insert(key, true);
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    for (key, value) in &updates {
        if !seen.contains_key(key) {
            out.push_str(&format!("{key}={value}\n"));
        }
    }
    fs::write(path, out)
}
pub fn start_mode(
    name: &str,
    ram: u32,
    port: u16,
    snapshot: SnapshotMode,
    headless: bool,
    managed: bool,
) -> io::Result<Child> {
    let info = inspect_mode(name, managed)?;
    validate_ram(&info, ram)?;
    if !info.image.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "AVD system image directory not found: {}",
                info.image.display()
            ),
        ));
    }
    let sdk = sdk_dir_mode(managed)?;
    let emulator = emulator_path(&sdk);
    if !emulator.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Emulator binary not found: {}", emulator.display()),
        ));
    }
    let mut command = Command::new(emulator);
    let layout = if managed {
        Some(crate::managed::Layout::resolve()?)
    } else {
        None
    };
    let datadir = layout
        .as_ref()
        .map(|layout| layout.avd.join(format!("{name}.avd")));
    let lowram_eligible =
        minimum_ram_mb(&info.api, info.is_16k).is_some_and(|minimum| minimum < 4096);
    command.args(launch_args(
        name,
        port,
        datadir.as_deref(),
        ram,
        snapshot,
        headless,
        lowram_eligible,
    ));
    if let Some(layout) = layout {
        crate::managed::ensure_emulator_home(&layout)?;
        command
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_SDK_ROOT", &sdk)
            .env("ANDROID_AVD_HOME", &layout.avd)
            .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
            .env("TMPDIR", &layout.tmp);
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.spawn()
}

fn launch_args(
    name: &str,
    port: u16,
    datadir: Option<&Path>,
    ram: u32,
    snapshot: SnapshotMode,
    headless: bool,
    lowram_eligible: bool,
) -> Vec<String> {
    let mut args = vec![
        "-avd".into(),
        name.into(),
        "-port".into(),
        port.to_string(),
        "-gpu".into(),
        "host".into(),
    ];
    if let Some(datadir) = datadir {
        args.push("-datadir".into());
        args.push(datadir.to_string_lossy().into_owned());
    }
    args.extend(["-memory".into(), ram.to_string()]);
    match snapshot {
        SnapshotMode::Normal => {}
        SnapshotMode::ColdBoot => args.push("-no-snapshot".into()),
        SnapshotMode::Reset => args.extend(["-wipe-data".into(), "-no-snapshot-load".into()]),
    }
    if headless {
        args.push("-no-window".into());
    }
    if lowram_eligible {
        args.push("-lowram".into());
    }
    args
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sdk_discovery_prefers_android_home_then_sdk_root() {
        let env = |key: &str| match key {
            "ANDROID_HOME" => Some("preferred".into()),
            "ANDROID_SDK_ROOT" => Some("legacy".into()),
            _ => None,
        };
        assert_eq!(
            sdk_dir_from(env, |path| path == std::path::Path::new("preferred")).unwrap(),
            PathBuf::from("preferred")
        );
        assert_eq!(
            sdk_dir_from(env, |path| path == std::path::Path::new("legacy")).unwrap(),
            PathBuf::from("legacy")
        );
    }

    #[test]
    fn sdk_discovery_uses_host_default() {
        let expected = if cfg!(windows) {
            PathBuf::from("local/Android/Sdk")
        } else {
            PathBuf::from("home/Library/Android/sdk")
        };
        let env = |key: &str| match key {
            "HOME" => Some("home".into()),
            "LOCALAPPDATA" => Some("local".into()),
            _ => None,
        };
        let resolved = sdk_dir_from(env, |path| path == expected).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn avd_discovery_honors_overrides_and_host_home() {
        assert_eq!(
            avd_base_from(|key| (key == "ANDROID_AVD_HOME").then(|| "explicit".into())).unwrap(),
            PathBuf::from("explicit")
        );
        assert_eq!(
            avd_base_from(|key| (key == "ANDROID_USER_HOME").then(|| "user".into())).unwrap(),
            PathBuf::from("user/avd")
        );
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        assert_eq!(
            avd_base_from(|key| (key == home_key).then(|| "home".into())).unwrap(),
            PathBuf::from("home/.android/avd")
        );
        assert!(avd_base_from(|_| None).is_err());
    }

    #[test]
    fn emulator_binary_name_matches_host() {
        let name = if cfg!(windows) {
            "emulator.exe"
        } else {
            "emulator"
        };
        assert_eq!(
            emulator_path(std::path::Path::new("sdk")),
            PathBuf::from("sdk/emulator").join(name)
        );
    }
    #[test]
    fn detects_16k() {
        assert!(is_16k(&parse(
            "image.sysdir.1=system-images;android-37;google_apis_ps16k;x86_64\n"
        )));
    }
    #[test]
    fn console_serial_parser_accepts_only_even_console_ports() {
        assert_eq!(console_port("emulator-5556"), Some(5556));
        assert_eq!(console_port("emulator-5557"), None);
        assert_eq!(console_port("0123ABC"), None);
    }

    #[test]
    fn headless_only_adds_no_window_and_composes_with_start_options() {
        let api36_lowram = minimum_ram_mb("36", false).is_some_and(|minimum| minimum < 4096);
        let normal = launch_args(
            "Alpha",
            5554,
            None,
            2048,
            SnapshotMode::Normal,
            false,
            api36_lowram,
        );
        assert!(!normal.contains(&"-no-window".into()));
        let headless = launch_args(
            "Alpha",
            5554,
            None,
            2048,
            SnapshotMode::ColdBoot,
            true,
            api36_lowram,
        );
        assert_eq!(
            headless.iter().filter(|arg| *arg == "-no-window").count(),
            1
        );
        assert!(headless.contains(&"-no-snapshot".into()));
        assert!(headless.contains(&"-lowram".into()));
        let api37_lowram = minimum_ram_mb("37.0", false).is_some_and(|minimum| minimum < 4096);
        assert!(!launch_args(
            "Alpha",
            5554,
            None,
            4096,
            SnapshotMode::Normal,
            true,
            api37_lowram,
        )
        .contains(&"-lowram".into()));
        let api37 = launch_args("Alpha", 5554, None, 4096, SnapshotMode::Normal, true, false);
        assert!(!api37.contains(&"-lowram".into()));
    }

    #[test]
    fn reset_args_wipe_data_and_cold_boot_without_disabling_snapshot_save() {
        let datadir = PathBuf::from("/managed/avd/Alpha.avd");
        let args = launch_args(
            "Alpha",
            5556,
            Some(&datadir),
            4096,
            SnapshotMode::Reset,
            false,
            false,
        );
        assert_eq!(
            args[..10],
            [
                "-avd",
                "Alpha",
                "-port",
                "5556",
                "-gpu",
                "host",
                "-datadir",
                "/managed/avd/Alpha.avd",
                "-memory",
                "4096"
            ]
        );
        assert_eq!(args.iter().filter(|arg| *arg == "-wipe-data").count(), 1);
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "-no-snapshot-load")
                .count(),
            1
        );
        assert!(!args.contains(&"-no-snapshot".into()));
        assert!(!args.contains(&"-no-window".into()));
        assert!(!args.contains(&"-lowram".into()));
    }

    #[test]
    fn port_selection_skips_collisions_and_uses_even_pairs() {
        let mut seen = Vec::new();
        let port = find_console_port(|port| {
            seen.push(port);
            port == 5558
        })
        .unwrap();
        assert_eq!(port, 5558);
        assert_eq!(seen, [5554, 5556, 5558]);
    }

    #[test]
    fn ram_validation_enforces_api_image_and_emulator_limits() {
        let info = AvdInfo {
            api: "36".into(),
            abi: "x86_64".into(),
            tag: "google_apis".into(),
            ram_mb: 4096,
            image: PathBuf::new(),
            is_16k: false,
            gpu_mode: "host".into(),
            gpu_enabled: "yes".into(),
            backup_exists: false,
        };
        assert!(validate_ram(&info, 2048).is_ok());
        let api37 = AvdInfo {
            api: "37".into(),
            ..info.clone()
        };
        assert!(validate_ram(&api37, 2048).is_err());
        assert!(validate_ram(&api37, 4096).is_ok());
        let api37dot0 = AvdInfo {
            api: "37.0".into(),
            ..info.clone()
        };
        assert!(validate_ram(&api37dot0, 4096).is_ok());
        let api38 = AvdInfo {
            api: "38".into(),
            ..info.clone()
        };
        assert!(validate_ram(&api38, 3072).is_err());
        let old_16k = AvdInfo {
            is_16k: true,
            ..info.clone()
        };
        assert!(validate_ram(&old_16k, 3072).is_err());
        let malformed = AvdInfo {
            api: "37-ext1".into(),
            ..info.clone()
        };
        assert!(validate_ram(&malformed, 4096).is_err());
        assert!(validate_ram(&info, 4096).is_ok());
    }

    #[test]
    fn numeric_android_versions_are_strict_and_drive_ram_minimum() {
        for api in ["37", "37.0", "37.1", "38"] {
            assert_eq!(minimum_ram_mb(api, false), Some(4096));
        }
        assert_eq!(minimum_ram_mb("36", false), Some(1536));
        assert_eq!(minimum_ram_mb("36", true), Some(4096));
        for api in ["", "beta", "37-ext1", "canary", "4294967296"] {
            assert_eq!(minimum_ram_mb(api, false), None);
        }
    }
    #[test]
    fn parser_ignores_malformed() {
        assert!(parse("broken\na=b\n").contains_key("a"));
    }

    #[test]
    fn parses_android_ram_units_as_megabytes() {
        assert_eq!(parse_ram_mb("2048"), Some(2048));
        assert_eq!(parse_ram_mb("2G"), Some(2048));
        assert_eq!(parse_ram_mb("512M"), Some(512));
        assert_eq!(parse_ram_mb("4294967295G"), None);
        assert_eq!(parse_ram_mb("invalid"), None);
    }
    #[test]
    fn tune_preserves_unrelated_config_and_backup() {
        let path = env::temp_dir().join(format!(
            "emutrim-{}-config.ini",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            "hw.ramSize=2048\ntarget=android-31\ncustom.key=keep\nhw.gpu.mode=auto\nhw.audioInput=no\nhw.camera.back=none\nimage.sysdir.1=system-images;android-31;google_apis;x86_64\n",
        )
        .unwrap();
        tune_file(&path, Some(1536)).unwrap();
        let edited = fs::read_to_string(&path).unwrap();
        assert!(edited.contains("hw.ramSize=1536\n"));
        assert!(edited.contains("hw.gpu.mode=host\n"));
        assert!(edited.contains("custom.key=keep\n"));
        assert!(edited.contains("hw.audioInput=no\n"));
        assert!(edited.contains("hw.camera.back=none\n"));
        assert_eq!(
            fs::read_to_string(path.with_extension("ini.emutrim.bak")).unwrap(),
            "hw.ramSize=2048\ntarget=android-31\ncustom.key=keep\nhw.gpu.mode=auto\nhw.audioInput=no\nhw.camera.back=none\nimage.sysdir.1=system-images;android-31;google_apis;x86_64\n"
        );
        let _ = fs::remove_file(path.with_extension("ini.emutrim.bak"));
        let _ = fs::remove_file(path);
    }

    fn temp_config(text: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "emutrim-{}-config.ini",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn tune_defaults_ram_from_detected_image_page_size() {
        for (target, image, expected) in [
            (
                "android-31",
                "system-images;android-31;google_apis;x86_64",
                "1536",
            ),
            (
                "android-37.0",
                "system-images;android-37.0;google_apis;x86_64",
                "4096",
            ),
            (
                "android-37.1",
                "system-images;android-37.1;google_apis_ps16k;x86_64",
                "4096",
            ),
        ] {
            let path = temp_config(&format!("target={target}\nimage.sysdir.1={image}\n"));
            tune_file(&path, None).unwrap();
            assert!(fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("hw.ramSize={expected}\n")));
            let _ = fs::remove_file(path.with_extension("ini.emutrim.bak"));
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn tune_accepts_explicit_ram_and_rejects_invalid_16k_values_before_backup() {
        let path = temp_config(
            "target=android-37.0\nimage.sysdir.1=system-images;android-37.0;google_apis_ps16k;x86_64\n",
        );
        let original = fs::read_to_string(&path).unwrap();
        assert!(tune_file(&path, Some(1536)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("ini.emutrim.bak").exists());
        tune_file(&path, Some(5120)).unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("hw.ramSize=5120\n"));
        let _ = fs::remove_file(path.with_extension("ini.emutrim.bak"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn tune_api37_rejects_explicit_low_ram_without_backup() {
        let path = temp_config(
            "target=android-37.0\nimage.sysdir.1=system-images;android-37.0;google_apis;x86_64\n",
        );
        let original = fs::read_to_string(&path).unwrap();
        let error = tune_file(&path, Some(2048)).unwrap_err();
        assert!(error.to_string().contains("API 37+ phone AVD"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("ini.emutrim.bak").exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn tune_rejects_unresolved_image_before_mutation() {
        let path = temp_config("hw.ramSize=4096\ncustom.key=keep\n");
        let original = fs::read_to_string(&path).unwrap();
        assert!(tune_file(&path, None).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("ini.emutrim.bak").exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn managed_ram_repair_changes_only_ram_once_and_preserves_line_endings() {
        let path = temp_config(
            "target=android-37.0\r\nhw.ramSize=2048\r\ncustom.key=keep\r\nimage.sysdir.1=system-images;android-37.0;google_apis;x86_64\r\n",
        );
        assert!(repair_managed_ram_config(&path).unwrap());
        let expected = "target=android-37.0\r\nhw.ramSize=4096\r\ncustom.key=keep\r\nimage.sysdir.1=system-images;android-37.0;google_apis;x86_64\r\n";
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        assert!(!repair_managed_ram_config(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        let _ = fs::remove_file(path);
    }
}
