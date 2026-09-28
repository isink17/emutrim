use super::{safe_avd_name, ClearTarget, Layout};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PendingReset {
    schema_version: u32,
    avd_name: String,
    avd_path: String,
    ini_path: String,
    phase: String,
}

impl PendingReset {
    fn new(target: &ClearTarget) -> io::Result<Self> {
        Ok(Self {
            schema_version: 1,
            avd_name: target.name(),
            avd_path: canonical_string(target.avd_dir())?,
            ini_path: canonical_string(target.ini())?,
            phase: "pending".into(),
        })
    }
}

pub(crate) fn create_or_verify(layout: &Layout, target: &ClearTarget) -> io::Result<()> {
    let marker = PendingReset::new(target)?;
    let (directory, path) = marker_path(layout, &marker.avd_name, true)?;
    validate_marker(layout, &path, &marker)?;
    match read_marker(&path, &marker)? {
        Some(existing) if existing == marker => return Ok(()),
        Some(_) => return Err(invalid("pending reset marker identity mismatch")),
        None => {}
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let temp = directory.join(format!(
        ".{}.{}.{}.tmp",
        marker_id(&marker.avd_name),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec(&marker).map_err(invalid)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let write_result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "pending reset marker appeared during creation",
            ));
        }
        fs::rename(&temp, &path)?;
        sync_directory(&directory)?;
        match read_marker(&path, &marker)? {
            Some(actual) if actual == marker => Ok(()),
            _ => Err(invalid("pending reset marker read-back mismatch")),
        }
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

pub(crate) fn validate_existing(layout: &Layout, target: &ClearTarget) -> io::Result<bool> {
    let expected = PendingReset::new(target)?;
    let Some(directory) = marker_directory(layout, false)? else {
        return Ok(false);
    };
    let path = directory.join(format!("{}.json", marker_id(&expected.avd_name)));
    Ok(read_marker(&path, &expected)?.is_some())
}

pub(crate) fn validate_named(layout: &Layout, name: &str) -> io::Result<bool> {
    if !safe_avd_name(name) {
        return Err(invalid("invalid managed AVD identity for reset marker"));
    }
    let Some(directory) = marker_directory(layout, false)? else {
        return Ok(false);
    };
    let path = directory.join(format!("{}.json", marker_id(name)));
    let Some(marker) = read_unchecked_marker(&path)? else {
        return Ok(false);
    };
    validate_marker(layout, &path, &marker)?;
    if marker.avd_name != name {
        return Err(invalid("pending reset marker name mismatch"));
    }
    Ok(true)
}

pub(crate) fn remove_after_success(layout: &Layout, target: &ClearTarget) -> io::Result<()> {
    let expected = PendingReset::new(target)?;
    let (directory, path) = marker_path(layout, &expected.avd_name, false)?;
    match read_marker(&path, &expected)? {
        Some(actual) if actual == expected => {}
        Some(_) => return Err(invalid("pending reset marker identity mismatch")),
        None => {
            return Err(invalid(
                "pending reset marker disappeared before completion",
            ))
        }
    }
    fs::remove_file(&path)?;
    if let Err(error) = sync_directory(&directory) {
        let _ = create_or_verify(layout, target);
        return Err(error);
    }
    if path.exists() {
        return Err(invalid("pending reset marker remains after removal"));
    }
    Ok(())
}

pub(crate) fn refuse_if_pending(layout: &Layout, target: &ClearTarget) -> io::Result<()> {
    let Some(directory) = marker_directory(layout, false)? else {
        return Ok(());
    };
    let name = target.name();
    let path = directory.join(format!("{}.json", marker_id(&name)));
    let Some(marker) = read_unchecked_marker(&path)? else {
        return Ok(());
    };
    validate_marker(layout, &path, &marker)?;
    if marker.avd_name != name {
        return Err(invalid("pending reset marker identity mismatch"));
    }
    Err(pending_error(&name))
}

pub(crate) fn refuse_named(layout: &Layout, name: &str) -> io::Result<()> {
    if !safe_avd_name(name) {
        return Ok(());
    }
    let Some(directory) = marker_directory(layout, false)? else {
        return Ok(());
    };
    let path = directory.join(format!("{}.json", marker_id(name)));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if is_link(&metadata) || !metadata.is_file() {
        return Err(invalid("pending reset marker is not a regular file"));
    }
    let marker: PendingReset = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|_| invalid("malformed pending reset marker"))?;
    validate_marker(layout, &path, &marker)?;
    if marker.avd_name != name {
        return Err(invalid("pending reset marker name mismatch"));
    }
    Err(pending_error(name))
}

pub(crate) fn refuse_if_any_pending(layout: &Layout) -> io::Result<()> {
    if let Some(marker) = read_all(layout)?.into_iter().next() {
        return Err(pending_error(&marker.avd_name));
    }
    Ok(())
}

pub(crate) fn refuse_for_serial(layout: &Layout, serial: &str) -> io::Result<()> {
    let markers = read_all(layout)?;
    if markers.is_empty() {
        return Ok(());
    }
    let port = crate::avd::console_port(serial).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reset guard requires emulator serial",
        )
    })?;
    if crate::platform::console_owner_pid(port)?.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot prove host-owned console while reset is pending",
        ));
    }
    let name = crate::avd::console::avd_name(port).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot authenticate emulator identity while reset is pending: {error}"),
        )
    })?;
    refuse_for_name(&markers, &name)
}

pub(crate) fn refuse_for_name(markers: &[PendingReset], name: &str) -> io::Result<()> {
    if markers.iter().any(|marker| marker.avd_name == name) {
        return Err(pending_error(name));
    }
    Ok(())
}

fn read_all(layout: &Layout) -> io::Result<Vec<PendingReset>> {
    let directory = marker_directory(layout, false)?;
    let Some(directory) = directory else {
        return Ok(Vec::new());
    };
    let mut markers = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "tmp") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid(
                "pending reset directory contains a non-file or link",
            ));
        }
        let bytes = fs::read(&path)?;
        let marker: PendingReset = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("malformed pending reset marker"))?;
        validate_marker(layout, &path, &marker)?;
        markers.push(marker);
    }
    Ok(markers)
}

fn marker_path(layout: &Layout, name: &str, create: bool) -> io::Result<(PathBuf, PathBuf)> {
    if !safe_avd_name(name) {
        return Err(invalid("invalid managed AVD identity for reset marker"));
    }
    let directory = marker_directory(layout, create)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "pending reset directory is absent")
    })?;
    let path = directory.join(format!("{}.json", marker_id(name)));
    Ok((directory, path))
}

fn marker_directory(layout: &Layout, create: bool) -> io::Result<Option<PathBuf>> {
    if !create && !layout.managed.exists() {
        return Ok(None);
    }
    let root = layout.root.canonicalize()?;
    let managed = layout.managed.canonicalize()?;
    if managed == root
        || !managed.starts_with(&root)
        || is_link(&fs::symlink_metadata(&layout.root)?)
        || is_link(&fs::symlink_metadata(&layout.managed)?)
    {
        return Err(invalid("managed root containment is unsafe"));
    }
    let directory = layout.managed.join("reset-pending");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_dir() => {
            Err(invalid("pending reset path is not a regular directory"))
        }
        Ok(_) => {
            if directory.canonicalize()? != managed.join("reset-pending") {
                return Err(invalid("pending reset directory escapes managed root"));
            }
            Ok(Some(directory))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir(&directory)?;
            sync_directory(&layout.managed)?;
            Ok(Some(directory))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_marker(path: &Path, expected: &PendingReset) -> io::Result<Option<PendingReset>> {
    let Some(marker) = read_unchecked_marker(path)? else {
        return Ok(None);
    };
    if marker.schema_version != 1 || marker.phase != "pending" {
        return Err(invalid("unsupported pending reset marker version or phase"));
    }
    if marker != *expected {
        return Err(invalid("pending reset marker identity or path mismatch"));
    }
    Ok(Some(marker))
}

fn read_unchecked_marker(path: &Path) -> io::Result<Option<PendingReset>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if is_link(&metadata) || !metadata.is_file() {
        return Err(invalid("pending reset marker is not a regular file"));
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let marker: PendingReset =
        serde_json::from_slice(&bytes).map_err(|_| invalid("malformed pending reset marker"))?;
    Ok(Some(marker))
}

fn validate_marker(layout: &Layout, path: &Path, marker: &PendingReset) -> io::Result<()> {
    if marker.schema_version != 1 || marker.phase != "pending" || !safe_avd_name(&marker.avd_name) {
        return Err(invalid("unsupported or unsafe pending reset marker"));
    }
    let expected_path = marker_path(layout, &marker.avd_name, false)?.1;
    if path != expected_path {
        return Err(invalid(
            "pending reset marker filename does not match identity",
        ));
    }
    let avd = PathBuf::from(&marker.avd_path);
    let ini = PathBuf::from(&marker.ini_path);
    let root = layout.avd.canonicalize()?;
    if avd.parent() != Some(root.as_path())
        || ini.parent() != Some(root.as_path())
        || avd.file_name() != Some(std::ffi::OsStr::new(&format!("{}.avd", marker.avd_name)))
        || ini.file_name() != Some(std::ffi::OsStr::new(&format!("{}.ini", marker.avd_name)))
        || avd.canonicalize()? != avd
        || ini.canonicalize()? != ini
    {
        return Err(invalid(
            "pending reset marker path escapes managed AVD home",
        ));
    }
    Ok(())
}

fn canonical_string(path: &Path) -> io::Result<String> {
    Ok(path.canonicalize()?.to_string_lossy().into_owned())
}

fn marker_id(name: &str) -> String {
    name.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_link(metadata: &fs::Metadata) -> bool {
    super::is_link_or_reparse(metadata)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn pending_error(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "Reset for AVD {name:?} is incomplete. Ensure it is stopped, then retry:\n  emutrim reset {name} --yes"
        ),
    )
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unsafe pending reset marker: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct NoopClearOps;

    impl super::super::ClearOps for NoopClearOps {
        fn console_owner(&self, _: u16) -> io::Result<Option<u32>> {
            Ok(None)
        }
        fn console_avd_name(&self, _: u16) -> io::Result<String> {
            unreachable!()
        }
    }

    fn fixture() -> (Layout, ClearTarget) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "emutrim-reset-marker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let managed = root.join("managed");
        let avd = managed.join("avd");
        let sdk = managed.join("sdk");
        fs::create_dir_all(avd.join("Alpha.avd")).unwrap();
        fs::create_dir_all(sdk.join("system-images/image")).unwrap();
        let target_path = avd.join("Alpha.avd");
        fs::write(
            avd.join("Alpha.ini"),
            format!("path={}\n", target_path.display()),
        )
        .unwrap();
        fs::write(
            target_path.join("config.ini"),
            "image.sysdir.1=system-images/image\nhw.ramSize=4096\n",
        )
        .unwrap();
        let layout = Layout {
            root: root.clone(),
            managed: managed.clone(),
            sdk,
            avd,
            tmp: managed.join("tmp"),
            manifest: managed.join("manifest.json"),
        };
        fs::write(
            &layout.manifest,
            serde_json::to_vec(&json!({"schema":1,"avds":["Alpha"]})).unwrap(),
        )
        .unwrap();
        let target = super::super::resolve_managed_avd(&layout, "Alpha").unwrap();
        (layout, target)
    }

    #[test]
    fn marker_is_atomically_published_read_back_and_removable() {
        let (layout, target) = fixture();
        create_or_verify(&layout, &target).unwrap();
        let marker = PendingReset::new(&target).unwrap();
        let path = marker_path(&layout, "Alpha", false).unwrap().1;
        let stored: PendingReset = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored, marker);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert!(validate_existing(&layout, &target).unwrap());
        assert!(refuse_if_pending(&layout, &target).is_err());
        remove_after_success(&layout, &target).unwrap();
        assert!(!validate_existing(&layout, &target).unwrap());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn retry_reuses_only_exact_existing_pending_marker() {
        let (layout, target) = fixture();
        create_or_verify(&layout, &target).unwrap();
        create_or_verify(&layout, &target).unwrap();
        assert!(validate_existing(&layout, &target).unwrap());
        let other = ClearTarget {
            name: "Beta".into(),
            avd_dir: target.avd_dir().into(),
            ini: target.ini().into(),
        };
        assert!(create_or_verify(&layout, &other).is_err());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn malformed_unknown_version_and_mismatched_path_markers_fail_closed() {
        let (layout, target) = fixture();
        create_or_verify(&layout, &target).unwrap();
        let path = marker_path(&layout, "Alpha", false).unwrap().1;
        fs::write(&path, b"{").unwrap();
        assert!(validate_named(&layout, "Alpha").is_err());
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "avd_name": "Alpha",
                "avd_path": target.avd_dir().display().to_string(),
                "ini_path": target.ini().display().to_string(),
                "phase": "pending"
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(validate_named(&layout, "Alpha").is_err());
        let mut marker = PendingReset::new(&target).unwrap();
        marker.avd_path = layout.root.join("outside.avd").display().to_string();
        fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(validate_named(&layout, "Alpha").is_err());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn clear_refuses_valid_pending_target_without_removing_marker() {
        let (layout, target) = fixture();
        create_or_verify(&layout, &target).unwrap();
        assert!(super::super::plan_clear(&layout, Some("Alpha")).is_err());
        assert!(validate_existing(&layout, &target).unwrap());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn clear_execution_rechecks_marker_added_after_dry_plan() {
        let (layout, target) = fixture();
        let plan = super::super::plan_clear(&layout, Some("Alpha")).unwrap();
        create_or_verify(&layout, &target).unwrap();
        assert!(plan.execute_with(&NoopClearOps).is_err());
        assert!(target.avd_dir().exists());
        assert!(validate_existing(&layout, &target).unwrap());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[test]
    fn pending_marker_blocks_exact_mutable_target_but_not_other_avd() {
        let (layout, target) = fixture();
        create_or_verify(&layout, &target).unwrap();
        let markers = read_all(&layout).unwrap();
        assert!(refuse_for_name(&markers, "Alpha").is_err());
        assert!(refuse_for_name(&markers, "Beta").is_ok());
        assert!(refuse_named(&layout, "Alpha").is_err());
        fs::remove_dir_all(layout.root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn marker_directory_symlink_is_refused() {
        use std::os::unix::fs::symlink;
        let (layout, target) = fixture();
        let outside = layout.root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, layout.managed.join("reset-pending")).unwrap();
        assert!(create_or_verify(&layout, &target).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(layout.managed.join("reset-pending")).unwrap();
        fs::remove_dir_all(outside).unwrap();
        fs::remove_dir_all(layout.root).unwrap();
    }
}
