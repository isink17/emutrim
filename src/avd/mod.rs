use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

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
pub fn start(name: &str, ram: u32) -> io::Result<()> {
    let config = parse(&fs::read_to_string(config_path(name)?)?);
    let sixteen = is_16k(&config);
    if sixteen && ram < 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "16 KB system image requires at least 4096 MB; refusing incompatible launch",
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
    command.args(["-avd", name, "-gpu", "host"]);
    if !sixteen {
        command.args(["-lowram", "-memory", &ram.to_string()]);
    }
    command.spawn()?;
    Ok(())
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
