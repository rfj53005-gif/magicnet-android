//! Rust component installer: release-pinned payload verification, mirror
//! routing, parallel transfers, atomic promotion, rollback and legacy cleanup.
//!
//! This is the semantic port of `installer/components/` (Go). The Go helper is
//! intentionally kept in the tree; this CLI command is the Rust control-plane
//! entry point and is exercised by `installer/tests.rs`.
//!
//! Filesystem layout, the owned workspace, the promotion journal, the
//! `components.installed.json` state file and the legacy ownership proofs are
//! identical to Go so either implementation can finish the other's interrupted
//! upgrade.

mod constants;
mod data;
mod http;
mod legacy;
mod log;
mod mirrors;
mod storage;
mod zip;
#[cfg(test)]
mod tests;

use std::fmt;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::App;

use constants::{
    DOWNLOADS_DIR, INSTALLED_MANIFEST_LIMIT, INSTALLED_MANIFEST_MODE, INSTALLED_MANIFEST_PATH,
    MANIFEST_LIMIT,
};
use data::{Component, Manifest, PayloadFile};
use log::Log;
use mirrors::{DownloadJob, RouteHint};
use storage::{
    cleanup_workspace, copy_verified, create_dir_all_mode, lock_module, open_no_follow, physical,
    prepare_workspace, promote, prune_empty_parents, read_bounded, recover_promotion, regular,
    sha256_hex, valid_file,
};
use zip::ZipArchive;

#[derive(Debug)]
pub(super) struct Error(String);

impl Error {
    pub(super) fn from_io(error: io::Error) -> Self {
        Self(error.to_string())
    }

    pub(super) fn message(&self) -> String {
        self.0.clone()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub(super) type Result<T> = std::result::Result<T, Error>;

/// Installer configuration. Fields are private to the crate and only tests may
/// set the timing/transport seams.
struct Config {
    archive: PathBuf,
    module_dir: PathBuf,
    previous_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    offline: bool,
    mirrors: Option<Vec<String>>,
    base_url: Option<String>,
    log: Log,
    body_idle_timeout: Option<Duration>,
    probe_timeout: Option<Duration>,
    slow_window: Option<Duration>,
    request_timeout: Option<Duration>,
    /// Test-only seam: loopback HTTP servers need `--proto =http,https`.
    allow_http: bool,
    curl: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            archive: PathBuf::new(),
            module_dir: PathBuf::new(),
            previous_dir: PathBuf::from("/data/adb/modules/MagicNet"),
            cache_dir: None,
            offline: false,
            mirrors: None,
            base_url: None,
            log: Log::Discard,
            body_idle_timeout: None,
            probe_timeout: None,
            slow_window: None,
            request_timeout: None,
            allow_http: false,
            curl: None,
        }
    }
}

struct InstallReport {
    version: String,
    components: usize,
}

struct Installer<'a> {
    cfg: &'a Config,
    mirrors: Vec<String>,
    route: Mutex<RouteHint>,
    route_select: Mutex<()>,
}

impl<'a> Installer<'a> {
    pub(super) fn new(cfg: &'a Config) -> Self {
        let mirrors = cfg
            .mirrors
            .clone()
            .unwrap_or_else(mirrors::default_mirrors);
        Self {
            cfg,
            mirrors,
            route: Mutex::new(RouteHint::default()),
            route_select: Mutex::new(()),
        }
    }

    /// Go: main.go:629-814 — `run`. One shot: validate, stage, verify, promote,
    /// recover on failure, and remove the owned workspace.
    pub(super) fn run(&self) -> Result<()> {
        let started = Instant::now();
        if self.cfg.archive.as_os_str().is_empty() || self.cfg.module_dir.as_os_str().is_empty() {
            return Err(Error(
                "archive and module directory are required".into(),
            ));
        }
        if let Some(cache) = &self.cfg.cache_dir {
            physical(cache, true)?;
        }
        let _lock = lock_module(&self.cfg.module_dir)?;
        let stage = prepare_workspace(&self.cfg.module_dir)?;
        let (report, mut error) = match self.install_staged(&stage) {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error)),
        };
        if let Err(recovery) = recover_promotion(&stage, &self.cfg.module_dir) {
            let message = format!(
                "component recovery retained at {}: {}",
                stage.display(),
                recovery.message()
            );
            return Err(Error(join_error(error, message)));
        }
        if let Err(cleanup) = cleanup_workspace(&stage) {
            error = Some(Error(join_error(error, cleanup.message())));
        }
        if let Some(error) = error {
            return Err(error);
        }
        if let Some(report) = report {
            self.cfg.log.line(format_args!(
                "[done] MagicNet {} ready: {} components in {:.1}s",
                report.version,
                report.components,
                started.elapsed().as_secs_f64()
            ));
        }
        Ok(())
    }

    fn install_staged(&self, stage: &Path) -> Result<InstallReport> {
        let archive = ZipArchive::open(&self.cfg.archive)?;
        let raw = archive.read_small_entry("components.json", MANIFEST_LIMIT)?;
        let manifest = Manifest::parse(&raw)?;
        manifest.validate()?;
        self.cfg.log.line(format_args!(
            "[install] MagicNet {}: checking {} components",
            manifest.version,
            manifest.components.len()
        ));
        let old_files = read_bounded(
            &self.cfg.module_dir.join(INSTALLED_MANIFEST_PATH),
            INSTALLED_MANIFEST_LIMIT,
        )
        .map(|raw| data::old_managed_files(&raw))
        .unwrap_or_default();
        let base = self.base_url(&manifest);
        let downloads_dir = stage.join(DOWNLOADS_DIR);
        let mut files: Vec<PayloadFile> = Vec::new();
        let mut wanted: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cached_components: Vec<(Component, PathBuf)> = Vec::new();
        let mut downloads: Vec<DownloadJob> = Vec::new();
        let mut scheduled: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for component in &manifest.components {
            for file in &component.files {
                files.push(file.clone());
                wanted.insert(file.path.clone());
            }
            let mut reused = false;
            for root in [&self.cfg.module_dir, &self.cfg.previous_dir] {
                match self.stage_local(component, root, stage) {
                    Ok(true) => {
                        self.cfg
                            .log
                            .line(format_args!("[reuse] {}: installed files match", component.id));
                        reused = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => return Err(error),
                }
            }
            if reused {
                continue;
            }
            if archive.has_files(&component.files) {
                self.cfg.log.line(format_args!(
                    "[bundled] {}: verifying offline payload",
                    component.id
                ));
                self.stage_zip(&archive, component, stage, false)?;
                continue;
            }
            // --cache-dir is an optional read-only archive source. Downloads and
            // their partial files belong only to this installation's workspace.
            let mut cached = downloads_dir.join(format!("{}.zip", component.sha256));
            let mut imported = false;
            if let Some(cache_dir) = &self.cfg.cache_dir {
                let source = cache_dir.join(format!("{}.zip", component.sha256));
                if valid_file(&source, component.size, &component.sha256) {
                    cached = source;
                    imported = true;
                    self.cfg.log.line(format_args!(
                        "[cache] {}: verified imported component",
                        component.id
                    ));
                }
            }
            if !imported {
                if self.cfg.offline {
                    return Err(Error(format!(
                        "{}: required component is unavailable offline",
                        component.id
                    )));
                }
                create_dir_all_mode(&downloads_dir, 0o700)?;
                if scheduled.insert(cached.clone()) {
                    self.cfg.log.line(format_args!(
                        "[download] {}: need {}",
                        component.id,
                        log::human_bytes(component.size)
                    ));
                    downloads.push(DownloadJob {
                        component: component.clone(),
                        direct: format!("{base}{}", component.asset),
                        destination: cached.clone(),
                    });
                }
            }
            cached_components.push((component.clone(), cached));
        }
        self.download_components(downloads)?;
        for (component, path) in cached_components {
            if !valid_file(&path, component.size, &component.sha256) {
                return Err(Error(format!(
                    "{}: cached component failed post-download verification",
                    component.id
                )));
            }
            let payload = ZipArchive::open(&path)?;
            if payload.len() != component.files.len() {
                return Err(Error(
                    "component ZIP contains unexpected files".into(),
                ));
            }
            self.stage_zip(&payload, &component, stage, true)?;
            if path.parent() == Some(downloads_dir.as_path()) {
                std::fs::remove_file(&path).map_err(Error::from_io)?;
            }
        }
        let mut obsolete = Vec::new();
        for name in old_files {
            if storage::managed_name(&name) && wanted.insert(name.clone()) {
                obsolete.push(name);
            }
        }
        let state_file = PayloadFile {
            path: INSTALLED_MANIFEST_PATH.to_string(),
            sha256: sha256_hex(&raw),
            size: raw.len() as i64,
            mode: INSTALLED_MANIFEST_MODE,
        };
        copy_verified(
            Cursor::new(raw.as_slice()),
            &stage.join(INSTALLED_MANIFEST_PATH),
            &state_file,
        )?;
        files.push(state_file);
        promote(stage, &self.cfg.module_dir, &files, &obsolete)?;
        prune_empty_parents(&self.cfg.module_dir, &obsolete);
        Ok(InstallReport {
            version: manifest.version,
            components: manifest.components.len(),
        })
    }

    fn stage_local(&self, component: &Component, root: &Path, stage: &Path) -> Result<bool> {
        if root.as_os_str().is_empty() {
            return Ok(false);
        }
        for file in &component.files {
            if !valid_file(&root.join(&file.path), file.size, &file.sha256) {
                return Ok(false);
            }
        }
        for file in &component.files {
            let name = root.join(&file.path);
            regular(&name)?;
            let mut source = open_no_follow(&name).map_err(Error::from_io)?;
            copy_verified(&mut source, &stage.join(&file.path), file)?;
        }
        Ok(true)
    }

    fn stage_zip(
        &self,
        archive: &ZipArchive,
        component: &Component,
        stage: &Path,
        exact: bool,
    ) -> Result<()> {
        if exact && archive.len() != component.files.len() {
            return Err(Error(
                "component ZIP contains unexpected files".into(),
            ));
        }
        for file in &component.files {
            let entry = archive.get(&file.path).ok_or_else(|| {
                Error(format!("missing/unsafe component entry: {}", file.path))
            })?;
            if !entry.is_file || entry.size != file.size as u64 {
                return Err(Error(format!(
                    "missing/unsafe component entry: {}",
                    file.path
                )));
            }
            let mut reader = archive.entry_reader(entry)?;
            copy_verified(&mut reader, &stage.join(&file.path), file)?;
            reader.finish()?;
        }
        Ok(())
    }

    fn base_url(&self, manifest: &Manifest) -> String {
        self.cfg.base_url.clone().unwrap_or_else(|| {
            format!(
                "https://github.com/{}/releases/download/{}/",
                manifest.repository, manifest.version
            )
        })
    }

    /// Prefetch every component payload into `--cache-dir`, hash-verified.
    /// This is a Rust-side addition; it reuses the exact Go download policy
    /// (route probing, 20 s idle, parallel workers, whole-archive SHA-256).
    pub(super) fn fetch(&self) -> Result<()> {
        if self.cfg.archive.as_os_str().is_empty() {
            return Err(Error("archive is required".into()));
        }
        let Some(cache_dir) = &self.cfg.cache_dir else {
            return Err(Error("cache directory is required for fetch".into()));
        };
        physical(cache_dir, true)?;
        create_dir_all_mode(cache_dir, 0o700)?;
        let archive = ZipArchive::open(&self.cfg.archive)?;
        let raw = archive.read_small_entry("components.json", MANIFEST_LIMIT)?;
        let manifest = Manifest::parse(&raw)?;
        manifest.validate()?;
        let base = self.base_url(&manifest);
        let mut downloads = Vec::new();
        let mut scheduled = std::collections::HashSet::new();
        let mut targets = Vec::new();
        for component in &manifest.components {
            let target = cache_dir.join(format!("{}.zip", component.sha256));
            targets.push((component.clone(), target.clone()));
            if valid_file(&target, component.size, &component.sha256) {
                self.cfg.log.line(format_args!(
                    "[cache] {}: verified imported component",
                    component.id
                ));
                continue;
            }
            if self.cfg.offline {
                return Err(Error(format!(
                    "{}: required component is unavailable offline",
                    component.id
                )));
            }
            if scheduled.insert(target.clone()) {
                self.cfg.log.line(format_args!(
                    "[download] {}: need {}",
                    component.id,
                    log::human_bytes(component.size)
                ));
                downloads.push(DownloadJob {
                    component: component.clone(),
                    direct: format!("{base}{}", component.asset),
                    destination: target,
                });
            }
        }
        self.download_components(downloads)?;
        for (component, target) in targets {
            if !valid_file(&target, component.size, &component.sha256) {
                return Err(Error(format!(
                    "{}: cached component failed post-download verification",
                    component.id
                )));
            }
        }
        self.cfg.log.line(format_args!(
            "[done] MagicNet {} components cached: {}",
            manifest.version,
            manifest.components.len()
        ));
        Ok(())
    }

    /// Read-only plan: validate the manifest and report each component's
    /// source. Never writes, locks, or downloads.
    pub(super) fn verify(&self) -> Result<()> {
        if self.cfg.archive.as_os_str().is_empty() {
            return Err(Error("archive is required".into()));
        }
        let archive = ZipArchive::open(&self.cfg.archive)?;
        let raw = archive.read_small_entry("components.json", MANIFEST_LIMIT)?;
        let manifest = Manifest::parse(&raw)?;
        manifest.validate()?;
        let mut download = 0usize;
        for component in &manifest.components {
            let mut source = "download";
            let mut reusable = false;
            for root in [&self.cfg.module_dir, &self.cfg.previous_dir] {
                if !root.as_os_str().is_empty()
                    && component
                        .files
                        .iter()
                        .all(|file| valid_file(&root.join(&file.path), file.size, &file.sha256))
                {
                    source = if std::ptr::eq(root, &self.cfg.module_dir) {
                        "installed"
                    } else {
                        "previous"
                    };
                    reusable = true;
                    break;
                }
            }
            if !reusable && archive.has_files(&component.files) {
                source = "bundled";
            } else if !reusable {
                if let Some(cache_dir) = &self.cfg.cache_dir {
                    let source_archive = cache_dir.join(format!("{}.zip", component.sha256));
                    if valid_file(&source_archive, component.size, &component.sha256) {
                        source = "cache";
                    }
                }
            }
            if source == "download" {
                download += 1;
            }
            self.cfg.log.line(format_args!(
                "[verify] {}: {}",
                component.id, source
            ));
        }
        self.cfg.log.line(format_args!(
            "[verify] MagicNet {}: {} components, {} need download",
            manifest.version,
            manifest.components.len(),
            download
        ));
        if self.cfg.offline && download > 0 {
            return Err(Error(format!(
                "{download} components are unavailable offline"
            )));
        }
        Ok(())
    }
}

fn join_error(current: Option<Error>, next: String) -> String {
    match current {
        Some(error) => format!("{}; {next}", error.message()),
        None => next,
    }
}

/// CLI entry point: `cli installer <install|fetch|verify|apply|cleanup-legacy>`.
pub(super) fn command(app: &App, args: &[String]) -> std::result::Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    if matches!(action, "" | "-h" | "--help" | "help") {
        print_usage();
        return Ok(());
    }
    let flags = parse_flags(&args[1..])?;
    let module_dir = flags
        .module_dir
        .clone()
        .unwrap_or_else(|| app.moddir.clone());
    if action == "cleanup-legacy" {
        if flags.module_dir.is_none() {
            return Err("module directory is required".to_string());
        }
        return legacy::cleanup_legacy_storage(&module_dir).map_err(|error| error.message());
    }
    let archive = flags
        .archive
        .ok_or_else(|| "installer archive is required (--archive <zip>)".to_string())?;
    let mut config = Config {
        archive,
        module_dir: module_dir.clone(),
        previous_dir: flags
            .previous_dir
            .unwrap_or_else(|| PathBuf::from("/data/adb/modules/MagicNet")),
        cache_dir: flags.cache_dir,
        offline: flags.offline,
        log: Log::Stdout,
        ..Config::default()
    };
    if action == "apply" {
        if config.cache_dir.is_none() {
            return Err("apply requires --cache-dir".to_string());
        }
        config.offline = true;
    }
    let installer = Installer::new(&config);
    let result = match action {
        "install" => installer.run(),
        "apply" => installer.run(),
        "fetch" => installer.fetch(),
        "verify" => installer.verify(),
        other => {
            return Err(format!(
                "unknown installer action: {other}\n{}",
                usage()
            ))
        }
    };
    result.map_err(|error| error.message())
}

#[derive(Default)]
struct Flags {
    archive: Option<PathBuf>,
    module_dir: Option<PathBuf>,
    previous_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    offline: bool,
}

fn parse_flags(args: &[String]) -> std::result::Result<Flags, String> {
    let mut flags = Flags::default();
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--offline" => flags.offline = true,
            "--archive" | "--module-dir" | "--previous-dir" | "--cache-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{} requires a value", args[index - 1]))?;
                if value.is_empty() {
                    return Err(format!("{} requires a non-empty value", args[index - 1]));
                }
                let path = PathBuf::from(value);
                match args[index - 1].as_str() {
                    "--archive" => flags.archive = Some(path),
                    "--module-dir" => flags.module_dir = Some(path),
                    "--previous-dir" => flags.previous_dir = Some(path),
                    "--cache-dir" => flags.cache_dir = Some(path),
                    _ => unreachable!(),
                }
            }
            other => return Err(format!("unknown installer option: {other}\n{}", usage())),
        }
        index += 1;
    }
    Ok(flags)
}

fn usage() -> String {
    "usage: cli installer {install|fetch|verify|apply} --archive <zip> [--module-dir <dir>] [--previous-dir <dir>] [--cache-dir <dir>] [--offline]\n       cli installer cleanup-legacy --module-dir <dir>".to_string()
}

fn print_usage() {
    println!("{}", usage());
}
