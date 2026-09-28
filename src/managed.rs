use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
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

#[derive(Clone, Debug)]
pub(crate) struct ClearTarget {
    name: String,
    avd_dir: PathBuf,
    ini: PathBuf,
}

impl ClearTarget {
    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }
    pub(crate) fn avd_dir(&self) -> &Path {
        &self.avd_dir
    }
    pub(crate) fn ini(&self) -> &Path {
        &self.ini
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClearPlan {
    layout: Layout,
    manifest: serde_json::Value,
    targets: Vec<ClearTarget>,
}

const FIRST_CONSOLE_PORT: u16 = 5554;
const LAST_CONSOLE_PORT: u16 = 5682;

pub(crate) trait ClearOps {
    fn console_owner(&self, port: u16) -> io::Result<Option<u32>>;
    fn console_avd_name(&self, port: u16) -> io::Result<String>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
    fn replace_manifest(&self, from: &Path, to: &Path) -> io::Result<()> {
        replace_file(from, to)
    }
}

pub(crate) struct SystemClearOps;

impl ClearOps for SystemClearOps {
    fn console_owner(&self, port: u16) -> io::Result<Option<u32>> {
        crate::platform::console_owner_pid(port)
    }
    fn console_avd_name(&self, port: u16) -> io::Result<String> {
        crate::avd::console::avd_name(port)
    }
}

pub(crate) fn plan_clear(layout: &Layout, requested: Option<&str>) -> io::Result<ClearPlan> {
    validate_manifest_file(layout)?;
    let data = fs::read(&layout.manifest)?;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&data).map_err(invalid_manifest)?;
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| invalid_manifest("manifest must be an object"))?;
    if object.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(invalid_manifest("unsupported managed manifest schema"));
    }
    let names = object
        .get("avds")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_manifest("manifest avds must be an array"))?;
    if names.is_empty() {
        if let Some(name) = requested {
            if !safe_avd_name(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid managed AVD name",
                ));
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("managed AVD {name:?} not found"),
            ));
        }
        return Ok(ClearPlan {
            layout: layout.clone(),
            manifest,
            targets: Vec::new(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    let avd_root = canonical_avd_root(layout)?;
    let mut targets = Vec::with_capacity(names.len());
    for value in names {
        let name = value
            .as_str()
            .filter(|name| safe_avd_name(name))
            .ok_or_else(|| invalid_manifest("manifest contains invalid AVD identity"))?;
        let identity_key = if cfg!(any(windows, target_os = "macos")) {
            name.to_ascii_lowercase()
        } else {
            name.to_owned()
        };
        if !seen.insert(identity_key) {
            return Err(invalid_manifest("duplicate AVD identity in manifest"));
        }
        let avd_dir = avd_root.join(format!("{name}.avd"));
        let ini = avd_root.join(format!("{name}.ini"));
        validate_owned_path(&avd_dir, &avd_root, true)?;
        validate_owned_path(&ini, &avd_root, false)?;
        if avd_dir.exists() || ini.exists() {
            let ini_text = fs::read_to_string(&ini).map_err(|e| {
                invalid_manifest(format!(
                    "managed AVD {name:?} has no readable identity file: {e}"
                ))
            })?;
            let recorded = ini_text
                .lines()
                .find_map(|line| line.strip_prefix("path="))
                .ok_or_else(|| {
                    invalid_manifest(format!("managed AVD {name:?} .ini has no path"))
                })?;
            let recorded_path = Path::new(recorded);
            if !recorded_path.is_absolute()
                || recorded_path
                    .components()
                    .any(|c| c == Component::ParentDir)
                || if avd_dir.exists() {
                    recorded_path.canonicalize()? != avd_dir.canonicalize()?
                } else {
                    recorded_path
                        .parent()
                        .and_then(|parent| parent.canonicalize().ok())
                        .as_deref()
                        != Some(avd_root.as_path())
                        || recorded_path.file_name() != avd_dir.file_name()
                }
            {
                return Err(invalid_manifest(format!(
                    "managed AVD {name:?} .ini path is not its owned directory"
                )));
            }
        }
        targets.push(ClearTarget {
            name: name.into(),
            avd_dir,
            ini,
        });
    }
    let selected = if let Some(name) = requested {
        if !safe_avd_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid managed AVD name",
            ));
        }
        let Some(target) = targets.iter().find(|target| target.name == name).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("managed AVD {name:?} not found"),
            ));
        };
        vec![target]
    } else {
        targets
    };
    Ok(ClearPlan {
        layout: layout.clone(),
        manifest,
        targets: selected,
    })
}

fn validate_manifest_file(layout: &Layout) -> io::Result<()> {
    let managed = layout.managed.canonicalize()?;
    let metadata = fs::symlink_metadata(&layout.manifest)?;
    let expected = managed.join(layout.manifest.file_name().unwrap_or_default());
    if is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || layout
            .manifest
            .parent()
            .and_then(|path| path.canonicalize().ok())
            .as_deref()
            != Some(managed.as_path())
        || layout.manifest.canonicalize()? != expected
    {
        return Err(invalid_manifest(
            "manifest file is outside managed metadata area or is a link",
        ));
    }
    Ok(())
}

fn invalid_manifest(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unsafe managed manifest: {message}"),
    )
}

fn safe_avd_name(name: &str) -> bool {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c));
    if !valid {
        return false;
    }
    #[cfg(windows)]
    {
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|digit| {
                    digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9')
                })
            })
        {
            return false;
        }
    }
    true
}

fn canonical_avd_root(layout: &Layout) -> io::Result<PathBuf> {
    let root = layout.root.canonicalize()?;
    let managed = layout.managed.canonicalize()?;
    let sdk = layout.sdk.canonicalize()?;
    let avd = layout.avd.canonicalize()?;
    if managed == root
        || !managed.starts_with(&root)
        || sdk == managed
        || !sdk.starts_with(&managed)
        || avd == managed
        || avd == sdk
        || !avd.starts_with(&managed)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed AVD root relationship is unsafe",
        ));
    }
    for path in [&layout.root, &layout.managed, &layout.avd] {
        if is_link_or_reparse(&fs::symlink_metadata(path)?) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed AVD root contains link",
            ));
        }
    }
    Ok(avd)
}

fn validate_owned_path(path: &Path, avd_root: &Path, directory: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_manifest("owned path has no parent"))?;
    if parent != avd_root || path == avd_root {
        return Err(invalid_manifest("owned path escapes AVD home"));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(meta) if is_link_or_reparse(&meta) => {
            return Err(invalid_manifest(
                "owned AVD path is a symlink or reparse point",
            ))
        }
        Ok(meta) if directory && !meta.is_dir() => {
            return Err(invalid_manifest("owned AVD payload is not a directory"))
        }
        Ok(meta) if !directory && !meta.is_file() => {
            return Err(invalid_manifest("owned AVD definition is not a file"))
        }
        _ => {}
    }
    if directory {
        validate_tree_no_links(path)?;
    }
    if path.canonicalize()? != path {
        return Err(invalid_manifest("owned path has ambiguous canonical form"));
    }
    Ok(())
}

fn validate_tree_no_links(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path)?.dev()
    };
    validate_tree_no_links_on_device(path, {
        #[cfg(unix)]
        {
            Some(device)
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

fn validate_tree_no_links_on_device(path: &Path, _device: Option<u64>) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if is_link_or_reparse(&metadata) {
            return Err(invalid_manifest(format!(
                "owned AVD tree contains link: {}",
                child.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if Some(metadata.dev()) != _device {
                return Err(invalid_manifest(format!(
                    "owned AVD tree crosses filesystem boundary: {}",
                    child.display()
                )));
            }
        }
        if metadata.is_dir() {
            validate_tree_no_links_on_device(&child, _device)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || has_reparse_attribute(metadata.file_attributes())
}

#[cfg(windows)]
fn has_reparse_attribute(attributes: u32) -> bool {
    attributes & 0x400 != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

impl ClearPlan {
    pub(crate) fn targets(&self) -> &[ClearTarget] {
        &self.targets
    }
    pub(crate) fn only(mut self, name: &str) -> Self {
        self.targets.retain(|target| target.name == name);
        self
    }

    pub(crate) fn check_running(&self, ops: &impl ClearOps) -> io::Result<()> {
        let names: std::collections::HashSet<_> = self
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        for port in (FIRST_CONSOLE_PORT..=LAST_CONSOLE_PORT).step_by(2) {
            if ops.console_owner(port)?.is_none() {
                continue;
            }
            let name = ops.console_avd_name(port).map_err(|error| io::Error::new(error.kind(), format!("cannot authenticate host-owned emulator console on {port}; refusing clear: {error}")))?;
            if names.contains(name.as_str()) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("Managed AVD {name:?} is running as emulator-{port}. Stop it first with: emutrim stop {name}")));
            }
        }
        Ok(())
    }

    pub(crate) fn execute_with(mut self, ops: &impl ClearOps) -> io::Result<()> {
        validate_manifest_file(&self.layout)?;
        if self.targets.is_empty() {
            return Ok(());
        }
        for target in &self.targets {
            validate_owned_path(&target.avd_dir, &canonical_avd_root(&self.layout)?, true)?;
            validate_owned_path(&target.ini, &canonical_avd_root(&self.layout)?, false)?;
        }
        let removed: std::collections::HashSet<_> = self
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        let avds = self
            .manifest
            .get_mut("avds")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| invalid_manifest("manifest avds changed after validation"))?;
        avds.retain(|entry| !entry.as_str().is_some_and(|name| removed.contains(name)));
        let prepared_manifest = prepare_manifest(&self.layout.manifest, &self.manifest)?;
        for target in &self.targets {
            if target.avd_dir.exists() {
                if let Err(error) = ops.remove_dir_all(&target.avd_dir) {
                    let _ = fs::remove_file(&prepared_manifest);
                    return Err(error);
                }
            }
            if target.ini.exists() {
                if let Err(error) = ops.remove_file(&target.ini) {
                    let _ = fs::remove_file(&prepared_manifest);
                    return Err(error);
                }
            }
        }
        let result = ops.replace_manifest(&prepared_manifest, &self.layout.manifest);
        if result.is_err() {
            let _ = fs::remove_file(&prepared_manifest);
        }
        result
    }
}

fn prepare_manifest(path: &Path, value: &serde_json::Value) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_manifest("manifest has no parent"))?;
    let bytes = serde_json::to_vec_pretty(value).map_err(invalid_manifest)?;
    let mut temp = None;
    for suffix in 0..100 {
        let candidate = parent.join(format!(".manifest-{}-{suffix}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                temp = Some(candidate);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let temp = temp.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate manifest temp file",
        )
    })?;
    Ok(temp)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
    let processes: String = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, path) = line.split_once('|')?;
            Path::new(path)
                .starts_with(layout.sdk.join("emulator"))
                .then(|| format!("{pid} {path}\n"))
        })
        .collect();
    Ok(emulator_pids(&processes, &layout.sdk.join("emulator")))
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

    fn clear_fixture(names: &[&str]) -> Layout {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = env::temp_dir().join(format!(
            "emutrim-clear-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let managed = root.join("managed");
        let layout = Layout {
            root,
            managed: managed.clone(),
            sdk: managed.join("sdk"),
            avd: managed.join("avd"),
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
        };
        let _ = fs::remove_dir_all(&layout.root);
        fs::create_dir_all(layout.sdk.join("system-images/image")).unwrap();
        fs::create_dir_all(layout.sdk.join("platform-tools")).unwrap();
        fs::create_dir_all(layout.sdk.join("cmdline-tools")).unwrap();
        fs::create_dir_all(layout.avd.join("..")).unwrap();
        for name in names {
            let avd = layout.avd.join(format!("{name}.avd"));
            fs::create_dir_all(&avd).unwrap();
            fs::write(avd.join("userdata.img"), b"mutable data").unwrap();
            fs::write(
                layout.avd.join(format!("{name}.ini")),
                format!("path={}\n", avd.display()),
            )
            .unwrap();
        }
        let manifest = serde_json::json!({
            "schema": 1,
            "sdk_packages": ["cmdline-tools;latest", "platform-tools", "emulator", "system-images;android-35;google_apis;arm64-v8a"],
            "system_image": "system-images;android-35;google_apis;arm64-v8a",
            "arch": "arm64-v8a",
            "host": "macos-aarch64",
            "avds": names,
            "other_metadata": {"keep": true}
        });
        fs::write(&layout.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        layout
    }

    #[derive(Default)]
    struct NoConsoles;
    impl ClearOps for NoConsoles {
        fn console_owner(&self, _: u16) -> io::Result<Option<u32>> {
            Ok(None)
        }
        fn console_avd_name(&self, _: u16) -> io::Result<String> {
            unreachable!()
        }
    }

    struct OneConsole {
        name: Option<&'static str>,
        auth_fails: bool,
    }
    impl ClearOps for OneConsole {
        fn console_owner(&self, port: u16) -> io::Result<Option<u32>> {
            Ok((port == 5554).then_some(1234))
        }
        fn console_avd_name(&self, _: u16) -> io::Result<String> {
            if self.auth_fails {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "auth failed",
                ))
            } else {
                Ok(self.name.unwrap_or("Other").into())
            }
        }
    }

    struct FailingMutation {
        fail_dir: Option<String>,
        fail_manifest: bool,
    }
    impl ClearOps for FailingMutation {
        fn console_owner(&self, _: u16) -> io::Result<Option<u32>> {
            Ok(None)
        }
        fn console_avd_name(&self, _: u16) -> io::Result<String> {
            unreachable!()
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            if self.fail_dir.as_deref() == path.file_name().and_then(|name| name.to_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected delete failure",
                ));
            }
            fs::remove_dir_all(path)
        }
        fn replace_manifest(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail_manifest {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected manifest replace failure",
                ))
            } else {
                replace_file(from, to)
            }
        }
    }

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

    #[test]
    fn clear_plan_is_dry_until_execute_and_preserves_sdk_metadata() {
        let layout = clear_fixture(&["Alpha", "Beta"]);
        fs::write(layout.sdk.join("system-images/image/sentinel"), b"image").unwrap();
        fs::write(layout.sdk.join("platform-tools/sentinel"), b"tools").unwrap();
        let plan = plan_clear(&layout, Some("Alpha")).unwrap();
        assert_eq!(plan.targets().len(), 1);
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        assert!(plan.check_running(&NoConsoles).is_ok());
        plan.execute_with(&NoConsoles).unwrap();
        assert!(!layout.avd.join("Alpha.avd").exists());
        assert!(!layout.avd.join("Alpha.ini").exists());
        assert!(layout.avd.join("Beta.avd").exists());
        assert_eq!(
            fs::read(layout.sdk.join("system-images/image/sentinel")).unwrap(),
            b"image"
        );
        assert_eq!(
            fs::read(layout.sdk.join("platform-tools/sentinel")).unwrap(),
            b"tools"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        assert_eq!(manifest["avds"], serde_json::json!(["Beta"]));
        assert_eq!(manifest["other_metadata"]["keep"], true);
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn clear_all_rejects_late_unsafe_entry_before_any_deletion() {
        let layout = clear_fixture(&["Alpha"]);
        let outside = layout.root.with_file_name("outside-clear-sentinel.txt");
        fs::write(&outside, b"untouched").unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        manifest["avds"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("../outside"));
        fs::write(&layout.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(plan_clear(&layout, None).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        fs::remove_file(outside).unwrap();
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn clear_rejects_manifest_symlink_and_avd_tree_symlink() {
        let layout = clear_fixture(&["Alpha"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = layout.root.with_file_name("outside-clear-tree");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("sentinel"), b"untouched").unwrap();
            symlink(&outside, layout.avd.join("Alpha.avd/external")).unwrap();
            assert!(plan_clear(&layout, Some("Alpha")).is_err());
            assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
            fs::remove_file(layout.avd.join("Alpha.avd/external")).unwrap();
            fs::remove_dir_all(outside).unwrap();

            let original = layout.manifest.with_extension("original");
            fs::rename(&layout.manifest, &original).unwrap();
            let outside_manifest = layout.root.with_file_name("outside-clear-manifest.json");
            fs::write(&outside_manifest, b"external manifest").unwrap();
            symlink(&outside_manifest, &layout.manifest).unwrap();
            assert!(plan_clear(&layout, Some("Alpha")).is_err());
            assert_eq!(fs::read(&outside_manifest).unwrap(), b"external manifest");
            fs::remove_file(&layout.manifest).unwrap();
            fs::rename(original, &layout.manifest).unwrap();
            fs::remove_file(outside_manifest).unwrap();
        }
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn missing_owned_files_are_idempotent_and_unknown_target_refuses() {
        let layout = clear_fixture(&["Alpha"]);
        fs::remove_dir_all(layout.avd.join("Alpha.avd")).unwrap();
        fs::remove_file(layout.avd.join("Alpha.ini")).unwrap();
        assert!(plan_clear(&layout, Some("Alpha"))
            .unwrap()
            .execute_with(&NoConsoles)
            .is_ok());
        assert_eq!(
            plan_clear(&layout, Some("Missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn running_exact_target_and_auth_uncertainty_refuse_before_mutation() {
        let layout = clear_fixture(&["Alpha"]);
        let plan = plan_clear(&layout, Some("Alpha")).unwrap();
        assert!(plan
            .check_running(&OneConsole {
                name: Some("Alpha"),
                auth_fails: false
            })
            .is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        let plan = plan_clear(&layout, Some("Alpha")).unwrap();
        assert!(plan
            .check_running(&OneConsole {
                name: None,
                auth_fails: true
            })
            .is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn unsupported_schema_and_malformed_json_refuse() {
        let layout = clear_fixture(&["Alpha"]);
        fs::write(&layout.manifest, b"{").unwrap();
        assert!(plan_clear(&layout, None).is_err());
        fs::write(&layout.manifest, br#"{"schema":99,"avds":["Alpha"]}"#).unwrap();
        assert!(plan_clear(&layout, None).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn clear_all_success_preserves_managed_sdk_and_external_same_name() {
        let layout = clear_fixture(&["Alpha", "Beta"]);
        let external = layout.root.with_file_name("external-avd");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("Alpha.ini"), b"external definition").unwrap();
        fs::write(external.join("userdata.img"), b"external data").unwrap();
        fs::write(layout.sdk.join("system-images/image/sentinel"), b"image").unwrap();
        plan_clear(&layout, None)
            .unwrap()
            .execute_with(&NoConsoles)
            .unwrap();
        assert!(!layout.avd.join("Alpha.avd").exists());
        assert!(!layout.avd.join("Beta.avd").exists());
        assert_eq!(
            fs::read(external.join("Alpha.ini")).unwrap(),
            b"external definition"
        );
        assert_eq!(
            fs::read(external.join("userdata.img")).unwrap(),
            b"external data"
        );
        assert_eq!(
            fs::read(layout.sdk.join("system-images/image/sentinel")).unwrap(),
            b"image"
        );
        assert!(plan_clear(&layout, None).unwrap().targets().is_empty());
        fs::remove_dir_all(external).unwrap();
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn empty_managed_avd_state_is_successful_noop() {
        let layout = clear_fixture(&[]);
        fs::remove_dir_all(&layout.sdk).unwrap();
        let before = fs::read(&layout.manifest).unwrap();
        let plan = plan_clear(&layout, None).unwrap();
        assert!(plan.targets().is_empty());
        plan.execute_with(&NoConsoles).unwrap();
        assert_eq!(fs::read(&layout.manifest).unwrap(), before);
        assert_eq!(
            plan_clear(&layout, Some("Missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn outside_ini_path_duplicate_identity_and_path_injection_refuse() {
        for bad in [
            "../outside",
            "foo/../../bar",
            ".",
            "",
            "/",
            "C:\\outside",
            "trailing.",
        ] {
            assert!(!safe_avd_name(bad));
        }
        let layout = clear_fixture(&["Alpha"]);
        fs::write(layout.avd.join("Alpha.ini"), "path=/tmp/external-avd\n").unwrap();
        assert!(plan_clear(&layout, Some("Alpha")).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        manifest["avds"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("Alpha"));
        fs::write(&layout.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(plan_clear(&layout, None).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn case_ambiguous_manifest_names_refuse() {
        let layout = clear_fixture(&["Alpha"]);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        manifest["avds"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("alpha"));
        fs::write(&layout.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(plan_clear(&layout, None).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn execute_rechecks_paths_before_first_deletion() {
        let layout = clear_fixture(&["Alpha"]);
        let plan = plan_clear(&layout, None).unwrap();
        let original = layout.manifest.with_extension("original");
        fs::rename(&layout.manifest, &original).unwrap();
        fs::create_dir(&layout.manifest).unwrap();
        assert!(plan.execute_with(&NoConsoles).is_err());
        assert!(layout.avd.join("Alpha.avd/userdata.img").exists());
        fs::remove_dir(&layout.manifest).unwrap();
        fs::rename(original, &layout.manifest).unwrap();
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn owned_path_guard_rejects_avd_and_managed_parents() {
        let layout = clear_fixture(&["Alpha"]);
        for forbidden in [&layout.root, &layout.managed, &layout.sdk, &layout.avd] {
            assert!(validate_owned_path(forbidden, &layout.avd, true).is_err());
        }
        let avd_root = layout.avd.canonicalize().unwrap();
        assert!(validate_owned_path(&avd_root.join("Alpha.avd"), &avd_root, true).is_ok());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn deletion_failure_keeps_manifest_authoritative_and_retryable() {
        let layout = clear_fixture(&["Alpha", "Beta"]);
        let before = fs::read(&layout.manifest).unwrap();
        let plan = plan_clear(&layout, None).unwrap();
        let ops = FailingMutation {
            fail_dir: Some("Beta.avd".into()),
            fail_manifest: false,
        };
        assert!(plan.execute_with(&ops).is_err());
        assert!(!layout.avd.join("Alpha.avd").exists());
        assert!(layout.avd.join("Beta.avd/userdata.img").exists());
        assert_eq!(fs::read(&layout.manifest).unwrap(), before);
        plan_clear(&layout, None)
            .unwrap()
            .execute_with(&NoConsoles)
            .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        assert_eq!(manifest["avds"], serde_json::json!([]));
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn manifest_replace_failure_leaves_old_record_for_safe_retry() {
        let layout = clear_fixture(&["Alpha"]);
        let before = fs::read(&layout.manifest).unwrap();
        let plan = plan_clear(&layout, None).unwrap();
        let ops = FailingMutation {
            fail_dir: None,
            fail_manifest: true,
        };
        assert!(plan.execute_with(&ops).is_err());
        assert!(!layout.avd.join("Alpha.avd").exists());
        assert!(!layout.avd.join("Alpha.ini").exists());
        assert_eq!(fs::read(&layout.manifest).unwrap(), before);
        plan_clear(&layout, None)
            .unwrap()
            .execute_with(&NoConsoles)
            .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&layout.manifest).unwrap()).unwrap();
        assert_eq!(manifest["avds"], serde_json::json!([]));
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_classification_checks_attribute_flag() {
        assert!(has_reparse_attribute(0x400));
        assert!(has_reparse_attribute(0x400 | 0x10));
        assert!(!has_reparse_attribute(0x10));
    }
}
