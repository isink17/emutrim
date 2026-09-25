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
    for key in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(value) = env::var_os(key) {
            let path = PathBuf::from(value);
            if path.is_dir() {
                return Ok(path);
            }
        }
    }
    let local = env::var_os("LOCALAPPDATA").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "ANDROID_SDK_ROOT/ANDROID_HOME/LOCALAPPDATA unavailable",
        )
    })?;
    let path = PathBuf::from(local).join("Android").join("Sdk");
    if path.is_dir() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Android SDK not found; set ANDROID_SDK_ROOT",
        ))
    }
}
pub fn avd_base() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("ANDROID_AVD_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("USERPROFILE")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "USERPROFILE unavailable"))?;
    Ok(PathBuf::from(home).join(".android").join("avd"))
}
pub fn config_path(name: &str) -> io::Result<PathBuf> {
    let path = avd_base()?.join(format!("{name}.avd")).join("config.ini");
    if path.is_file() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("AVD {name:?} not found"),
        ))
    }
}
pub fn list() -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(avd_base()?)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".avd"))
            {
                if entry.path().join("config.ini").is_file() {
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
pub fn inspect(name: &str) -> io::Result<AvdInfo> {
    let path = config_path(name)?;
    let config = parse(&fs::read_to_string(&path)?);
    let image_relative = config.get("image.sysdir.1").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "AVD config has no system image path",
        )
    })?;
    let image = sdk_dir()?.join(image_relative);
    let ram_mb = config
        .get("hw.ramSize")
        .and_then(|value| value.parse().ok())
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

pub fn validate_ram(info: &AvdInfo, ram_mb: u32) -> io::Result<()> {
    if info.is_16k && ram_mb < 4096 {
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
pub fn tune(name: &str, ram: u32) -> io::Result<()> {
    tune_file(&config_path(name)?, ram)
}
fn tune_file(path: &PathBuf, ram: u32) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let config = parse(&text);
    if is_16k(&config) && ram < 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "16 KB system image requires at least 4096 MB; refusing incompatible --ram",
        ));
    }
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
pub fn start(name: &str, ram: u32, port: u16) -> io::Result<Child> {
    let info = inspect(name)?;
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
    let emulator = sdk_dir()?.join("emulator").join("emulator.exe");
    if !emulator.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "emulator.exe not found in Android SDK",
        ));
    }
    let mut command = Command::new(emulator);
    command.args(["-avd", name, "-port", &port.to_string(), "-gpu", "host"]);
    command.arg("-memory").arg(ram.to_string());
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
            "hw.ramSize=2048\ncustom.key=keep\nhw.gpu.mode=auto\n",
        )
        .unwrap();
        tune_file(&path, 1536).unwrap();
        let edited = fs::read_to_string(&path).unwrap();
        assert!(edited.contains("hw.ramSize=1536\n"));
        assert!(edited.contains("hw.gpu.mode=host\n"));
        assert!(edited.contains("custom.key=keep\n"));
        assert_eq!(
            fs::read_to_string(path.with_extension("ini.emutrim.bak")).unwrap(),
            "hw.ramSize=2048\ncustom.key=keep\nhw.gpu.mode=auto\n"
        );
        let _ = fs::remove_file(path.with_extension("ini.emutrim.bak"));
        let _ = fs::remove_file(path);
    }
}
