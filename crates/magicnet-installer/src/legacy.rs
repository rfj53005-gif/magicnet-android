//! Temporary migration for releases through v1.6.2.
//!
//! Ported from `installer/components/legacy_cleanup.go`. This is an
//! installation boundary, never a boot-time or recurring cleaner. Ownership is
//! proven by exact byte markers, an exclusive legacy lock, or the old worker's
//! exact public contract; a matching name alone never authorizes deletion.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::constants::LEGACY_FAREWELL_IDLE;
use super::storage::{clean_path, open_no_follow, physical, read_bounded};
use super::{Error, Result};

const FAREWELL_MARKER: &[u8] = b"id=\"farewell\"";
const BACKUP_MARKER: &[u8] = b"magicnet-install-backup-v1\n";

fn legacy_archive_name(name: &str) -> bool {
    name.len() == 68
        && name.ends_with(".zip")
        && name[..64]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn legacy_download_name(name: &str) -> bool {
    name.strip_prefix(".download-")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn legacy_backup_pid(name: &str) -> Option<i32> {
    let rest = name.strip_prefix("MagicNet.install-backup.")?;
    if rest.is_empty()
        || !rest.bytes().next().is_some_and(|b| matches!(b, b'1'..=b'9'))
        || !rest.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    rest.parse::<i32>().ok()
}

fn legacy_farewell_name(name: &str) -> bool {
    name.strip_prefix(".magicnet-farewell.").is_some_and(|rest| {
        rest.len() == 6 && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

fn legacy_token(bytes: &[u8]) -> bool {
    bytes.len() == 33
        && bytes[32] == b'\n'
        && bytes[..32]
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Go: legacy_cleanup.go:27-30 — `legacyExact`.
fn legacy_exact(name: &Path, expected: &[u8]) -> bool {
    read_bounded(name, expected.len() as u64)
        .map(|data| data == expected)
        .unwrap_or(false)
}

/// Go: legacy_cleanup.go:32-38 — `legacyDirectory`.
fn legacy_directory(name: &Path) -> bool {
    if physical(name, false).is_err() {
        return false;
    }
    fs::metadata(name).is_ok_and(|meta| meta.is_dir())
}

/// Go: legacy_cleanup.go:40-46 — `legacyRemoveEmpty`.
fn legacy_remove_empty(name: &Path) -> Result<()> {
    match fs::remove_dir(name) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOTEMPTY) | Some(libc::EEXIST) | Some(libc::ENOENT)
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(Error::from_io(error)),
    }
}

fn entry_name(entry: &fs::DirEntry) -> Option<String> {
    entry.file_name().to_str().map(str::to_string)
}

/// Go: legacy_cleanup.go:48-95 — `cleanupLegacyComponentCache`.
fn cleanup_legacy_component_cache(cache: &Path) -> Result<()> {
    if !legacy_directory(cache) {
        return Ok(());
    }
    let lock_path = cache.join("install.lock");
    // No regular lock file means the old updater never owned this cache.
    if super::storage::regular(&lock_path).is_err() {
        return Ok(());
    }
    let lock = open_no_follow(&lock_path).map_err(Error::from_io)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Error(format!(
            "legacy component cache is busy: {}",
            io::Error::last_os_error()
        )));
    }
    let entries = fs::read_dir(cache).map_err(Error::from_io)?;
    for entry in entries {
        let entry = entry.map_err(Error::from_io)?;
        let Some(name) = entry_name(&entry) else {
            continue;
        };
        if !legacy_archive_name(&name) && !legacy_download_name(&name) {
            continue;
        }
        let path = cache.join(&name);
        if super::storage::regular(&path).is_ok() {
            fs::remove_file(&path).map_err(Error::from_io)?;
        }
    }
    let entries: Vec<_> = fs::read_dir(cache)
        .map_err(Error::from_io)?
        .collect::<io::Result<Vec<_>>>()
        .map_err(Error::from_io)?;
    if entries.len() == 1 && entries[0].file_name() == OsStr::new("install.lock") {
        fs::remove_file(&lock_path).map_err(Error::from_io)?;
        legacy_remove_empty(cache)?;
    }
    drop(lock);
    Ok(())
}

/// Go: legacy_cleanup.go:97-110 — `cleanupLegacyLogs`.
fn cleanup_legacy_logs(directory: &Path) -> Result<()> {
    if !legacy_directory(directory) {
        return Ok(());
    }
    for filename in ["kam.log", "kam.log.1"] {
        let name = directory.join(filename);
        if super::storage::regular(&name).is_ok() {
            fs::remove_file(&name).map_err(Error::from_io)?;
        }
    }
    legacy_remove_empty(directory)
}

fn chmod_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(Error::from_io)?;
    if !meta.file_type().is_dir() {
        return Ok(());
    }
    let mode = meta.permissions().mode() | 0o700;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(Error::from_io)?;
    for entry in fs::read_dir(path).map_err(Error::from_io)? {
        let entry = entry.map_err(Error::from_io)?;
        chmod_tree(&entry.path())?;
    }
    Ok(())
}

/// Go: legacy_cleanup.go:112-158 — `cleanupLegacyBackups`.
fn cleanup_legacy_backups(parent: &Path) -> Result<()> {
    if !legacy_directory(parent) {
        return Ok(());
    }
    let entries = fs::read_dir(parent).map_err(Error::from_io)?;
    for entry in entries {
        let entry = entry.map_err(Error::from_io)?;
        let Some(name) = entry_name(&entry) else {
            continue;
        };
        let Some(pid) = legacy_backup_pid(&name) else {
            continue;
        };
        // A live or inaccessible owner is not stale.
        if unsafe { libc::kill(pid, 0) } == 0 {
            continue;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            continue;
        }
        let directory = parent.join(&name);
        if !legacy_directory(&directory)
            || !legacy_exact(&directory.join(".magicnet-install-backup-v1"), BACKUP_MARKER)
        {
            continue;
        }
        // Old backups preserve read-only directory modes. Walk physically and
        // change only owned directories, never symlink targets or user files.
        chmod_tree(&directory)?;
        if !legacy_exact(&directory.join(".magicnet-install-backup-v1"), BACKUP_MARKER) {
            return Err(Error(format!(
                "legacy backup ownership changed: {}",
                directory.display()
            )));
        }
        fs::remove_dir_all(&directory).map_err(Error::from_io)?;
    }
    Ok(())
}

/// Go: legacy_cleanup.go:160-182 — `legacyFarewellIdle`.
fn legacy_farewell_idle(directory: &Path) -> bool {
    let Ok(meta) = fs::metadata(directory) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    if age < LEGACY_FAREWELL_IDLE {
        return false;
    }
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    let mut needle = directory.as_os_str().as_bytes().to_vec();
    needle.push(b'/');
    for process in processes {
        let Ok(process) = process else {
            continue;
        };
        if !process
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        match fs::read(process.path().join("cmdline")) {
            Ok(command) => {
                if command.windows(needle.len()).any(|window| window == needle) {
                    return false;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    true
}

fn farewell_relative(directory: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(directory).ok()?;
    let text = relative.to_str()?;
    Some(if text.is_empty() {
        ".".to_string()
    } else {
        text.to_string()
    })
}

/// Go: legacy_cleanup.go:184-233 — `cleanupLegacyFarewell`.
fn cleanup_legacy_farewell(parent: &Path) -> Result<()> {
    let entries = fs::read_dir(parent).map_err(Error::from_io)?;
    for entry in entries {
        let entry = entry.map_err(Error::from_io)?;
        let Some(name) = entry_name(&entry) else {
            continue;
        };
        if !legacy_farewell_name(&name) {
            continue;
        }
        let directory = parent.join(&name);
        if !legacy_directory(&directory) || !legacy_farewell_idle(&directory) {
            continue;
        }
        // These old directories have no owner marker. Require their exact
        // public worker contract; a matching directory name alone is not proof.
        let worker = concat!(
            "#!/system/bin/sh\nMN_FAREWELL_RUN=",
            "$",
            "{0%/*}\n. \"$MN_FAREWELL_RUN/uninstall.sh\" || exit 1\nmagicnet_farewell_worker \"$MN_FAREWELL_RUN\"\n"
        );
        if !legacy_exact(&directory.join("worker.sh"), worker.as_bytes())
            || !legacy_exact(&directory.join("httpd.conf"), b"A:127.0.0.1\nD:*\n")
        {
            continue;
        }
        let Ok(token) = read_bounded(&directory.join("token"), 33) else {
            continue;
        };
        if !legacy_token(&token) {
            continue;
        }
        let token = String::from_utf8_lossy(&token).trim().to_string();
        let Ok(page) = read_bounded(
            &directory.join("www").join(&token).join("index.html"),
            256 * 1024,
        ) else {
            continue;
        };
        if !page
            .windows(FAREWELL_MARKER.len())
            .any(|window| window == FAREWELL_MARKER)
        {
            continue;
        }
        let mut allowed = std::collections::HashSet::new();
        for name in [
            ".",
            "busybox",
            "uninstall.sh",
            "launcher.sh",
            "token",
            "version",
            "httpd.conf",
            "worker.sh",
            "www",
            "www/",
        ] {
            allowed.insert(name.to_string());
        }
        allowed.insert(format!("www/{token}"));
        allowed.insert(format!("www/{token}/index.html"));
        if !farewell_walk(&directory, &allowed) {
            continue;
        }
        fs::remove_dir_all(&directory).map_err(Error::from_io)?;
    }
    Ok(())
}

fn farewell_walk(root: &Path, allowed: &std::collections::HashSet<String>) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
        let Some(relative) = farewell_relative(root, &path) else {
            return false;
        };
        if !allowed.contains(&relative) {
            return false;
        }
        if meta.is_dir() {
            let Ok(entries) = fs::read_dir(&path) else {
                return false;
            };
            for entry in entries {
                match entry {
                    Ok(entry) => stack.push(entry.path()),
                    Err(_) => return false,
                }
            }
        }
    }
    true
}

/// Go: legacy_cleanup.go:235-267 — `cleanupLegacyStorage`.
pub fn cleanup_legacy_storage(module_dir: &Path) -> Result<()> {
    let module_dir = clean_path(module_dir);
    let parent = module_dir.parent().unwrap_or(Path::new("/"));
    if !module_dir.is_absolute()
        || module_dir.file_name() != Some(OsStr::new("MagicNet"))
        || !matches!(
            parent.file_name().and_then(OsStr::to_str),
            Some("modules") | Some("modules_update")
        )
        || !legacy_directory(&module_dir)
    {
        return Err(Error(
            "legacy cleanup requires a physical MagicNet manager directory".into(),
        ));
    }
    let props = read_bounded(&module_dir.join("module.prop"), 64 * 1024)?;
    let text = String::from_utf8_lossy(&props);
    let mut identities = 0usize;
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("id=") {
            if rest != "MagicNet" {
                return Err(Error("legacy cleanup module identity mismatch".into()));
            }
            identities += 1;
        }
    }
    if identities != 1 {
        return Err(Error(
            "legacy cleanup requires one MagicNet module identity".into(),
        ));
    }
    let adb = parent.parent().unwrap_or(Path::new("/"));
    let mut first: Option<Error> = None;
    let results = [
        cleanup_legacy_component_cache(&adb.join("magicnet-components")),
        cleanup_legacy_logs(&adb.join("cache").join("MagicNet")),
        cleanup_legacy_backups(&adb.join("modules")),
        cleanup_legacy_backups(&adb.join("modules_update")),
        cleanup_legacy_farewell(adb),
    ];
    for result in results {
        if let Err(error) = result {
            if first.is_none() {
                first = Some(error);
            }
        }
    }
    match first {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
