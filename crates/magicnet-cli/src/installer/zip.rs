//! Minimal, validating ZIP reader used by the installer.
//!
//! Go uses `archive/zip` in process. The Rust CLI has no ZIP dependency and
//! must also run on Android without one, so this module parses the central
//! directory itself (bounded, seek-based) and streams entry payloads through
//! the trusted system/busybox `unzip -p`, exactly like
//! `module_update/archive.rs` already does. Entries are only ever extracted by
//! their exact, pre-validated names; no archive-wide extraction happens.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use super::storage::safe_name;
use super::{Error, Result};

const LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
const EOCD: &[u8; 4] = b"PK\x05\x06";
const MAX_CENTRAL_BYTES: u64 = 64 * 1024 * 1024;
const UNZIP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub(super) struct ZipEntry {
    pub(super) name: String,
    pub(super) is_file: bool,
    pub(super) size: u64,
}

pub(super) struct ZipArchive {
    path: PathBuf,
    entries: BTreeMap<String, ZipEntry>,
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|part| u16::from_le_bytes([part[0], part[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|part| u32::from_le_bytes([part[0], part[1], part[2], part[3]]))
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    let lower = bytes.len().saturating_sub(22 + u16::MAX as usize);
    (lower..=bytes.len().checked_sub(22)?)
        .rev()
        .find(|offset| {
            bytes.get(*offset..*offset + 4) == Some(EOCD)
                && u16_at(bytes, *offset + 20)
                    .is_some_and(|comment| *offset + 22 + comment as usize == bytes.len())
        })
}

impl ZipArchive {
    /// Go: main.go:214-229 — `zipIndex`. Directory entries are validated but
    /// not indexed; file names must be safe and unique.
    pub(super) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(Error::from_io)?;
        let file_len = file.metadata().map_err(Error::from_io)?.len();
        if file_len < 22 {
            return Err(Error(format!("invalid ZIP archive: {}", path.display())));
        }
        let tail_len = (22 + u16::MAX as usize).min(file_len as usize);
        let mut tail = vec![0u8; tail_len];
        file.read_exact_at(&mut tail, file_len - tail_len as u64)
            .map_err(Error::from_io)?;
        let eocd_in_tail = find_eocd(&tail).ok_or_else(|| {
            Error(format!("missing ZIP end record: {}", path.display()))
        })?;
        let eocd = file_len - tail_len as u64 + eocd_in_tail as u64;
        let eocd_bytes = &tail[eocd_in_tail..];
        let count = u16_at(eocd_bytes, 10).unwrap_or(0) as usize;
        let central_size = u32_at(eocd_bytes, 12).unwrap_or(0) as u64;
        let central_start = u32_at(eocd_bytes, 16).unwrap_or(0) as u64;
        if u16_at(eocd_bytes, 4) != Some(0)
            || u16_at(eocd_bytes, 6) != Some(0)
            || u16_at(eocd_bytes, 8).map(usize::from) != Some(count)
            || count == 0
            || central_size == u32::MAX as u64
            || central_start == u32::MAX as u64
            || central_size > MAX_CENTRAL_BYTES
            || central_start.checked_add(central_size) != Some(eocd)
        {
            return Err(Error(format!(
                "unsupported or invalid ZIP directory: {}",
                path.display()
            )));
        }
        let mut central = vec![0u8; central_size as usize];
        file.read_exact_at(&mut central, central_start)
            .map_err(Error::from_io)?;
        let mut cursor = 0usize;
        let mut entries = BTreeMap::new();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for _ in 0..count {
            if central.get(cursor..cursor + 4) != Some(CENTRAL_HEADER)
                || cursor + 46 > central.len()
            {
                return Err(Error(format!(
                    "invalid ZIP central directory: {}",
                    path.display()
                )));
            }
            let flags = u16_at(&central, cursor + 8).unwrap_or(0);
            let method = u16_at(&central, cursor + 10).unwrap_or(0);
            let crc = u32_at(&central, cursor + 16).unwrap_or(0);
            let compressed = u32_at(&central, cursor + 20).unwrap_or(0);
            let size = u32_at(&central, cursor + 24).unwrap_or(0);
            let name_len = u16_at(&central, cursor + 28).unwrap_or(0) as usize;
            let extra_len = u16_at(&central, cursor + 30).unwrap_or(0) as usize;
            let comment_len = u16_at(&central, cursor + 32).unwrap_or(0) as usize;
            let external = u32_at(&central, cursor + 38).unwrap_or(0);
            let local = u32_at(&central, cursor + 42).unwrap_or(0);
            let creator = (u16_at(&central, cursor + 4).unwrap_or(0) >> 8) as u8;
            let next = cursor
                .checked_add(46 + name_len + extra_len + comment_len)
                .filter(|next| *next <= central.len())
                .ok_or_else(|| Error(format!("invalid ZIP entry: {}", path.display())))?;
            // ZIP64, encryption and non-store/deflate methods are rejected.
            if compressed == u32::MAX
                || size == u32::MAX
                || local == u32::MAX
                || flags & 1 != 0
                || !matches!(method, 0 | 8)
                || (method == 0 && compressed != size)
                || (size > 0 && compressed == 0)
            {
                return Err(Error(format!(
                    "unsupported ZIP entry in {}",
                    path.display()
                )));
            }
            let raw_name = central
                .get(cursor + 46..cursor + 46 + name_len)
                .ok_or_else(|| Error(format!("invalid ZIP entry: {}", path.display())))?;
            let name = std::str::from_utf8(raw_name)
                .map_err(|_| Error(format!("non-UTF-8 ZIP entry: {}", path.display())))?
                .to_string();
            let directory = name.ends_with('/');
            let canonical = name.strip_suffix('/').unwrap_or(&name);
            if !safe_name(canonical) {
                return Err(Error(format!(
                    "unsafe ZIP entry {:?} in {}",
                    name,
                    path.display()
                )));
            }
            let symlink = creator == 3 && ((external >> 16) & 0o170000) == 0o120000;
            let is_file = !directory && !symlink;
            // Read and validate the local header so the central directory
            // cannot alias a different name/method than the extractor sees.
            let mut local_header = [0u8; 30];
            file.read_exact_at(&mut local_header, local as u64)
                .map_err(|_| Error(format!("invalid ZIP local header: {}", path.display())))?;
            if &local_header[0..4] != LOCAL_HEADER {
                return Err(Error(format!(
                    "invalid ZIP local header: {}",
                    path.display()
                )));
            }
            let local_flags = u16::from_le_bytes([local_header[6], local_header[7]]);
            let local_method = u16::from_le_bytes([local_header[8], local_header[9]]);
            let local_crc = u32::from_le_bytes([
                local_header[14],
                local_header[15],
                local_header[16],
                local_header[17],
            ]);
            let local_compressed = u32::from_le_bytes([
                local_header[18],
                local_header[19],
                local_header[20],
                local_header[21],
            ]);
            let local_size = u32::from_le_bytes([
                local_header[22],
                local_header[23],
                local_header[24],
                local_header[25],
            ]);
            let local_name_len = u16::from_le_bytes([local_header[26], local_header[27]]) as usize;
            let local_extra_len = u16::from_le_bytes([local_header[28], local_header[29]]) as usize;
            if local_flags != flags
                || local_method != method
                || local_name_len != name_len
                || (flags & 0x0008 == 0
                    && (local_crc != crc
                        || local_compressed != compressed
                        || local_size != size))
            {
                return Err(Error(format!(
                    "inconsistent ZIP local header: {}",
                    path.display()
                )));
            }
            let data_start = (local as u64)
                .checked_add(30 + local_name_len as u64 + local_extra_len as u64)
                .filter(|start| *start <= central_start)
                .ok_or_else(|| Error(format!("invalid ZIP data offset: {}", path.display())))?;
            let data_end = data_start
                .checked_add(compressed as u64)
                .filter(|end| *end <= central_start)
                .ok_or_else(|| Error(format!("invalid ZIP data range: {}", path.display())))?;
            let mut local_name = vec![0u8; local_name_len];
            file.read_exact_at(&mut local_name, (local as u64) + 30)
                .map_err(|_| Error(format!("invalid ZIP local name: {}", path.display())))?;
            if local_name != raw_name {
                return Err(Error(format!(
                    "mismatched ZIP entry name: {}",
                    path.display()
                )));
            }
            if !directory {
                if entries.contains_key(canonical) {
                    return Err(Error(format!(
                        "duplicate ZIP entry {:?} in {}",
                        canonical,
                        path.display()
                    )));
                }
                entries.insert(
                    canonical.to_string(),
                    ZipEntry {
                        name: canonical.to_string(),
                        is_file,
                        size: size as u64,
                    },
                );
            }
            ranges.push((local as u64, data_end));
            cursor = next;
        }
        if cursor != central.len() {
            return Err(Error(format!(
                "trailing ZIP central data: {}",
                path.display()
            )));
        }
        ranges.sort_unstable();
        let mut previous_end = 0u64;
        for (start, end) in ranges {
            if start < previous_end {
                return Err(Error(format!(
                    "overlapping ZIP entries: {}",
                    path.display()
                )));
            }
            previous_end = end;
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    pub(super) fn get(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.get(name)
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn has_files(&self, files: &[super::data::PayloadFile]) -> bool {
        files
            .iter()
            .all(|file| self.entries.get(&file.path).is_some_and(|entry| entry.is_file))
    }

    /// Read a small entry fully into memory (used only for `components.json`).
    pub(super) fn read_small_entry(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        let entry = self
            .get(name)
            .filter(|entry| entry.is_file)
            .ok_or_else(|| Error(format!("missing ZIP entry: {name}")))?;
        if entry.size > limit {
            return Err(Error(format!("oversized ZIP entry: {name}")));
        }
        let mut reader = self.entry_reader(entry)?;
        let mut raw = Vec::with_capacity(entry.size as usize);
        (&mut reader)
            .take(entry.size + 1)
            .read_to_end(&mut raw)
            .map_err(Error::from_io)?;
        if raw.len() as u64 != entry.size {
            let _ = reader.finish();
            return Err(Error(format!("truncated ZIP entry: {name}")));
        }
        reader.finish()?;
        Ok(raw)
    }

    pub(super) fn entry_reader(&self, entry: &ZipEntry) -> Result<UnzipReader> {
        let (program, busybox) = trusted_unzip()
            .ok_or_else(|| Error("no trusted unzip is available".into()))?;
        let mut command = Command::new(program);
        if busybox {
            command.arg("unzip");
        }
        command
            .env_clear()
            .env("PATH", "/system/bin:/system/xbin:/vendor/bin:/usr/bin:/bin")
            .env("LC_ALL", "C")
            .arg("-p")
            .arg(&self.path)
            .arg(unzip_pattern(&entry.name))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(Error::from_io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error("unzip stdout is unavailable".into()))?;
        Ok(UnzipReader {
            child,
            stdout,
            expected: entry.size,
            count: 0,
            deadline: Instant::now() + UNZIP_TIMEOUT,
        })
    }
}

/// Escape Info-ZIP/BusyBox wildcard characters so one entry name can only
/// match itself. Content is still SHA-256 verified against the manifest.
fn unzip_pattern(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        match character {
            '*' => out.push_str("[*]"),
            '?' => out.push_str("[?]"),
            '[' => out.push_str("[[]"),
            other => out.push(other),
        }
    }
    out
}

fn trusted_unzip() -> Option<(&'static str, bool)> {
    #[cfg(target_os = "android")]
    let candidates = [
        ("/system/bin/unzip", false),
        ("/data/adb/ksu/bin/busybox", true),
        ("/data/adb/magisk/busybox", true),
        ("/data/adb/ap/bin/busybox", true),
    ];
    #[cfg(not(target_os = "android"))]
    let candidates = [("/usr/bin/unzip", false), ("/bin/unzip", false)];
    candidates
        .into_iter()
        .find(|(path, _)| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
}

/// Streaming reader over `unzip -p` output for one entry.
pub(super) struct UnzipReader {
    child: Child,
    stdout: ChildStdout,
    expected: u64,
    count: u64,
    deadline: Instant,
}

impl UnzipReader {
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        loop {
            let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if result > 0 {
                return Ok(true);
            }
            if result == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Require the child to have produced exactly the declared entry size and
    /// exited successfully (unzip also validates the entry CRC).
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.count != self.expected {
            let _ = self.child.kill();
            let _ = self.child.wait();
            return Err(Error("truncated ZIP entry".into()));
        }
        let mut byte = [0u8; 1];
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(Error("timed out reading a ZIP entry".into()));
            }
            if !self.wait_readable(remaining.min(Duration::from_secs(5))).map_err(Error::from_io)? {
                continue;
            }
            match self.stdout.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err(Error("unexpected trailing ZIP entry bytes".into()));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::from_io(error)),
            }
        }
        let status = self.child.wait().map_err(Error::from_io)?;
        if !status.success() {
            return Err(Error("unzip rejected a ZIP entry".into()));
        }
        Ok(())
    }
}

impl Read for UnzipReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let limit = self.expected.saturating_add(1).saturating_sub(self.count);
        if limit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP entry exceeded its declared size",
            ));
        }
        let length = buffer.len().min(limit as usize);
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reading a ZIP entry",
                ));
            }
            if !self.wait_readable(remaining.min(Duration::from_secs(5)))? {
                continue;
            }
            match self.stdout.read(&mut buffer[..length]) {
                Ok(n) => {
                    self.count += n as u64;
                    return Ok(n);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for UnzipReader {
    fn drop(&mut self) {
        if self.count != self.expected || self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
