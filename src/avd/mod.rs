use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::PathBuf;
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
    validate_ram_for_image(info.is_16k, ram_mb)
}

fn validate_ram_for_image(is_16k: bool, ram_mb: u32) -> io::Result<()> {
    if is_16k && ram_mb < 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "16 KB system image requires at least 4096 MB; refusing incompatible --ram",
        ));
    }
    if !(1536..=8192).contains(&ram_mb) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "emulator RAM must be between 1536 and 8192 MB",
        ));
    }
    Ok(())
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
    let is_16k = is_16k(&config);
    let ram = requested_ram.unwrap_or(if is_16k { 4096 } else { 1536 });
    validate_ram_for_image(is_16k, ram)?;
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
    cold_boot: bool,
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
    command.args(["-avd", name, "-port", &port.to_string(), "-gpu", "host"]);
    if managed {
        let layout = crate::managed::Layout::resolve()?;
        command
            .arg("-datadir")
            .arg(layout.avd.join(format!("{name}.avd")));
        command
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_SDK_ROOT", &sdk)
            .env("ANDROID_AVD_HOME", &layout.avd)
            .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
            .env("TMPDIR", &layout.tmp);
    }
    command.arg("-memory").arg(ram.to_string());
    if cold_boot {
        command.arg("-no-snapshot");
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    if !info.is_16k {
        command.arg("-lowram");
    }
    command.spawn()
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
    fn ram_validation_enforces_image_and_emulator_limits() {
        let info = AvdInfo {
            api: "37".into(),
            abi: "x86_64".into(),
            tag: "google_apis".into(),
            ram_mb: 4096,
            image: PathBuf::new(),
            is_16k: true,
            gpu_mode: "host".into(),
            gpu_enabled: "yes".into(),
            backup_exists: false,
        };
        assert!(validate_ram(&info, 1536).is_err());
        assert!(validate_ram(&info, 4096).is_ok());
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
            "hw.ramSize=2048\ncustom.key=keep\nhw.gpu.mode=auto\nhw.audioInput=no\nhw.camera.back=none\nimage.sysdir.1=system-images;android-31;google_apis;x86_64\n",
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
            "hw.ramSize=2048\ncustom.key=keep\nhw.gpu.mode=auto\nhw.audioInput=no\nhw.camera.back=none\nimage.sysdir.1=system-images;android-31;google_apis;x86_64\n"
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
        for (image, expected) in [
            ("system-images;android-31;google_apis;x86_64", "1536"),
            ("system-images;android-37;google_apis_ps16k;x86_64", "4096"),
        ] {
            let path = temp_config(&format!("image.sysdir.1={image}\n"));
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
        let path =
            temp_config("image.sysdir.1=system-images;android-37;google_apis_ps16k;x86_64\n");
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
    fn tune_rejects_unresolved_image_before_mutation() {
        let path = temp_config("hw.ramSize=4096\ncustom.key=keep\n");
        let original = fs::read_to_string(&path).unwrap();
        assert!(tune_file(&path, None).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("ini.emutrim.bak").exists());
        let _ = fs::remove_file(path);
    }
}
