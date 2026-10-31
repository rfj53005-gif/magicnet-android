//! Filesystem primitives shared by the installer: path validation, bounded
//! reads, the module lock, the owned workspace, and the promotion journal.
//!
//! Ported from `installer/components/main.go`, `storage.go` and
//! `legacy_cleanup.go`. The byte-for-byte layout of the workspace and the
//! promotion journal is preserved so a Go and a Rust installer can recover each
//! other's interrupted transaction during an upgrade.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::constants::{
    DOWNLOAD_TEMP_PREFIX, INSTALLED_MANIFEST_PATH, LOCK_FORK_GRACE_ATTEMPTS, LOCK_FORK_GRACE_INTERVAL,
    MAX_PROMOTION_CHANGES, PROMOTION_FILE, PROMOTION_LIMIT, WORKSPACE_NAME, WORKSPACE_OWNER,
};
use super::{Error, Result};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(super) fn is_hex_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// `filepath.Clean` for slash-separated names. Returns `".` for the empty
/// result, matching Go.
fn go_path_clean(name: &str) -> String {
    let absolute = name.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            value => parts.push(value),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Go: main.go:80-84 — `safeName`.
pub(super) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && go_path_clean(name) == name
        && !name.starts_with('/')
        && name != "."
        && name != ".."
        && !name.starts_with("../")
        && !name
            .bytes()
            .any(|b| matches!(b, b'\\' | 0 | b'\r' | b'\n' | b'\t'))
}

/// Go: main.go:85-98 — `managedName`.
pub(super) fn managed_name(name: &str) -> bool {
    if !safe_name(name) {
        return false;
    }
    if let Some(rest) = name.strip_prefix("bin/") {
        if rest.matches('/').count() == 0 && !rest.is_empty() {
            return name != "bin/magicnet-components";
        }
    }
    ["webroot/", ".config/sing-box/zashboard/", ".config/sing-box/rules/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Go: `path.Dir`.
pub(super) fn dir_name(name: &str) -> String {
    match name.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        Some(_) => "/".to_string(),
        None => ".".to_string(),
    }
}

/// Go: `filepath.Clean` for host paths (lexical only, never resolves
/// symlinks).
pub(super) fn clean_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Go: `filepath.Clean` for host paths, then make it absolute.
pub(super) fn lexical_absolute(path: &Path) -> io::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(clean_path(&joined))
}

/// Go: main.go:136-158 — validate every existing path component. No
/// extraction, copy, or removal follows an installation-owned symlink.
pub(super) fn physical(name: &Path, allow_missing: bool) -> Result<()> {
    let absolute = lexical_absolute(name).map_err(Error::from_io)?;
    let mut current = PathBuf::from("/");
    for part in absolute.components().skip(1) {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(Error(format!("refusing symlink: {}", current.display())));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => {}
            Err(error) => return Err(Error::from_io(error)),
        }
    }
    Ok(())
}

/// Go: main.go:159-171 — `regular`.
pub(super) fn regular(name: &Path) -> Result<fs::Metadata> {
    physical(name, false)?;
    let meta = fs::symlink_metadata(name).map_err(Error::from_io)?;
    if !meta.is_file() {
        return Err(Error(format!("not a regular file: {}", name.display())));
    }
    Ok(meta)
}

pub(super) fn open_no_follow(name: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(name)
}

/// Go: main.go:172-188 — `validFile`.
pub(super) fn valid_file(name: &Path, size: i64, digest: &str) -> bool {
    if size < 0 || regular(name).is_err() {
        return false;
    }
    let Ok(mut file) = open_no_follow(name) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.is_file() || meta.len() != size as u64 {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_total: u64 = 0;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                read_total += n as u64;
                if read_total > size as u64 {
                    return false;
                }
                hasher.update(&buffer[..n]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    read_total == size as u64 && hex_digest(&hasher) == digest
}

pub(super) fn hex_digest(hasher: &Sha256) -> String {
    let digest = hasher.clone().finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Go: main.go:189-213 — `copyVerified`. Creates the destination with O_EXCL
/// 0600, hashes while copying, rejects a size/hash mismatch, and applies the
/// manifest mode only after verification.
pub(super) fn copy_verified<R: Read>(reader: R, target: &Path, file: &super::data::PayloadFile) -> Result<()> {
    if let Some(parent) = target.parent() {
        create_dir_all_mode(parent, 0o755)?;
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(target)
        .map_err(Error::from_io)?;
    let mut hasher = Sha256::new();
    let mut limited = reader.take((file.size as u64).saturating_add(1));
    let mut buffer = [0u8; 64 * 1024];
    let mut total: i64 = 0;
    loop {
        match limited.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                total += n as i64;
                hasher.update(&buffer[..n]);
                out.write_all(&buffer[..n]).map_err(Error::from_io)?;
                if total > file.size {
                    return Err(Error(format!("checksum mismatch: {}", file.path)));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::from_io(error)),
        }
    }
    out.sync_all().map_err(Error::from_io)?;
    drop(out);
    if total != file.size || hex_digest(&hasher) != file.sha256 {
        return Err(Error(format!("checksum mismatch: {}", file.path)));
    }
    fs::set_permissions(target, fs::Permissions::from_mode(file.mode))
        .map_err(Error::from_io)?;
    Ok(())
}

/// Go: storage.go:41-55 — bounded read of a regular file.
pub(super) fn read_bounded(name: &Path, limit: u64) -> Result<Vec<u8>> {
    regular(name)?;
    let mut file = open_no_follow(name).map_err(Error::from_io)?;
    let mut raw = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(Error::from_io)?;
    if raw.len() as u64 > limit {
        return Err(Error(format!(
            "oversized installer state: {}",
            name.display()
        )));
    }
    Ok(raw)
}

pub(super) fn create_dir_all_mode(path: &Path, mode: u32) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    fs::create_dir_all(path).map_err(Error::from_io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(Error::from_io)
}

/// Go: storage.go:23-39 — lock the module directory inode itself, so two
/// processes can never lock different inodes and closing the descriptor (even
/// on process death) releases it.
///
/// EAGAIN grace retry (Rust-only, deliberate deviation from Go): a flock is
/// owned by the open file description, which child processes briefly inherit
/// during the fork→exec window before close-on-exec closes the copy. std's
/// process spawn forks the whole descriptor table, so a concurrent installer
/// launching curl can keep the parent's already-closed lock alive for a few
/// milliseconds and surface as spurious EAGAIN. Bounded retry absorbs that
/// transient copy while preserving the reject-concurrent-install contract:
/// a real competing installer holds the lock for the whole run and still
/// produces the same error.
pub(super) fn lock_module(root: &Path) -> Result<File> {
    physical(root, true)?;
    create_dir_all_mode(root, 0o700)?;
    let lock = File::open(root).map_err(Error::from_io)?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&lock);
    let mut last: Option<io::Error> = None;
    for _ in 0..LOCK_FORK_GRACE_ATTEMPTS {
        if unsafe { libc::flock(raw, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EAGAIN) {
            return Err(Error(format!(
                "another component installation is running: {}",
                error
            )));
        }
        last = Some(error);
        std::thread::sleep(LOCK_FORK_GRACE_INTERVAL);
    }
    Err(Error(format!(
        "another component installation is running: {}",
        last.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN))
    )))
}

/// Go: storage.go:59-90 — `prepareWorkspace`.
pub(super) fn prepare_workspace(root: &Path) -> Result<PathBuf> {
    let stage = root.join(WORKSPACE_NAME);
    physical(&stage, true)?;
    match fs::read_dir(&stage) {
        Ok(entries) => {
            let names: Vec<_> = entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()
                .map_err(Error::from_io)?;
            if !names.is_empty() {
                if read_bounded(&stage.join(WORKSPACE_OWNER), 0).is_err() {
                    return Err(Error(format!(
                        "unrecognized component workspace left untouched: {}",
                        stage.display()
                    )));
                }
                if let Err(error) = recover_promotion(&stage, root) {
                    return Err(Error(format!(
                        "component recovery retained at {}: {error}",
                        stage.display()
                    )));
                }
            }
            cleanup_workspace(&stage)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::from_io(error)),
    }
    fs::create_dir(&stage).map_err(Error::from_io)?;
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).map_err(Error::from_io)?;
    let owner = stage.join(WORKSPACE_OWNER);
    let write = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&owner)
        .and_then(|file| file.sync_all());
    if let Err(error) = write {
        let _ = fs::remove_dir_all(&stage);
        return Err(Error::from_io(error));
    }
    Ok(stage)
}

/// `os.RemoveAll` semantics for one workspace entry: a regular file, symlink
/// or directory is removed without following symlinks.
pub(super) fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).map_err(Error::from_io),
        Ok(_) => fs::remove_file(path).map_err(Error::from_io),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::from_io(error)),
    }
}

/// Go: storage.go:94-110 — remove the ownership marker last.
pub(super) fn cleanup_workspace(stage: &Path) -> Result<()> {
    let entries = fs::read_dir(stage).map_err(Error::from_io)?;
    for entry in entries {
        let entry = entry.map_err(Error::from_io)?;
        if entry.file_name() != OsStr::new(WORKSPACE_OWNER) {
            remove_any(&entry.path())?;
        }
    }
    match fs::remove_file(stage.join(WORKSPACE_OWNER)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::from_io(error)),
    }
    fs::remove_dir(stage).map_err(Error::from_io)
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PromotionChange {
    pub(super) path: String,
    pub(super) existed: bool,
    pub(super) install: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PromotionJournal {
    pub(super) schema: i64,
    pub(super) changes: Vec<PromotionChange>,
}

impl PromotionJournal {
    /// Go: storage.go:123-142 — `promotionJournal.validate`.
    fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.changes.is_empty()
            || self.changes.len() > MAX_PROMOTION_CHANGES
        {
            return Err(Error("invalid component promotion journal".into()));
        }
        let mut names = HashSet::new();
        for change in &self.changes {
            if (!managed_name(&change.path) && change.path != INSTALLED_MANIFEST_PATH)
                || !names.insert(change.path.clone())
            {
                return Err(Error("unsafe component promotion journal".into()));
            }
        }
        for name in &names {
            let mut parent = dir_name(name);
            while parent != "." {
                if names.contains(&parent) {
                    return Err(Error("overlapping component promotion journal".into()));
                }
                parent = dir_name(&parent);
            }
        }
        Ok(())
    }

    fn to_bytes(&self) -> Vec<u8> {
        let changes: Vec<Value> = self
            .changes
            .iter()
            .map(|c| json!({"path": c.path, "existed": c.existed, "install": c.install}))
            .collect();
        serde_json::to_vec(&json!({"schema": self.schema, "changes": changes}))
            .unwrap_or_else(|_| Vec::new())
    }

    fn parse(raw: &[u8]) -> Result<Self> {
        let value: Value =
            serde_json::from_slice(raw).map_err(|_| Error("invalid component promotion journal".into()))?;
        let schema = value["schema"]
            .as_i64()
            .ok_or_else(|| Error("invalid component promotion journal".into()))?;
        let changes = value["changes"]
            .as_array()
            .ok_or_else(|| Error("invalid component promotion journal".into()))?
            .iter()
            .map(|change| {
                let path = change["path"]
                    .as_str()
                    .ok_or_else(|| Error("invalid component promotion journal".into()))?;
                let existed = change["existed"]
                    .as_bool()
                    .ok_or_else(|| Error("invalid component promotion journal".into()))?;
                let install = change["install"]
                    .as_bool()
                    .ok_or_else(|| Error("invalid component promotion journal".into()))?;
                Ok(PromotionChange {
                    path: path.to_string(),
                    existed,
                    install,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let journal = Self { schema, changes };
        journal.validate()?;
        Ok(journal)
    }
}

/// Go: storage.go:147-199 — `preparePromotion`: finish validation and write
/// the complete plan before any old file moves.
pub(super) fn prepare_promotion(
    stage: &Path,
    root: &Path,
    files: &[super::data::PayloadFile],
    obsolete: &[String],
) -> Result<PromotionJournal> {
    let mut changes = Vec::with_capacity(files.len() + obsolete.len());
    for file in files {
        changes.push(PromotionChange {
            path: file.path.clone(),
            existed: false,
            install: true,
        });
    }
    for name in obsolete {
        changes.push(PromotionChange {
            path: name.clone(),
            existed: false,
            install: false,
        });
    }
    let mut journal = PromotionJournal { schema: 1, changes };
    journal.validate()?;
    for change in &mut journal.changes {
        let dst = root.join(&change.path);
        physical(&dst, true)?;
        match fs::symlink_metadata(&dst) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(Error(format!(
                        "unsafe component destination: {}",
                        dst.display()
                    )));
                }
                change.existed = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::from_io(error)),
        }
        if change.install {
            regular(&stage.join(&change.path))?;
        }
        let rollback = stage.join(".rollback").join(&change.path);
        if let Some(parent) = dst.parent() {
            create_dir_all_mode(parent, 0o755)?;
        }
        if let Some(parent) = rollback.parent() {
            create_dir_all_mode(parent, 0o755)?;
        }
    }
    let raw = journal.to_bytes();
    let tmp = stage.join(format!("{PROMOTION_FILE}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(Error::from_io)?;
    file.write_all(&raw).map_err(Error::from_io)?;
    file.sync_all().map_err(Error::from_io)?;
    drop(file);
    fs::rename(&tmp, stage.join(PROMOTION_FILE)).map_err(Error::from_io)?;
    Ok(journal)
}

/// Go: storage.go:201-223 — `promote`.
pub(super) fn promote(
    stage: &Path,
    root: &Path,
    files: &[super::data::PayloadFile],
    obsolete: &[String],
) -> Result<()> {
    let journal = prepare_promotion(stage, root, files, obsolete)?;
    for change in &journal.changes {
        let dst = root.join(&change.path);
        if change.existed {
            fs::rename(&dst, stage.join(".rollback").join(&change.path))
                .map_err(Error::from_io)?;
        }
        if change.install {
            fs::rename(stage.join(&change.path), &dst).map_err(Error::from_io)?;
        }
    }
    fs::rename(stage.join(PROMOTION_FILE), stage.join(".committed")).map_err(Error::from_io)
}

/// Go: storage.go:228-284 — `recoverPromotion`. Recovery is repeatable if it
/// is interrupted itself: a backup is consumed only by the atomic rename back.
pub(super) fn recover_promotion(stage: &Path, root: &Path) -> Result<()> {
    let journal_path = stage.join(PROMOTION_FILE);
    match fs::symlink_metadata(&journal_path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::from_io(error)),
    }
    let raw = read_bounded(&journal_path, PROMOTION_LIMIT)?;
    let journal = PromotionJournal::parse(&raw)?;
    for change in journal.changes.iter().rev() {
        let dst = root.join(&change.path);
        physical(&dst, true)?;
        let existing = match fs::symlink_metadata(&dst) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(Error(format!(
                        "unsafe component recovery destination: {}",
                        dst.display()
                    )));
                }
                Some(meta)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::from_io(error)),
        };
        if change.existed {
            let backup = stage.join(".rollback").join(&change.path);
            match fs::symlink_metadata(&backup) {
                Ok(meta) if meta.is_file() => {
                    physical(&backup, false)?;
                    fs::rename(&backup, &dst).map_err(Error::from_io)?;
                }
                Ok(_) => {
                    return Err(Error(format!(
                        "unsafe component recovery copy: {}",
                        backup.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if existing.is_none() {
                        return Err(Error(format!(
                            "component and recovery copy are both missing: {}",
                            change.path
                        )));
                    }
                }
                Err(error) => return Err(Error::from_io(error)),
            }
        } else if change.install {
            let staged = stage.join(&change.path);
            match fs::symlink_metadata(&staged) {
                Ok(meta) if meta.is_file() => continue,
                Ok(_) => {
                    return Err(Error(format!(
                        "unsafe staged component copy: {}",
                        staged.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::from_io(error)),
            }
            if existing.is_none() {
                continue;
            }
            fs::rename(&dst, &staged).map_err(Error::from_io)?;
        }
    }
    fs::remove_file(&journal_path).map_err(Error::from_io)
}

/// Go: storage.go:286-294 — `pruneEmptyParents`.
pub(super) fn prune_empty_parents(root: &Path, obsolete: &[String]) {
    for name in obsolete {
        let mut parent = dir_name(name);
        while managed_name(&parent) {
            if fs::remove_dir(root.join(&parent)).is_err() {
                break;
            }
            parent = dir_name(&parent);
        }
    }
}

pub(super) struct TempDownload {
    pub(super) file: File,
    pub(super) path: PathBuf,
}

impl TempDownload {
    /// Keep the file after it has been atomically renamed to its destination.
    pub(super) fn disarm(&mut self) {
        self.path = PathBuf::new();
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Go: main.go:434 — `os.CreateTemp(filepath.Dir(destination), ".download-")`.
pub(super) fn create_download_temp(directory: &Path) -> Result<TempDownload> {
    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let path = directory.join(format!(
            "{DOWNLOAD_TEMP_PREFIX}{}-{sequence}-{stamp}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => return Ok(TempDownload { file, path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::from_io(error)),
        }
    }
    Err(Error("unable to allocate a partial download file".into()))
}
