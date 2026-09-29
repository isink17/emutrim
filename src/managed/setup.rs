use super::{is_link_or_reparse, read_dirs, validate_tree_no_links, Layout};
use std::cmp::Ordering;
use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const BASE_PACKAGES: [&str; 3] = ["cmdline-tools;latest", "platform-tools", "emulator"];
const MAC_AVD: &str = "EmuTrim_Mac_Acceptance_20260927";
const WINDOWS_AVD: &str = "EmuTrim_Managed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Host {
    WindowsX64,
    MacArm64,
}

impl Host {
    fn current() -> io::Result<Self> {
        host_for(cfg!(windows), cfg!(target_os = "macos"), env::consts::ARCH).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "managed setup supports Windows x86_64 and Apple Silicon macOS only",
            )
        })
    }

    fn abi(self) -> &'static str {
        match self {
            Self::WindowsX64 => "x86_64",
            Self::MacArm64 => "arm64-v8a",
        }
    }

    fn manifest_name(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows-x86_64",
            Self::MacArm64 => "macos-aarch64",
        }
    }

    fn fresh_avd(self) -> &'static str {
        match self {
            Self::WindowsX64 => WINDOWS_AVD,
            Self::MacArm64 => MAC_AVD,
        }
    }
}

fn host_for(windows: bool, macos: bool, arch: &str) -> Option<Host> {
    match (windows, macos, arch) {
        (true, false, "x86_64") => Some(Host::WindowsX64),
        (false, true, "aarch64") => Some(Host::MacArm64),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SystemImage {
    platform: String,
    version: Vec<u32>,
    tag: String,
    abi: String,
}

impl SystemImage {
    fn manifest_package(&self) -> String {
        format!("system-images;{};{};{}", self.platform, self.tag, self.abi)
    }

    fn cli_package(&self) -> String {
        format!("system-images/{}/{}/{}", self.platform, self.tag, self.abi)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AndroidSdkTool {
    AndroidCli(PathBuf),
    SdkManager(PathBuf),
}

impl AndroidSdkTool {
    fn label(&self) -> &'static str {
        match self {
            Self::AndroidCli(_) => "Android CLI",
            Self::SdkManager(_) => "sdkmanager",
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::AndroidCli(path) | Self::SdkManager(path) => path,
        }
    }

    fn list(&self, layout: &Layout) -> io::Result<Output> {
        let mut command = match self {
            Self::AndroidCli(path) => {
                let mut command = Command::new(path);
                command
                    .arg(format!("--sdk={}", layout.sdk.display()))
                    .args(["sdk", "list", "--all"])
                    .env_remove("JAVA_HOME");
                command
            }
            Self::SdkManager(path) if cfg!(windows) => {
                let args = vec![
                    format!("--sdk_root={}", layout.sdk.display()),
                    "--channel=0".to_owned(),
                    "--list".to_owned(),
                ];
                return run_batch(path, &args, layout, false);
            }
            Self::SdkManager(path) => {
                let mut command = Command::new(path);
                command.args([
                    format!("--sdk_root={}", layout.sdk.display()),
                    "--channel=0".to_owned(),
                    "--list".to_owned(),
                ]);
                command
            }
        };
        with_managed_environment(&mut command, layout).output()
    }

    fn install(&self, layout: &Layout, packages: &[String]) -> io::Result<bool> {
        if packages.is_empty() {
            return Ok(true);
        }
        let status = match self {
            Self::AndroidCli(path) => {
                let mut command = Command::new(path);
                command
                    .arg(format!("--sdk={}", layout.sdk.display()))
                    .arg("sdk")
                    .arg("install")
                    .args(packages.iter().map(|package| package.replace(';', "/")))
                    .env_remove("JAVA_HOME");
                with_managed_environment(&mut command, layout).status()?
            }
            Self::SdkManager(path) if cfg!(windows) => {
                let mut args = vec![format!("--sdk_root={}", layout.sdk.display())];
                args.extend(packages.iter().cloned());
                return Ok(run_batch(path, &args, layout, true)?.status.success());
            }
            Self::SdkManager(path) => {
                let mut command = Command::new(path);
                command
                    .arg(format!("--sdk_root={}", layout.sdk.display()))
                    .args(packages);
                with_managed_environment(&mut command, layout).status()?
            }
        };
        Ok(status.success())
    }
}

fn with_managed_environment<'a>(command: &'a mut Command, layout: &Layout) -> &'a mut Command {
    command
        .env("ANDROID_HOME", &layout.sdk)
        .env("ANDROID_SDK_ROOT", &layout.sdk)
        .env("ANDROID_AVD_HOME", &layout.avd)
        .env("ANDROID_USER_HOME", layout.tmp.join("android-user"))
        .env("ANDROID_EMULATOR_HOME", layout.tmp.join("emulator-home"))
        .env("TMPDIR", &layout.tmp)
        .env("TEMP", &layout.tmp)
        .env("TMP", &layout.tmp)
}

fn run_batch(path: &Path, args: &[String], layout: &Layout, inherit: bool) -> io::Result<Output> {
    let mut command = batch_process(path, args)?;
    with_managed_environment(&mut command, layout);
    if inherit {
        let status = command.status()?;
        Ok(Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    } else {
        Ok(command.output()?)
    }
}

fn batch_process(path: &Path, args: &[String]) -> io::Result<Command> {
    let command_line = batch_command_line(path, args)?;
    let mut command = Command::new("cmd.exe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command
            .arg("/D")
            .arg("/S")
            .arg("/C")
            .raw_arg(format!(" \"{command_line}\""));
    }
    #[cfg(not(windows))]
    command.args(["/D", "/C", &command_line]);
    Ok(command)
}

fn batch_command_line(path: &Path, args: &[String]) -> io::Result<String> {
    let path = path.to_string_lossy();
    if path.chars().any(is_cmd_metachar)
        || args
            .iter()
            .any(|arg| arg.chars().any(is_cmd_metachar) || arg.contains('"'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe character in Windows batch-tool argument",
        ));
    }
    let mut line = format!("\"{path}\"");
    for arg in args {
        line.push(' ');
        if arg.chars().any(char::is_whitespace) {
            line.push('"');
            line.push_str(arg);
            line.push('"');
        } else {
            line.push_str(arg);
        }
    }
    Ok(line)
}

fn is_cmd_metachar(ch: char) -> bool {
    matches!(
        ch,
        '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')' | '"'
    )
}

fn cli_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(profile) = env::var_os("USERPROFILE") {
        paths.push(PathBuf::from(profile).join("AppData/AndroidCLI/android.exe"));
    }
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path).map(|dir| {
            dir.join(if cfg!(windows) {
                "android.exe"
            } else {
                "android"
            })
        }));
    }
    paths.extend(sdk_roots().into_iter().flat_map(|root| {
        read_dirs(&root.join("cmdline-tools"))
            .unwrap_or_default()
            .into_iter()
            .map(|version| {
                version.join("bin").join(if cfg!(windows) {
                    "android.exe"
                } else {
                    "android"
                })
            })
            .collect::<Vec<_>>()
    }));
    unique_files(paths)
}

fn sdkmanager_candidates() -> Vec<PathBuf> {
    unique_files(
        sdk_roots()
            .into_iter()
            .flat_map(|root| {
                read_dirs(&root.join("cmdline-tools"))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|version| {
                        version.join("bin").join(if cfg!(windows) {
                            "sdkmanager.bat"
                        } else {
                            "sdkmanager"
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    )
}

fn sdk_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for key in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(path) = env::var_os(key).filter(|path| !path.is_empty()) {
            roots.push(PathBuf::from(path));
        }
    }
    if cfg!(windows) {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Android/Sdk"));
        }
    } else if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Library/Android/sdk"));
    }
    roots
}

fn unique_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .filter(|path| {
            let key = if cfg!(windows) {
                path.to_string_lossy().to_ascii_lowercase()
            } else {
                path.to_string_lossy().into_owned()
            };
            seen.insert(key)
        })
        .collect()
}

fn run_candidate(tool: &AndroidSdkTool, layout: &Layout) -> io::Result<Vec<SystemImage>> {
    let version = match tool {
        AndroidSdkTool::SdkManager(path) if cfg!(windows) => {
            run_batch(path, &["--version".to_owned()], layout, false)?
        }
        _ => {
            let mut command = Command::new(tool.path());
            command.arg("--version");
            if matches!(tool, AndroidSdkTool::AndroidCli(_)) {
                command.env_remove("JAVA_HOME");
            }
            command.output()?
        }
    };
    if !version.status.success() {
        return Err(io::Error::other(format!(
            "{} --version exited {}",
            tool.label(),
            version.status
        )));
    }
    let listing = tool.list(layout)?;
    if !listing.status.success() {
        return Err(io::Error::other(format!(
            "{} catalog listing exited {}; stderr: {}",
            tool.label(),
            listing.status,
            String::from_utf8_lossy(&listing.stderr).trim()
        )));
    }
    let host = Host::current()?;
    validate_catalog(true, &listing.stdout, host, tool.label())
}

fn validate_catalog(
    exit_success: bool,
    stdout: &[u8],
    host: Host,
    tool: &str,
) -> io::Result<Vec<SystemImage>> {
    if !exit_success {
        return Err(io::Error::other(format!(
            "{tool} catalog listing failed despite its output"
        )));
    }
    let images = parse_images(&String::from_utf8_lossy(stdout));
    if !images
        .iter()
        .any(|image| image.tag == "google_apis" && image.abi == host.abi())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{tool} catalog has no ordinary google_apis image for {}",
                host.abi()
            ),
        ));
    }
    Ok(images)
}

fn discover_tool(layout: &Layout) -> io::Result<(AndroidSdkTool, Vec<SystemImage>)> {
    let host = Host::current()?;
    let mut candidates = Vec::new();
    if host == Host::WindowsX64 {
        candidates.extend(cli_candidates().into_iter().map(AndroidSdkTool::AndroidCli));
    }
    candidates.extend(
        sdkmanager_candidates()
            .into_iter()
            .map(AndroidSdkTool::SdkManager),
    );
    let mut errors = Vec::new();
    for candidate in candidates {
        match run_candidate(&candidate, layout) {
            Ok(images) => return Ok((candidate, images)),
            Err(error) => errors.push(format!("{}: {error}", candidate.path().display())),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no healthy Android SDK management tool found; {}",
            errors.join("; ")
        ),
    ))
}

fn parse_images(text: &str) -> Vec<SystemImage> {
    let mut images = Vec::new();
    for line in text.lines() {
        for token in line.split_whitespace() {
            let normalized = token.trim_matches(|ch: char| {
                !ch.is_ascii_alphanumeric() && ch != '/' && ch != ';' && ch != '.' && ch != '-'
            });
            let parts: Vec<_> = normalized.split(['/', ';']).collect();
            if parts.len() != 4 || parts[0] != "system-images" || parts[2] != "google_apis" {
                continue;
            }
            let Some(version) = parse_platform(parts[1]) else {
                continue;
            };
            if !matches!(parts[3], "x86_64" | "arm64-v8a") {
                continue;
            }
            let image = SystemImage {
                platform: parts[1].to_owned(),
                version,
                tag: parts[2].to_owned(),
                abi: parts[3].to_owned(),
            };
            if !images.contains(&image) {
                images.push(image);
            }
        }
    }
    images
}

fn parse_platform(platform: &str) -> Option<Vec<u32>> {
    let version = platform.strip_prefix("android-")?;
    let parts: Option<Vec<u32>> = version
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect();
    parts.filter(|parts| !parts.is_empty())
}

fn select_image(images: &[SystemImage], host: Host) -> io::Result<SystemImage> {
    images
        .iter()
        .filter(|image| image.tag == "google_apis" && image.abi == host.abi())
        .max_by(|left, right| compare_versions(&left.version, &right.version))
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no stable ordinary google_apis {} system image listed",
                    host.abi()
                ),
            )
        })
}

fn compare_versions(left: &[u32], right: &[u32]) -> Ordering {
    left.iter().cmp(right.iter())
}

pub(super) fn run(layout: &Layout) -> io::Result<()> {
    let host = Host::current()?;
    prepare_managed_dirs(layout)?;
    let (mut manifest, image, name, fresh, mut selected_tool) = load_or_plan(layout, host)?;
    let package = image.manifest_package();
    let required: Vec<String> = BASE_PACKAGES
        .iter()
        .map(|package| (*package).to_owned())
        .chain(std::iter::once(package.clone()))
        .collect();
    let original_manifest = manifest.clone();
    let manifest_exists = path_present(&layout.manifest)?;
    manifest["system_image"] = package.clone().into();
    manifest["sdk_packages"] = serde_json::json!(required);
    manifest["arch"] = host.abi().into();
    manifest["host"] = host.manifest_name().into();
    manifest["avds"] = serde_json::json!([name]);
    if manifest_needs_write(manifest_exists, &original_manifest, &manifest) {
        write_manifest(layout, &manifest)?;
    }

    let mut missing = Vec::new();
    for package in &required {
        if !package_installed(layout, package)? {
            missing.push(package.clone());
        }
    }
    if !missing.is_empty() {
        let tool = match selected_tool.take() {
            Some(tool) => tool,
            None => discover_tool(layout)?.0,
        };
        if !tool.install(layout, &missing)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} failed installing {}; if prompted, accept required licenses interactively and rerun: {}",
                    tool.label(),
                    missing.join(", "),
                    install_hint(&tool, layout, &missing)
                ),
            ));
        }
        for package in &missing {
            if !package_installed(layout, package)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SDK package {package} remains absent after installation"),
                ));
            }
        }
    }

    let avd_path = layout.avd.join(format!("{name}.avd"));
    let avd_ini = layout.avd.join(format!("{name}.ini"));
    if validate_avd(layout, &name, &image)?.is_none() {
        let manager = avdmanager_path(&layout.sdk);
        if !manager.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("avdmanager missing: {}", manager.display()),
            ));
        }
        clear_partial_avd(layout, &avd_path, &avd_ini)?;
        let args = vec![
            "create".to_owned(),
            "avd".to_owned(),
            "--name".to_owned(),
            name.clone(),
            "--package".to_owned(),
            image.manifest_package(),
            "--path".to_owned(),
            avd_path.to_string_lossy().into_owned(),
            "--device".to_owned(),
            "pixel_2".to_owned(),
        ];
        let output = if cfg!(windows) {
            run_batch(&manager, &args, layout, false)?
        } else {
            let mut command = Command::new(manager);
            command.args(&args);
            with_managed_environment(&mut command, layout).output()?
        };
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "avdmanager failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        validate_avd(layout, &name, &image)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "AVD creation did not produce a valid managed config.ini and .ini",
            )
        })?;
    }
    println!("managed AVD ready: {}", avd_path.display());
    if fresh {
        println!("managed system image: {package}");
    }
    Ok(())
}

fn load_or_plan(
    layout: &Layout,
    host: Host,
) -> io::Result<(
    serde_json::Value,
    SystemImage,
    String,
    bool,
    Option<AndroidSdkTool>,
)> {
    if path_present(&layout.manifest)? {
        let manifest = super::read_manifest(layout)?;
        if manifest["host"].as_str() != Some(host.manifest_name())
            || manifest["arch"].as_str() != Some(host.abi())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed manifest belongs to a different host or architecture",
            ));
        }
        let package = manifest["system_image"]
            .as_str()
            .ok_or_else(|| super::invalid_manifest("managed system_image is missing"))?;
        let image = parse_images(package)
            .into_iter()
            .find(|image| image.manifest_package() == package)
            .filter(|image| image.abi == host.abi())
            .ok_or_else(|| {
                super::invalid_manifest("managed system_image is not an eligible host image")
            })?;
        let packages = manifest["sdk_packages"]
            .as_array()
            .and_then(|packages| {
                packages
                    .iter()
                    .map(serde_json::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| super::invalid_manifest("managed sdk_packages is malformed"))?;
        let mut expected: Vec<String> = BASE_PACKAGES
            .iter()
            .map(|package| (*package).to_owned())
            .collect();
        expected.push(image.manifest_package());
        if packages.len() != expected.len()
            || packages
                .iter()
                .any(|package| !expected.iter().any(|expected| expected == package))
        {
            return Err(super::invalid_manifest(
                "managed sdk_packages are unknown or incomplete",
            ));
        }
        let names = manifest["avds"]
            .as_array()
            .ok_or_else(|| super::invalid_manifest("manifest avds must be an array"))?;
        if names.len() > 1 {
            return Err(super::invalid_manifest(
                "managed setup expects at most one owned AVD",
            ));
        }
        let name = names
            .first()
            .and_then(serde_json::Value::as_str)
            .unwrap_or(host.fresh_avd())
            .to_owned();
        if !super::safe_avd_name(&name) {
            return Err(super::invalid_manifest("managed AVD identity is invalid"));
        }
        Ok((manifest, image, name, false, None))
    } else {
        let (tool, listing) = discover_tool(layout)?;
        let image = select_image(&listing, host)?;
        let name = host.fresh_avd().to_owned();
        let paths = [
            layout.avd.join(format!("{name}.avd")),
            layout.avd.join(format!("{name}.ini")),
        ];
        for path in &paths {
            if path_present(path)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "managed AVD {name} exists without ownership metadata; refusing overwrite"
                    ),
                ));
            }
        }
        let mut packages: Vec<String> = BASE_PACKAGES.iter().map(|p| (*p).to_owned()).collect();
        packages.push(image.manifest_package());
        let manifest = serde_json::json!({
            "schema": 1,
            "sdk_packages": packages,
            "system_image": image.manifest_package(),
            "arch": host.abi(),
            "host": host.manifest_name(),
            "avds": [name],
        });
        Ok((manifest, image, name, true, Some(tool)))
    }
}

fn prepare_managed_dirs(layout: &Layout) -> io::Result<()> {
    for path in [
        &layout.root,
        &layout.managed,
        &layout.sdk,
        &layout.avd,
        &layout.tmp,
    ] {
        reject_reparse_ancestors(path)?;
        fs::create_dir_all(path)?;
        reject_reparse_ancestors(path)?;
    }
    let root = layout.root.canonicalize()?;
    for path in [&layout.managed, &layout.sdk, &layout.avd, &layout.tmp] {
        let canonical = path.canonicalize()?;
        if canonical == root || !canonical.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed path escapes configured root",
            ));
        }
    }
    ensure_emulator_home(layout)?;
    Ok(())
}

pub(super) fn ensure_emulator_home(layout: &Layout) -> io::Result<()> {
    let home = layout.tmp.join("emulator-home");
    reject_reparse_ancestors(&home)?;
    fs::create_dir_all(&home)?;
    reject_reparse_ancestors(&home)?;
    let root = layout.root.canonicalize()?;
    let managed = layout.managed.canonicalize()?;
    let tmp = layout.tmp.canonicalize()?;
    let canonical_home = home.canonicalize()?;
    if managed.parent() != Some(root.as_path())
        || tmp.parent() != Some(managed.as_path())
        || canonical_home.parent() != Some(tmp.as_path())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed Emulator home escapes managed root",
        ));
    }
    Ok(())
}

fn reject_reparse_ancestors(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            _ => current.push(component.as_os_str()),
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "managed path contains symlink or reparse point: {}",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn package_installed(layout: &Layout, package: &str) -> io::Result<bool> {
    let path = match package {
        "cmdline-tools;latest" => avdmanager_path(&layout.sdk),
        "platform-tools" => {
            layout
                .sdk
                .join("platform-tools")
                .join(if cfg!(windows) { "adb.exe" } else { "adb" })
        }
        "emulator" => crate::avd::emulator_path(&layout.sdk),
        _ => layout
            .sdk
            .join(package.replace(';', "/"))
            .join("package.xml"),
    };
    reject_reparse_ancestors(&path)?;
    Ok(path.is_file())
}

fn avdmanager_path(sdk: &Path) -> PathBuf {
    sdk.join("cmdline-tools/latest/bin")
        .join(tool_filename("avdmanager", cfg!(windows)))
}

fn tool_filename(stem: &str, windows: bool) -> String {
    if windows {
        format!("{stem}.bat")
    } else {
        stem.to_owned()
    }
}

fn validate_avd(layout: &Layout, name: &str, image: &SystemImage) -> io::Result<Option<()>> {
    let avd = layout.avd.join(format!("{name}.avd"));
    let ini = layout.avd.join(format!("{name}.ini"));
    let avd_exists = path_present(&avd)?;
    let ini_exists = path_present(&ini)?;
    if !avd_exists && !ini_exists {
        return Ok(None);
    }
    for (path, is_dir) in [(&avd, true), (&ini, false)] {
        match fs::symlink_metadata(path) {
            Ok(meta) if is_link_or_reparse(&meta) => {
                return Err(super::invalid_manifest(
                    "managed AVD path is a reparse point",
                ))
            }
            Ok(meta) if is_dir && !meta.is_dir() => {
                return Err(super::invalid_manifest(
                    "managed AVD payload is not a directory",
                ))
            }
            Ok(meta) if !is_dir && !meta.is_file() => {
                return Err(super::invalid_manifest("managed AVD ini is not a file"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            _ => {}
        }
    }
    if avd_exists {
        validate_tree_no_links(&avd)?;
    }
    if ini_exists {
        let text = fs::read_to_string(&ini)?;
        let recorded = text
            .lines()
            .find_map(|line| line.strip_prefix("path="))
            .ok_or_else(|| super::invalid_manifest("managed AVD .ini has no path"))?;
        if !path_identity_eq(Path::new(recorded), &avd) {
            return Err(super::invalid_manifest(
                "managed AVD .ini points outside its owned directory",
            ));
        }
    }
    if avd_exists && ini_exists {
        let config = avd.join("config.ini");
        if !config.is_file()
            || !fs::read_to_string(config)?.lines().any(|line| {
                line.strip_prefix("image.sysdir.1=").is_some_and(|value| {
                    let value = value.replace('\\', "/");
                    let value = value.trim_end_matches('/');
                    value == image.cli_package() || value == image.manifest_package()
                })
            })
        {
            return Err(super::invalid_manifest(
                "managed AVD config does not match manifest image",
            ));
        }
        crate::avd::config_path_mode(name, true)?;
        return Ok(Some(()));
    }
    Ok(None)
}

fn path_identity_eq(left: &Path, right: &Path) -> bool {
    if !left.is_absolute() || left.components().any(|part| part == Component::ParentDir) {
        return false;
    }
    let left = left.canonicalize().unwrap_or_else(|_| left.to_owned());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_owned());
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn path_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn clear_partial_avd(layout: &Layout, avd: &Path, ini: &Path) -> io::Result<()> {
    if path_present(avd)? {
        reject_reparse_ancestors(avd)?;
        validate_tree_no_links(avd)?;
        fs::remove_dir_all(avd)?;
    }
    if path_present(ini)? {
        reject_reparse_ancestors(ini)?;
        let metadata = fs::symlink_metadata(ini)?;
        if is_link_or_reparse(&metadata) || !metadata.is_file() {
            return Err(super::invalid_manifest("partial managed AVD ini is unsafe"));
        }
        let text = fs::read_to_string(ini)?;
        let recorded = text.lines().find_map(|line| line.strip_prefix("path="));
        if recorded.is_none_or(|recorded| !path_identity_eq(Path::new(recorded), avd)) {
            return Err(super::invalid_manifest(
                "partial managed AVD ini identity is unsafe",
            ));
        }
        fs::remove_file(ini)?;
    }
    let _ = layout;
    Ok(())
}

fn write_manifest(layout: &Layout, manifest: &serde_json::Value) -> io::Result<()> {
    let temp = super::prepare_manifest(&layout.manifest, manifest)?;
    super::replace_file(&temp, &layout.manifest)
}

fn manifest_needs_write(
    exists: bool,
    original: &serde_json::Value,
    updated: &serde_json::Value,
) -> bool {
    !exists || original != updated
}

fn install_hint(tool: &AndroidSdkTool, layout: &Layout, packages: &[String]) -> String {
    match tool {
        AndroidSdkTool::AndroidCli(path) => format!(
            "\"{}\" --sdk=\"{}\" sdk install {}",
            path.display(),
            layout.sdk.display(),
            packages
                .iter()
                .map(|p| p.replace(';', "/"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        AndroidSdkTool::SdkManager(path) => format!(
            "\"{}\" --sdk_root=\"{}\" --licenses",
            path.display(),
            layout.sdk.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = "\
system-images/android-36/google_apis/x86_64 5.0.0 description\n\
system-images;android-36.1;google_apis;x86_64 5.0.0 description\n\
system-images/android-37.0/google_apis/x86_64 6.0.0 description\n\
system-images/android-37.1/google_apis_ps16k/x86_64 1.0.0 description\n\
system-images/android-37.2/google_apis_ps16k/x86_64 1.0.0 description\n\
system-images/android-37.2-beta3/google_apis_ps16k/x86_64 1.0.0 description\n\
system-images/android-35-ext15/google_apis/x86_64 1.0.0 description\n\
system-images/android-36/google_apis_playstore/x86_64 1.0.0 description\n\
system-images/android-37.0/google_apis/arm64-v8a 6.0.0 description\n\
system-images/android-37.1/google_apis_ps16k/arm64-v8a 1.0.0 description\n";

    #[test]
    fn prepare_managed_dirs_creates_emulator_home() {
        let root = temp_root("emulator home");
        let layout = fixture_layout(root.clone());
        prepare_managed_dirs(&layout).unwrap();
        assert!(layout.tmp.join("emulator-home").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_slash_and_semicolon_catalog_forms_and_selects_numeric_latest() {
        let images = parse_images(CATALOG);
        let win = select_image(&images, Host::WindowsX64).unwrap();
        let mac = select_image(&images, Host::MacArm64).unwrap();
        assert_eq!(
            win.manifest_package(),
            "system-images;android-37.0;google_apis;x86_64"
        );
        assert_eq!(
            mac.manifest_package(),
            "system-images;android-37.0;google_apis;arm64-v8a"
        );
        assert_eq!(
            parse_images("system-images;android-36.1;google_apis;x86_64")[0].version,
            [36, 1]
        );
    }

    #[test]
    fn rejects_specialized_preview_extension_and_wrong_abi_images() {
        let images = parse_images(CATALOG);
        assert_eq!(images.len(), 4);
        assert!(select_image(&[], Host::WindowsX64).is_err());
        assert!(parse_platform("android-37.2-beta3").is_none());
        assert!(parse_platform("android-35-ext15").is_none());
        assert!(parse_platform("android-CANARY").is_none());
        assert_eq!(compare_versions(&[37, 1], &[37, 0]), Ordering::Greater);
        assert_eq!(compare_versions(&[37, 0], &[36, 1]), Ordering::Greater);
    }

    #[test]
    fn host_policy_rejects_unsupported_architectures() {
        assert_eq!(host_for(true, false, "x86_64"), Some(Host::WindowsX64));
        assert_eq!(host_for(false, true, "aarch64"), Some(Host::MacArm64));
        assert_eq!(host_for(true, false, "aarch64"), None);
        assert_eq!(host_for(false, true, "x86_64"), None);
        assert_eq!(host_for(false, false, "x86_64"), None);
    }

    #[test]
    fn windows_batch_tool_names_are_centralized() {
        assert_eq!(tool_filename("sdkmanager", true), "sdkmanager.bat");
        assert_eq!(tool_filename("avdmanager", true), "avdmanager.bat");
        assert_eq!(tool_filename("sdkmanager", false), "sdkmanager");
        assert_eq!(tool_filename("avdmanager", false), "avdmanager");
    }

    #[test]
    fn nonzero_listing_is_unhealthy_even_when_stdout_has_valid_catalog() {
        assert!(
            validate_catalog(false, CATALOG.as_bytes(), Host::WindowsX64, "Android CLI").is_err()
        );
        assert!(
            validate_catalog(true, b"no package rows", Host::WindowsX64, "Android CLI").is_err()
        );
    }

    #[test]
    fn fresh_manifest_is_written_even_when_plan_already_matches() {
        let manifest = serde_json::json!({"schema": 1});
        assert!(manifest_needs_write(false, &manifest, &manifest));
        assert!(!manifest_needs_write(true, &manifest, &manifest));
        assert!(manifest_needs_write(
            true,
            &manifest,
            &serde_json::json!({"schema": 1, "avds": []})
        ));
    }

    #[test]
    fn existing_manifest_keeps_its_image_and_avd_name() {
        let root = temp_root("existing managed");
        let layout = fixture_layout(root.clone());
        fs::create_dir_all(&layout.managed).unwrap();
        let manifest = serde_json::json!({
            "schema": 1,
            "sdk_packages": ["cmdline-tools;latest", "platform-tools", "emulator", "system-images;android-36.1;google_apis;x86_64"],
            "system_image": "system-images;android-36.1;google_apis;x86_64",
            "arch": "x86_64",
            "host": "windows-x86_64",
            "avds": ["ExistingManaged"],
        });
        fs::write(&layout.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let (manifest, image, name, fresh, tool) = load_or_plan(&layout, Host::WindowsX64).unwrap();
        assert_eq!(manifest["avds"], serde_json::json!(["ExistingManaged"]));
        assert_eq!(
            image.manifest_package(),
            "system-images;android-36.1;google_apis;x86_64"
        );
        assert_eq!(name, "ExistingManaged");
        assert!(!fresh);
        assert!(tool.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_paths_with_spaces_keep_native_boundaries() {
        let root = temp_root("Test User EmuTrim");
        let layout = fixture_layout(root.clone());
        assert!(layout.sdk.to_string_lossy().contains("Test User EmuTrim"));
        assert_eq!(
            crate::avd::emulator_path(&layout.sdk),
            layout.sdk.join(if cfg!(windows) {
                "emulator/emulator.exe"
            } else {
                "emulator/emulator"
            })
        );
        assert_eq!(
            layout.avd.join("EmuTrim_Managed.avd"),
            root.join("managed/avd/EmuTrim_Managed.avd")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_owned_avd_is_recoverable_but_mismatched_ini_is_rejected() {
        let root = temp_root("partial managed AVD");
        let layout = fixture_layout(root.clone());
        fs::create_dir_all(&layout.avd).unwrap();
        let image = parse_images("system-images;android-37.0;google_apis;x86_64")
            .pop()
            .unwrap();
        let avd = layout.avd.join("EmuTrim_Managed.avd");
        fs::create_dir_all(&avd).unwrap();
        assert!(validate_avd(&layout, "EmuTrim_Managed", &image)
            .unwrap()
            .is_none());
        let ini = layout.avd.join("EmuTrim_Managed.ini");
        fs::write(&ini, "path=C:\\outside\\owned.avd\n").unwrap();
        assert!(validate_avd(&layout, "EmuTrim_Managed", &image).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn fixture_layout(root: PathBuf) -> Layout {
        let managed = root.join("managed");
        Layout {
            root,
            sdk: managed.join("sdk"),
            avd: managed.join("avd"),
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
            managed,
        }
    }

    #[test]
    fn windows_batch_arguments_keep_spaces_quoted_and_reject_shell_metacharacters() {
        let line = batch_command_line(
            Path::new(r"C:\SDK Tools\sdkmanager.bat"),
            &[
                "--sdk_root=C:\\Managed SDK".into(),
                "system-images;android-37.0;google_apis;x86_64".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            line,
            r#""C:\SDK Tools\sdkmanager.bat" "--sdk_root=C:\Managed SDK" system-images;android-37.0;google_apis;x86_64"#
        );
        assert!(batch_command_line(Path::new(r"C:\bad&path\sdkmanager.bat"), &[]).is_err());
    }

    #[test]
    #[cfg(windows)]
    fn windows_batch_invocation_preserves_argument_with_spaces() {
        let root = temp_root("batch tool with spaces");
        fs::create_dir_all(&root).unwrap();
        let script = root.join("fake sdkmanager.bat");
        fs::write(&script, "@echo off\r\necho [%~1]\r\n").unwrap();
        let output = batch_process(&script, &["value with spaces".to_owned()])
            .unwrap()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "status={}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("[value with spaces]"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_package_normalizes_manifest_identity_to_slash_path() {
        let image = parse_images("system-images;android-37.0;google_apis;x86_64")
            .pop()
            .unwrap();
        assert_eq!(
            image.manifest_package(),
            "system-images;android-37.0;google_apis;x86_64"
        );
        assert_eq!(
            image.cli_package(),
            "system-images/android-37.0/google_apis/x86_64"
        );
    }
}
