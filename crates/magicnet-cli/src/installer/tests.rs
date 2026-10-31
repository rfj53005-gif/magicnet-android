//! Regression suite for the Rust installer. Each Go test in
//! `installer/components/*_test.go` has a Rust counterpart; the mapping is
//! documented in `docs/rust-installer-equivalence.md`.

#![allow(clippy::needless_range_loop)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use super::constants::*;
use super::data::*;
use super::legacy::cleanup_legacy_storage;
use super::log::Log;
use super::mirrors::*;
use super::storage::*;
use super::{Config, Installer, Result};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(super) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(super) fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "magicnet-installer-test-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn write_file(path: &Path, value: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(path, value).expect("write fixture");
}

pub(super) fn assert_content(path: &Path, value: &[u8]) {
    let got = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert_eq!(got, value, "unexpected content at {}", path.display());
}

pub(super) fn assert_absent(path: &Path) {
    assert!(
        fs::symlink_metadata(path).is_err(),
        "unexpected path: {}",
        path.display()
    );
}

pub(super) fn assert_no_workspace(root: &Path) {
    assert!(
        fs::symlink_metadata(&root.join(WORKSPACE_NAME)).is_err(),
        "component workspace remains at {}",
        root.display()
    );
    if let Some(parent) = root.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                assert!(
                    !name.to_string_lossy().starts_with(".magicnet-components"),
                    "components polluted the module parent: {}",
                    entry.path().display()
                );
            }
        }
    }
}

pub(super) fn log_buffer() -> (Log, Arc<Mutex<Vec<u8>>>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    (Log::Buffer(buffer.clone()), buffer)
}

pub(super) struct Fixture {
    pub(super) root: TempDir,
    pub(super) config: Config,
    pub(super) log: Arc<Mutex<Vec<u8>>>,
}

pub(super) fn options_fixture(archive: &Path) -> Fixture {
    let root = TempDir::new();
    let (log, buffer) = log_buffer();
    let config = Config {
        archive: archive.to_path_buf(),
        module_dir: root.join("new"),
        previous_dir: root.join("old"),
        cache_dir: Some(root.join("cache")),
        offline: true,
        mirrors: Some(Vec::new()),
        base_url: None,
        log,
        body_idle_timeout: None,
        probe_timeout: None,
        slow_window: None,
        request_timeout: None,
        allow_http: true,
        curl: None,
    };
    Fixture {
        root,
        config,
        log: buffer,
    }
}

pub(super) fn install(fixture: &Fixture) -> Result<()> {
    Installer::new(&fixture.config).run()
}

pub(super) fn manifest_fixture(parts: Vec<Component>) -> Manifest {
    Manifest {
        schema: 1,
        module: "MagicNet".into(),
        version: "v1.4.8".into(),
        architecture: compiled_architecture().into(),
        repository: "LIghtJUNction/MagicNet".into(),
        components: parts,
    }
}

pub(super) fn manifest_json(manifest: &Manifest) -> Vec<u8> {
    let components: Vec<_> = manifest
        .components
        .iter()
        .map(|component| {
            let files: Vec<_> = component
                .files
                .iter()
                .map(|file| {
                    json!({
                        "path": file.path,
                        "sha256": file.sha256,
                        "size": file.size,
                        "mode": file.mode,
                    })
                })
                .collect();
            json!({
                "id": component.id,
                "sha256": component.sha256,
                "size": component.size,
                "asset": component.asset,
                "files": files,
            })
        })
        .collect();
    serde_json::to_vec(&json!({
        "schema": manifest.schema,
        "module": manifest.module,
        "version": manifest.version,
        "architecture": manifest.architecture,
        "repository": manifest.repository,
        "components": components,
    }))
    .expect("serialize manifest")
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Minimal stored-entry ZIP writer used only by tests.
pub(super) fn zip_files(files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mode) in files {
        let name = name.as_bytes();
        let crc = crc32(data);
        let offset = out.len() as u32;
        let external = (0o100000 | (mode & 0o777)) << 16;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0800u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(data);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&0x0314u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&external.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub(super) fn zip_regular(files: &[(&str, &[u8])]) -> Vec<u8> {
    let owned: Vec<(String, Vec<u8>, u32)> = files
        .iter()
        .map(|(name, data)| (name.to_string(), data.to_vec(), 0o644))
        .collect();
    zip_files(&owned)
}

pub(super) fn part(id: &str, name: &str, value: &[u8]) -> (Component, Vec<u8>) {
    let payload = zip_regular(&[(name, value)]);
    let sha = sha256_hex(&payload);
    let component = Component {
        id: id.to_string(),
        sha256: sha.clone(),
        size: payload.len() as i64,
        asset: format!("MagicNet-component-{id}-{sha}.zip"),
        files: vec![PayloadFile {
            path: name.to_string(),
            sha256: sha256_hex(value),
            size: value.len() as i64,
            mode: 0o755,
        }],
    };
    (component, payload)
}

pub(super) fn archive_fixture(root: &Path, manifest: &Manifest, bundled: &[(&str, &[u8])]) -> PathBuf {
    let raw = manifest_json(manifest);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![("components.json".into(), raw, 0o644)];
    for (name, data) in bundled {
        files.push((name.to_string(), data.to_vec(), 0o644));
    }
    let target = root.join("module.zip");
    write_file(&target, &zip_files(&files));
    target
}

// ---------------------------------------------------------------------------
// Loopback HTTP fixture
// ---------------------------------------------------------------------------

pub(super) struct Request {
    // Recorded by the parser for fidelity with Go's request
    // fixture; only the path drives routing decisions in these tests.
    #[allow(dead_code)]
    pub(super) method: String,
    pub(super) path: String,
    pub(super) headers: HashMap<String, String>,
}

impl Request {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

type Handler = Arc<dyn Fn(Request, TcpStream) + Send + Sync>;

pub(super) struct TestServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub(super) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(super) fn start_server<F>(handler: F) -> TestServer
where
    F: Fn(Request, TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking test server");
    let addr = listener.local_addr().expect("test server address");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handler: Handler = Arc::new(handler);
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let handler = handler.clone();
                    std::thread::spawn(move || {
                        if let Some(request) = read_request(&stream) {
                            handler(request, stream);
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    TestServer {
        addr: format!("127.0.0.1:{}", addr.port()),
        stop,
        handle: Some(handle),
    }
}

fn read_request(mut stream: &TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Some(Request {
        method,
        path,
        headers,
    })
}

pub(super) fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub(super) fn send_head(stream: &mut TcpStream, status: u16, headers: &[(String, String)]) {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

pub(super) fn send_response(
    stream: &mut TcpStream,
    status: u16,
    mut headers: Vec<(String, String)>,
    body: &[u8],
) {
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        headers.push(("Content-Length".into(), body.len().to_string()));
    }
    send_head(stream, status, &headers);
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

pub(super) fn respond_payload(stream: &mut TcpStream, payload: &[u8]) {
    send_response(stream, 200, Vec::new(), payload);
}

pub(super) fn range_parts(request: &Request, payload: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let Some(range) = request.header("range") else {
        return (
            200,
            vec![("Content-Length".into(), payload.len().to_string())],
            payload.to_vec(),
        );
    };
    let Some(rest) = range.strip_prefix("bytes=") else {
        return (416, Vec::new(), Vec::new());
    };
    let Some((start, end)) = rest.split_once('-') else {
        return (416, Vec::new(), Vec::new());
    };
    let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else {
        return (416, Vec::new(), Vec::new());
    };
    if start != 0 || end >= payload.len() || end < start {
        return (416, Vec::new(), Vec::new());
    }
    (
        206,
        vec![
            (
                "Content-Range".into(),
                format!("bytes 0-{end}/{}", payload.len()),
            ),
            ("Content-Length".into(), (end + 1).to_string()),
        ],
        payload[..=end].to_vec(),
    )
}

pub(super) fn serve_range(stream: &mut TcpStream, request: &Request, payload: &[u8]) {
    let (status, headers, body) = range_parts(request, payload);
    send_response(stream, status, headers, &body);
}

/// Trickle a payload in fixed-size chunks with a pause between them. Returns
/// true when the client disconnected before the transfer finished.
pub(super) fn trickle(
    stream: &mut TcpStream,
    payload: &[u8],
    chunk: usize,
    pause: Duration,
) -> bool {
    let mut offset = 0;
    while offset < payload.len() {
        let end = (offset + chunk).min(payload.len());
        if stream.write_all(&payload[offset..end]).is_err() {
            return true;
        }
        let _ = stream.flush();
        offset = end;
        if offset < payload.len() {
            std::thread::sleep(pause);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Go: main_test.go
// ---------------------------------------------------------------------------

#[test]
fn full_package_needs_no_network() {
    let (component, _) = part("engine", "bin/sing-box", b"engine");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![component]),
        &[("bin/sing-box", b"engine")],
    );
    let fixture = options_fixture(&archive);
    install(&fixture).expect("offline install");
    assert_content(
        &fixture.config.module_dir.join("bin/sing-box"),
        b"engine",
    );
    let mode = fs::metadata(fixture.config.module_dir.join("bin/sing-box"))
        .expect("installed file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "executable mode was not restored");
    let cache = fixture.config.cache_dir.as_ref().unwrap();
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            assert!(
                !entry.file_name().to_string_lossy().ends_with(".zip"),
                "offline install cached a payload archive"
            );
        }
    }
}

#[test]
fn unchanged_old_component_is_reused_without_manifest() {
    let (component, _) = part("engine", "bin/sing-box", b"same");
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component]), &[]);
    let fixture = options_fixture(&archive);
    write_file(&fixture.config.previous_dir.join("bin/sing-box"), b"same");
    install(&fixture).expect("reuse previous install");
    assert_content(
        &fixture.config.module_dir.join("bin/sing-box"),
        b"same",
    );
}

#[test]
fn corrupt_old_component_uses_verified_cache() {
    let (component, payload) = part("engine", "bin/sing-box", b"good");
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component.clone()]), &[]);
    let fixture = options_fixture(&archive);
    write_file(&fixture.config.previous_dir.join("bin/sing-box"), b"evil");
    let cache = fixture.config.cache_dir.as_ref().unwrap();
    write_file(&cache.join(format!("{}.zip", component.sha256)), &payload);
    install(&fixture).expect("install from verified cache");
    assert_content(
        &fixture.config.module_dir.join("bin/sing-box"),
        b"good",
    );
    assert_content(&fixture.config.previous_dir.join("bin/sing-box"), b"evil");
}

#[test]
fn missing_second_component_leaves_old_installation_untouched() {
    let (a, _) = part("a", "bin/a", b"new-a");
    let (b, _) = part("b", "bin/b", b"new-b");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![a, b]),
        &[("bin/a", b"new-a")],
    );
    let fixture = options_fixture(&archive);
    write_file(&fixture.config.module_dir.join("bin/a"), b"old-a");
    write_file(&fixture.config.module_dir.join("bin/b"), b"old-b");
    assert!(install(&fixture).is_err(), "missing component was accepted");
    assert_content(&fixture.config.module_dir.join("bin/a"), b"old-a");
    assert_content(&fixture.config.module_dir.join("bin/b"), b"old-b");
    assert_no_workspace(&fixture.config.module_dir);
}

#[test]
fn only_changed_component_downloads() {
    let (a, _) = part("a", "bin/a", b"unchanged");
    let (b, payload) = part("b", "bin/b", b"new");
    let expected = format!("/{}", b.asset);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = start_server(move |request, mut stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        if request.path == expected {
            respond_payload(&mut stream, &payload);
        } else {
            send_response(&mut stream, 404, Vec::new(), b"");
        }
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![a, b]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    write_file(&config.previous_dir.join("bin/a"), b"unchanged");
    write_file(&config.previous_dir.join("bin/b"), b"old");
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    install(&fixture).expect("download changed component");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "probe plus transfer");
    install(&fixture).expect("unchanged reinstall");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "unchanged reinstall accessed the network"
    );
}

#[test]
fn download_checksum_failure_preserves_installed_file() {
    let (component, _) = part("engine", "bin/sing-box", b"good");
    let size = component.size as usize;
    let server = start_server(move |_request, mut stream| {
        send_response(&mut stream, 200, Vec::new(), &vec![b'x'; size]);
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component.clone()]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    write_file(&config.module_dir.join("bin/sing-box"), b"old");
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    assert!(install(&fixture).is_err(), "tampered download was accepted");
    assert_content(&fixture.config.module_dir.join("bin/sing-box"), b"old");
    let cache = fixture.config.cache_dir.as_ref().unwrap();
    assert_absent(&cache.join(format!("{}.zip", component.sha256)));
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".download-"),
                "partial downloads leaked"
            );
        }
    }
    assert_no_workspace(&fixture.config.module_dir);
}

#[test]
fn truncated_download_is_not_cached() {
    let (component, _) = part("engine", "bin/sing-box", b"good");
    let declared = component.size.to_string();
    let server = start_server(move |_request, mut stream| {
        send_response(
            &mut stream,
            200,
            vec![("Content-Length".into(), declared.clone())],
            b"short",
        );
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component.clone()]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    assert!(install(&fixture).is_err(), "truncated payload accepted");
    let cache = fixture.config.cache_dir.as_ref().unwrap();
    assert_absent(&cache.join(format!("{}.zip", component.sha256)));
    assert_no_workspace(&fixture.config.module_dir);
}

#[test]
fn wrong_architecture_fails() {
    let (component, _) = part("engine", "bin/sing-box", b"engine");
    let mut manifest = manifest_fixture(vec![component]);
    manifest.architecture = "not-this-architecture".into();
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest, &[]);
    let fixture = options_fixture(&archive);
    assert!(install(&fixture).is_err(), "incompatible architecture accepted");
}

#[test]
fn unsafe_manifest_paths_fail() {
    for name in [
        "../outside",
        "/bin/root",
        "bin/../../escape",
        "bin\\evil",
        ".config/sing-box/config.json",
        "bin/magicnet-components",
    ] {
        let (component, _) = part("engine", name, b"bad");
        let manifest = manifest_fixture(vec![component]);
        assert!(
            manifest.validate().is_err(),
            "unsafe manifest path accepted: {name}"
        );
    }
}

#[test]
fn duplicate_ownership_and_parent_collision_fail() {
    let (a, _) = part("a", "bin/a", b"x");
    let mut b = a.clone();
    b.id = "b".into();
    b.asset = format!("MagicNet-component-b-{}.zip", b.sha256);
    assert!(
        manifest_fixture(vec![a, b]).validate().is_err(),
        "duplicate file ownership accepted"
    );
    let (a, _) = part("a", "webroot/a", b"x");
    let (b, _) = part("b", "webroot/a/b", b"x");
    assert!(
        manifest_fixture(vec![a, b]).validate().is_err(),
        "file/directory overlap accepted"
    );
}

#[test]
fn unexpected_archive_entry_fails() {
    let (mut component, _) = part("engine", "bin/sing-box", b"good");
    let payload = zip_regular(&[
        ("bin/sing-box", b"good"),
        ("../escape", b"bad"),
    ]);
    component.sha256 = sha256_hex(&payload);
    component.size = payload.len() as i64;
    component.asset = format!("MagicNet-component-{}-{}.zip", component.id, component.sha256);
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component.clone()]), &[]);
    let fixture = options_fixture(&archive);
    let cache = fixture.config.cache_dir.as_ref().unwrap();
    write_file(&cache.join(format!("{}.zip", component.sha256)), &payload);
    assert!(install(&fixture).is_err(), "unsafe payload accepted");
}

#[test]
fn destination_symlink_fails_without_writing_outside() {
    let (component, _) = part("engine", "bin/sing-box", b"new");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![component]),
        &[("bin/sing-box", b"new")],
    );
    let fixture = options_fixture(&archive);
    let outside = TempDir::new();
    write_file(&outside.path().join("sing-box"), b"outside");
    fs::create_dir_all(&fixture.config.module_dir).unwrap();
    std::os::unix::fs::symlink(outside.path(), fixture.config.module_dir.join("bin")).unwrap();
    assert!(install(&fixture).is_err(), "symlink destination accepted");
    assert_content(&outside.path().join("sing-box"), b"outside");
}

#[test]
fn corrupt_bundled_payload_fails() {
    let (component, _) = part("engine", "bin/sing-box", b"good");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![component]),
        &[("bin/sing-box", b"evil")],
    );
    let fixture = options_fixture(&archive);
    assert!(install(&fixture).is_err(), "corrupt offline payload accepted");
}

#[test]
fn promotion_preflight_preserves_old_files_on_invalid_stage() {
    let root = TempDir::new();
    let stage = TempDir::new();
    write_file(&root.path().join("bin/a"), b"old-a");
    write_file(&stage.path().join("bin/a"), b"new-a");
    let files = vec![
        PayloadFile {
            path: "bin/a".into(),
            sha256: String::new(),
            size: 0,
            mode: 0o644,
        },
        PayloadFile {
            path: "bin/missing".into(),
            sha256: String::new(),
            size: 0,
            mode: 0o644,
        },
    ];
    assert!(
        promote(stage.path(), root.path(), &files, &[]).is_err(),
        "promotion unexpectedly succeeded"
    );
    assert_content(&root.path().join("bin/a"), b"old-a");
}

#[test]
fn obsolete_managed_files_are_removed_but_config_survives() {
    let (component, _) = part("webui", "webroot/new.js", b"new");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![component]),
        &[("webroot/new.js", b"new")],
    );
    let fixture = options_fixture(&archive);
    let (mut old, _) = part("old", "webroot/old.js", b"old");
    old.files.push(PayloadFile {
        path: ".config/sing-box/config.json".into(),
        sha256: sha256_hex(b"private config"),
        size: 14,
        mode: 0o600,
    });
    write_file(
        &fixture.config.module_dir.join("components.installed.json"),
        &manifest_json(&manifest_fixture(vec![old])),
    );
    write_file(&fixture.config.module_dir.join("webroot/old.js"), b"old");
    write_file(
        &fixture
            .config
            .module_dir
            .join(".config/sing-box/config.json"),
        b"private config",
    );
    install(&fixture).expect("install with obsolete files");
    assert_absent(&fixture.config.module_dir.join("webroot/old.js"));
    assert_content(
        &fixture
            .config
            .module_dir
            .join(".config/sing-box/config.json"),
        b"private config",
    );
}

#[test]
fn mirror_fallback_after_direct_failure() {
    let (component, payload) = part("engine", "bin/sing-box", b"good");
    let mirror_calls = Arc::new(AtomicUsize::new(0));
    let counter = mirror_calls.clone();
    let server = start_server(move |request, mut stream| {
        if request.path.starts_with("/mirror/") {
            counter.fetch_add(1, Ordering::SeqCst);
            respond_payload(&mut stream, &payload);
        } else {
            send_response(&mut stream, 503, Vec::new(), b"");
        }
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(vec![format!("{}/mirror/", server.url())]);
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    install(&fixture).expect("install through mirror");
    assert!(
        mirror_calls.load(Ordering::SeqCst) >= 2,
        "mirror was not probed and used"
    );
}

// ---------------------------------------------------------------------------
// Go: mirrors_test.go, mirrors_slow_test.go
// ---------------------------------------------------------------------------

fn large_download_fixture(size: usize) -> (Component, Vec<u8>) {
    let mut payload = vec![b'x'; size];
    payload[..4].copy_from_slice(b"PK\x03\x04");
    (
        Component {
            id: "sample".into(),
            sha256: sha256_hex(&payload),
            size: size as i64,
            asset: "sample.zip".into(),
            files: Vec::new(),
        },
        payload,
    )
}

fn probe_config(module_dir: &Path) -> Config {
    Config {
        module_dir: module_dir.to_path_buf(),
        previous_dir: PathBuf::new(),
        mirrors: Some(Vec::new()),
        log: Log::Discard,
        allow_http: true,
        ..Config::default()
    }
}

#[test]
fn default_mirror_candidates_are_distinct_https() {
    let mirrors = default_mirrors();
    assert!(
        mirrors.len() >= 7 && mirrors.len() <= MAX_MIRROR_ROUTES,
        "unexpected candidate pool size"
    );
    let mut seen = std::collections::HashSet::new();
    for raw in mirrors {
        let Some(rest) = raw.strip_prefix("https://") else {
            panic!("mirror is not HTTPS: {raw}");
        };
        let authority = rest.split('/').next().unwrap_or("");
        assert!(!authority.is_empty(), "missing host: {raw}");
        assert!(!authority.contains('@'), "credentials in mirror: {raw}");
        assert!(!raw.contains('?') && !raw.contains('#'), "query in mirror");
        assert!(raw.ends_with('/'), "mirror path must be /");
        assert!(seen.insert(raw.clone()), "duplicate built-in mirror {raw}");
    }
}

#[test]
fn probe_checks_range_content_and_identity() {
    let (component, payload) = large_download_fixture(2 << 20);
    for name in [
        "range",
        "ignore-range",
        "html",
        "wrong-length",
        "wrong-range",
        "truncated",
        "encoded",
        "forbidden",
    ] {
        let bad_request = Arc::new(AtomicBool::new(false));
        let marker = bad_request.clone();
        let payload = payload.clone();
        let server = start_server(move |request, mut stream| {
            if request.header("range") != Some("bytes=0-1048575")
                || request.header("accept-encoding") != Some("identity")
            {
                marker.store(true, Ordering::SeqCst);
            }
            match name {
                "range" => serve_range(&mut stream, &request, &payload),
                "ignore-range" => respond_payload(&mut stream, &payload),
                "html" => {
                    let mut body = b"<html>".to_vec();
                    body.extend_from_slice(&payload[6..]);
                    send_response(&mut stream, 200, Vec::new(), &body);
                }
                "wrong-length" => {
                    send_response(
                        &mut stream,
                        200,
                        vec![("Content-Length".into(), "16".into())],
                        &payload[..16],
                    );
                }
                "wrong-range" => {
                    send_response(
                        &mut stream,
                        206,
                        vec![
                            (
                                "Content-Range".into(),
                                format!("bytes 1-1048576/{}", payload.len()),
                            ),
                            ("Content-Length".into(), (1 << 20).to_string()),
                        ],
                        &payload[..1 << 20],
                    );
                }
                "truncated" => {
                    send_response(
                        &mut stream,
                        200,
                        vec![("Content-Length".into(), payload.len().to_string())],
                        &payload[..8192],
                    );
                }
                "encoded" => {
                    send_response(
                        &mut stream,
                        200,
                        vec![
                            ("Content-Length".into(), payload.len().to_string()),
                            ("Content-Encoding".into(), "gzip".into()),
                        ],
                        &payload,
                    );
                }
                _ => send_response(&mut stream, 403, Vec::new(), b""),
            }
        });
        let temp = TempDir::new();
        let config = probe_config(temp.path());
        let installer = Installer::new(&config);
        let route = installer.probe(
            &component,
            &format!("{}/asset", server.url()),
            "fixture",
            "",
        );
        assert!(
            !bad_request.load(Ordering::SeqCst),
            "{name}: probe did not use its bounded identity sample"
        );
        let valid = name == "range" || name == "ignore-range";
        if valid {
            assert!(
                route.speed > 0.0 && route.bytes == ROUTE_PROBE_BYTES,
                "{name}: valid sample rejected: {route:?}"
            );
            assert!(route.reason.is_empty());
        } else {
            assert_eq!(route.speed, 0.0, "{name}: invalid sample ranked as usable");
            assert!(!route.reason.is_empty(), "{name}: missing reason");
        }
    }
}

#[test]
fn tiny_probe_requires_whole_payload_checksum() {
    let (component, mut payload) = large_download_fixture(8192);
    payload[20] ^= 1;
    let server = start_server(move |_request, mut stream| {
        respond_payload(&mut stream, &payload);
    });
    let temp = TempDir::new();
    let config = probe_config(temp.path());
    let installer = Installer::new(&config);
    let route = installer.probe(&component, &format!("{}/asset", server.url()), "fixture", "");
    assert_eq!(route.speed, 0.0, "tampered tiny ZIP admitted");
    assert_eq!(route.reason, "checksum mismatch");
}

#[test]
fn routes_measure_beyond_initial_burst() {
    let (component, payload) = large_download_fixture(2 << 20);
    let server = start_server(move |request, mut stream| {
        if request.path.starts_with("/mirror/") {
            serve_range(&mut stream, &request, &payload);
            return;
        }
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        if stream.write_all(&payload[..65536]).is_err() {
            return;
        }
        let _ = stream.flush();
        let mut start = 65536;
        while start < payload.len() {
            std::thread::sleep(Duration::from_millis(30));
            let end = (start + 65536).min(payload.len());
            if stream.write_all(&payload[start..end]).is_err() {
                return;
            }
            let _ = stream.flush();
            start = end;
        }
    });
    let temp = TempDir::new();
    let mut config = probe_config(temp.path());
    config.mirrors = Some(vec![format!("{}/mirror/", server.url())]);
    let installer = Installer::new(&config);
    let routes = installer.routes(&component, &format!("{}/asset", server.url()));
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].prefix, format!("{}/mirror/", server.url()));
}

#[test]
fn small_verified_route_does_not_suppress_large_asset_probe() {
    let (small, small_payload) = part("small", "bin/small", b"small content");
    let (large, large_payload) = large_download_fixture(2 << 20);
    let large_probes = Arc::new(AtomicUsize::new(0));
    let counter = large_probes.clone();
    let server = start_server(move |request, mut stream| {
        if request.path == "/large" {
            if request.header("range").is_some() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            serve_range(&mut stream, &request, &large_payload);
        } else {
            serve_range(&mut stream, &request, &small_payload);
        }
    });
    let temp = TempDir::new();
    let mut config = probe_config(temp.path());
    config.cache_dir = Some(temp.path().to_path_buf());
    let installer = Installer::new(&config);
    installer
        .download(&small, &format!("{}/small", server.url()), &temp.path().join("small"))
        .expect("download small");
    installer
        .download(&large, &format!("{}/large", server.url()), &temp.path().join("large"))
        .expect("download large");
    assert!(
        large_probes.load(Ordering::SeqCst) > 0,
        "a tiny cached success suppressed the large-asset throughput probe"
    );
}

#[test]
fn route_hint_expiry_and_sample_size() {
    let now = Instant::now();
    let hint = RouteHint {
        set: true,
        prefix: String::new(),
        name: String::new(),
        verified_at: Some(now),
        sample_size: ROUTE_PROBE_BYTES,
    };
    let large = Component {
        size: 2 << 20,
        ..large_download_fixture(16).0
    };
    assert!(hint.usable(&large), "recent meaningful sample was discarded");
    let stale = RouteHint {
        verified_at: Some(now - Duration::from_secs(6 * 60)),
        ..hint.clone()
    };
    assert!(!stale.usable(&large), "stale route is still reusable");
    let tiny = RouteHint {
        sample_size: 1024,
        ..hint.clone()
    };
    let small = Component {
        size: 512,
        ..large_download_fixture(16).0
    };
    assert!(!tiny.usable(&large));
    assert!(tiny.usable(&small));
}

#[test]
fn mirror_probe_pool_is_bounded_deduplicated_and_race_free() {
    let (component, payload) = large_download_fixture(8192);
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (calls_handler, active_handler, peak_handler) =
        (calls.clone(), active.clone(), peak.clone());
    let server = start_server(move |request, mut stream| {
        let current = active_handler.fetch_add(1, Ordering::SeqCst) + 1;
        peak_handler.fetch_max(current, Ordering::SeqCst);
        calls_handler.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(10));
        serve_range(&mut stream, &request, &payload);
        active_handler.fetch_sub(1, Ordering::SeqCst);
    });
    let temp = TempDir::new();
    let (log, buffer) = log_buffer();
    let mut config = probe_config(temp.path());
    config.log = log;
    let mut mirrors = Vec::new();
    for index in 0..24 {
        let prefix = format!("{}/m{index}/", server.url());
        mirrors.push(prefix.clone());
        mirrors.push(prefix.clone());
        mirrors.push(prefix.trim_end_matches('/').to_string());
    }
    // The installer captured mirrors at construction; build again with them.
    config.mirrors = Some(mirrors);
    let installer = Installer::new(&config);
    let routes = installer.rank_routes(&component, &format!("{}/asset", server.url()), &HashSet::new());
    assert_eq!(routes.len(), MAX_MIRROR_ROUTES + 1);
    assert_eq!(calls.load(Ordering::SeqCst), MAX_MIRROR_ROUTES + 1);
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        (2..=ROUTE_PROBE_WORKERS).contains(&peak),
        "unexpected worker concurrency {peak}"
    );
    let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_eq!(text.matches("[route]").count(), routes.len());
    drop(installer);
}

#[test]
fn slow_body_switches_before_idle_deadline() {
    let (component, payload) = large_download_fixture(2 << 20);
    let direct_canceled = Arc::new(AtomicBool::new(false));
    let canceled = direct_canceled.clone();
    let server = start_server(move |request, mut stream| {
        if request.path.starts_with("/mirror/") {
            serve_range(&mut stream, &request, &payload);
            return;
        }
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        if stream.write_all(&payload[..65536]).is_err() {
            return;
        }
        let _ = stream.flush();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            if stream.write_all(&payload[..256]).is_err() {
                canceled.store(true, Ordering::SeqCst);
                return;
            }
            let _ = stream.flush();
        }
    });
    let cache = TempDir::new();
    let mirror = format!("{}/mirror/", server.url());
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    config.mirrors = Some(vec![mirror.clone()]);
    config.body_idle_timeout = Some(Duration::from_millis(500));
    config.slow_window = Some(Duration::from_millis(40));
    config.request_timeout = Some(Duration::from_secs(2));
    let installer = Installer::new(&config);
    *installer.route.lock().unwrap() = RouteHint {
        set: true,
        prefix: String::new(),
        name: "GitHub".into(),
        verified_at: None,
        sample_size: 0,
    };
    let target = cache.path().join("verified");
    let started = Instant::now();
    installer
        .download(&component, &format!("{}/asset", server.url()), &target)
        .expect("fallback download");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "trickling cached route was not replaced promptly"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while !direct_canceled.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(direct_canceled.load(Ordering::SeqCst));
    assert_eq!(installer.route.lock().unwrap().prefix, mirror);
    assert!(valid_file(&target, component.size, &component.sha256));
    if let Ok(entries) = fs::read_dir(cache.path()) {
        for entry in entries.flatten() {
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".download-"),
                "failed partial survived"
            );
        }
    }
}

#[test]
fn slow_fallback_does_not_reject_only_progressing_path() {
    let (component, payload) = large_download_fixture(512 << 10);
    let direct_calls = Arc::new(AtomicUsize::new(0));
    let calls = direct_calls.clone();
    let server = start_server(move |request, mut stream| {
        calls.fetch_add(1, Ordering::SeqCst);
        if request.header("range").is_some() {
            // This path is only used by the first attempt's route probe.
        }
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        if stream.write_all(&payload[..1024]).is_err() {
            return;
        }
        let _ = stream.flush();
        std::thread::sleep(Duration::from_millis(40));
        // Go's delayedReader limits exactly the read after the stall to 16
        // bytes; keep that shape so the 20 ms window measures the stall.
        if stream.write_all(&payload[1024..1040]).is_err() {
            return;
        }
        let _ = stream.flush();
        // Keep the post-stall read shaped like Go's delayedReader: the client
        // must observe only 16 bytes inside the 20 ms window.
        std::thread::sleep(Duration::from_millis(50));
        let mut offset = 1040;
        while offset < payload.len() {
            let end = (offset + 16).min(payload.len());
            if stream.write_all(&payload[offset..end]).is_err() {
                return;
            }
            offset = end;
        }
        let _ = stream.flush();
    });
    let cache = TempDir::new();
    let (log, log_buffer) = log_buffer();
    let mut config = probe_config(cache.path());
    config.log = log;
    config.cache_dir = Some(cache.path().to_path_buf());
    config.mirrors = Some(vec!["https://mirror.invalid/".into()]);
    config.slow_window = Some(Duration::from_millis(20));
    config.request_timeout = Some(Duration::from_secs(30));
    let installer = Installer::new(&config);
    *installer.route.lock().unwrap() = RouteHint {
        set: true,
        prefix: String::new(),
        name: String::new(),
        verified_at: None,
        sample_size: 0,
    };
    let target = cache.path().join("verified");
    installer
        .download(&component, &format!("{}/asset", server.url()), &target)
        .expect("slow only path");
    let text = String::from_utf8(log_buffer.lock().unwrap().clone()).unwrap();
    assert_eq!(
        direct_calls.load(Ordering::SeqCst),
        2,
        "expected exactly one bounded soft-floor retry; log: {text}"
    );
    assert!(valid_file(&target, component.size, &component.sha256));
    assert!(!installer.route.lock().unwrap().set);
}

#[test]
fn probe_timeout_retains_measured_slow_candidate() {
    let (component, payload) = large_download_fixture(2 << 20);
    let server = start_server(move |_request, mut stream| {
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        let _ = stream.write_all(&payload[..8192]);
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(2));
    });
    let temp = TempDir::new();
    let mut config = probe_config(temp.path());
    config.probe_timeout = Some(Duration::from_millis(60));
    let installer = Installer::new(&config);
    let route = installer.probe(&component, &server.url(), "slow", "");
    assert!(
        route.speed > 0.0 && route.bytes == 8192 && route.elapsed < Duration::from_secs(1),
        "bounded partial sample mishandled: {route:?}"
    );
}

#[test]
fn bad_mirror_never_publishes_unverified_bytes() {
    let (component, mut payload) = large_download_fixture(2 << 20);
    let last = payload.len() - 1;
    payload[last] ^= 1;
    let server = start_server(move |request, mut stream| {
        serve_range(&mut stream, &request, &payload);
    });
    let cache = TempDir::new();
    let config = probe_config(cache.path());
    let installer = Installer::new(&config);
    let target = cache.path().join("verified");
    assert!(
        installer
            .download(&component, &server.url(), &target)
            .is_err(),
        "fast probe bypassed the full payload hash"
    );
    assert_absent(&target);
}

#[test]
fn probe_does_not_leak_workers_after_timeout() {
    let (component, _) = large_download_fixture(2 << 20);
    let server = start_server(move |_request, _stream| {
        std::thread::sleep(Duration::from_millis(400));
    });
    let temp = TempDir::new();
    let mut config = probe_config(temp.path());
    config.probe_timeout = Some(Duration::from_millis(20));
    let mut mirrors = Vec::new();
    for index in 0..8 {
        mirrors.push(format!("{}/m{index}/", server.url()));
    }
    config.mirrors = Some(mirrors);
    let installer = Installer::new(&config);
    let routes = installer.rank_routes(&component, "https://github.com/asset", &HashSet::new());
    assert_eq!(routes.len(), 9, "fallback routes were lost");
    for route in routes {
        assert_eq!(route.speed, 0.0);
        assert!(route.retryable && !route.reason.is_empty());
    }
    // A working installer must still make progress afterwards.
    let (working, payload) = large_download_fixture(8192);
    let good = start_server(move |_request, mut stream| {
        respond_payload(&mut stream, &payload);
    });
    let route = installer.probe(&working, &good.url(), "fixture", "");
    assert!(route.speed > 0.0, "probe workers were leaked");
}

#[test]
fn small_slow_probe_is_not_mistaken_for_checksum_failure() {
    let (component, payload) = large_download_fixture(512 << 10);
    let server = start_server(move |_request, mut stream| {
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        let _ = stream.write_all(&payload[..8192]);
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(2));
    });
    let temp = TempDir::new();
    let mut config = probe_config(temp.path());
    config.probe_timeout = Some(Duration::from_millis(60));
    let installer = Installer::new(&config);
    let route = installer.probe(&component, &server.url(), "slow-small", "");
    assert!(
        route.speed > 0.0 && route.bytes == 8192 && route.reason.is_empty(),
        "partial small sample incorrectly excluded: {route:?}"
    );
}

#[test]
fn probe_timeout_still_allows_verified_download() {
    let (component, payload) = large_download_fixture(512 << 10);
    let gets = Arc::new(AtomicUsize::new(0));
    let counter = gets.clone();
    let server = start_server(move |request, mut stream| {
        if request.header("range").is_some() {
            std::thread::sleep(Duration::from_millis(100));
            return;
        }
        counter.fetch_add(1, Ordering::SeqCst);
        respond_payload(&mut stream, &payload);
    });
    let cache = TempDir::new();
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    config.mirrors = Some(vec!["https://mirror.invalid/".into()]);
    config.probe_timeout = Some(Duration::from_millis(20));
    let installer = Installer::new(&config);
    let target = cache.path().join("verified");
    installer
        .download(&component, &server.url(), &target)
        .expect("inconclusive probe must not block a valid transfer");
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    assert!(valid_file(&target, component.size, &component.sha256));
}

// ---------------------------------------------------------------------------
// Go: storage_test.go
// ---------------------------------------------------------------------------

fn promotion_fixture() -> (TempDir, PathBuf, Vec<PayloadFile>, Vec<String>) {
    let root = TempDir::new();
    let stage = prepare_workspace(root.path()).expect("prepare promotion workspace");
    for (name, value) in [
        ("bin/a", "old-a"),
        ("components.installed.json", "old manifest"),
        ("webroot/obsolete/old.js", "old webui"),
        (".config/user.json", "private fixture"),
    ] {
        write_file(&root.path().join(name), value.as_bytes());
    }
    let files = vec![
        PayloadFile {
            path: "bin/a".into(),
            sha256: String::new(),
            size: 0,
            mode: 0o644,
        },
        PayloadFile {
            path: "bin/b".into(),
            sha256: String::new(),
            size: 0,
            mode: 0o644,
        },
        PayloadFile {
            path: "components.installed.json".into(),
            sha256: String::new(),
            size: 0,
            mode: 0o600,
        },
    ];
    for file in &files {
        write_file(
            &stage.join(&file.path),
            format!("new {}", file.path).as_bytes(),
        );
    }
    (root, stage, files, vec!["webroot/obsolete/old.js".into()])
}

fn promotion_moves(
    journal: &PromotionJournal,
    root: &Path,
    stage: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut moves = Vec::new();
    for change in &journal.changes {
        let dst = root.join(&change.path);
        if change.existed {
            moves.push((dst.clone(), stage.join(".rollback").join(&change.path)));
        }
        if change.install {
            moves.push((stage.join(&change.path), dst));
        }
    }
    moves
}

fn assert_original_promotion_files(root: &Path) {
    assert_content(&root.join("bin/a"), b"old-a");
    assert_content(&root.join("components.installed.json"), b"old manifest");
    assert_content(&root.join("webroot/obsolete/old.js"), b"old webui");
    assert_content(&root.join(".config/user.json"), b"private fixture");
    assert_absent(&root.join("bin/b"));
}

#[test]
fn repeated_online_upgrades_keep_only_current_files() {
    let mut archives = Vec::new();
    let mut payloads: HashMap<String, Vec<u8>> = HashMap::new();
    for version in 0..3 {
        let (component, blob) = part(
            "webui",
            &format!("webroot/version-{version}/index.js"),
            format!("webui-{version}").as_bytes(),
        );
        let mut manifest = manifest_fixture(vec![component.clone()]);
        manifest.version = format!("v1.4.{}", 8 + version);
        let root = TempDir::new();
        let archive = archive_fixture(root.path(), &manifest, &[]);
        archives.push((root, archive));
        payloads.insert(format!("/{}", component.asset), blob);
    }
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let payloads = Arc::new(payloads);
    let server = start_server(move |request, mut stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        match payloads.get(&request.path) {
            Some(blob) => respond_payload(&mut stream, blob),
            None => send_response(&mut stream, 404, Vec::new(), b""),
        }
    });
    let first_archive = archives[0].1.clone();
    let fixture = options_fixture(&first_archive);
    let fixture_root = fixture.root;
    let mut config = fixture.config;
    config.cache_dir = None;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    write_file(&config.module_dir.join(".config/subscription.url"), b"private fixture");
    let parent = config.module_dir.parent().unwrap().to_path_buf();
    write_file(&parent.join("unrelated.txt"), b"keep");
    for (version, (_archive_root, archive)) in archives.iter().enumerate() {
        config.archive = archive.clone();
        Installer::new(&config).run().expect("online upgrade");
        assert_content(
            &config
                .module_dir
                .join(format!("webroot/version-{version}/index.js")),
            format!("webui-{version}").as_bytes(),
        );
        let versions = fs::read_dir(config.module_dir.join("webroot"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("version-"))
            .count();
        assert_eq!(versions, 1, "historical component directories remain");
        assert_no_workspace(&config.module_dir);
        let before = requests.load(Ordering::SeqCst);
        Installer::new(&config).run().expect("unchanged reinstall");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            before,
            "unchanged reinstallation accessed the network"
        );
        assert_no_workspace(&config.module_dir);
    }
    assert_content(
        &config.module_dir.join(".config/subscription.url"),
        b"private fixture",
    );
    // The parent must survive the installation itself; checking it after the
    // fixture TempDir is dropped is impossible (drop deletes the tree), so the
    // assertion runs before cleanup.
    assert_content(&parent.join("unrelated.txt"), b"keep");
    drop(fixture_root);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        6,
        "want one probe and one transfer per changed component"
    );
}

#[test]
fn explicit_archive_source_is_never_created_or_populated() {
    let (component, payload) = part("engine", "bin/sing-box", b"good");
    let server = start_server(move |_request, mut stream| {
        respond_payload(&mut stream, &payload);
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    install(&fixture).expect("network install");
    assert_absent(fixture.config.cache_dir.as_ref().unwrap());
    assert_no_workspace(&fixture.config.module_dir);
}

#[test]
fn failed_install_cleans_workspace_and_preserves_unrelated_files() {
    let (component, _) = part("engine", "bin/sing-box", b"new");
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.cache_dir = None;
    write_file(&config.module_dir.join("bin/sing-box"), b"old");
    write_file(&config.module_dir.join(".config/user.json"), b"keep");
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    for _ in 0..3 {
        assert!(install(&fixture).is_err(), "missing offline payload accepted");
        assert_no_workspace(&fixture.config.module_dir);
        assert_content(&fixture.config.module_dir.join("bin/sing-box"), b"old");
        assert_content(&fixture.config.module_dir.join(".config/user.json"), b"keep");
    }
}

#[test]
fn module_lock_cannot_be_bypassed_with_another_archive_source() {
    let (component, _) = part("engine", "bin/sing-box", b"new");
    let temp = TempDir::new();
    let archive = archive_fixture(
        temp.path(),
        &manifest_fixture(vec![component]),
        &[("bin/sing-box", b"new")],
    );
    let fixture = options_fixture(&archive);
    let lock = lock_module(&fixture.config.module_dir).expect("module lock");
    let stage = prepare_workspace(&fixture.config.module_dir).expect("workspace");
    write_file(&stage.join(".downloads/active"), b"active transfer");
    let mut config = fixture.config;
    config.cache_dir = None;
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    let error = install(&fixture).expect_err("concurrent installation was not rejected");
    assert!(
        error.message().contains("another component installation"),
        "unexpected error: {}",
        error.message()
    );
    assert_content(&stage.join(".downloads/active"), b"active transfer");
    drop(lock);
    install(&fixture).expect("install after lock release");
    assert_no_workspace(&fixture.config.module_dir);
}

#[test]
fn unknown_workspace_and_symlinks_are_left_untouched() {
    for kind in ["unknown", "symlink", "wrong-marker"] {
        let (component, _) = part("engine", "bin/sing-box", b"new");
        let temp = TempDir::new();
        let archive = archive_fixture(
            temp.path(),
            &manifest_fixture(vec![component]),
            &[("bin/sing-box", b"new")],
        );
        let fixture = options_fixture(&archive);
        let stage = fixture.config.module_dir.join(WORKSPACE_NAME);
        let protected;
        if kind == "symlink" {
            let outside = TempDir::new();
            protected = outside.path().join("keep");
            write_file(&protected, b"keep");
            fs::create_dir_all(&fixture.config.module_dir).unwrap();
            std::os::unix::fs::symlink(outside.path(), &stage).unwrap();
            // Keep the temp dir alive until the assertions finish.
            let result = install(&fixture);
            assert!(result.is_err(), "symlinked workspace was adopted");
            assert_content(&protected, b"keep");
            drop(outside);
            continue;
        } else {
            protected = stage.join("keep");
            write_file(&protected, b"keep");
            if kind == "wrong-marker" {
                write_file(&stage.join(WORKSPACE_OWNER), b"not an empty owned marker");
            }
        }
        assert!(install(&fixture).is_err(), "unrecognized workspace was adopted");
        assert_content(&protected, b"keep");
    }
}

#[test]
fn interrupted_promotion_recovers_at_every_rename_boundary() {
    for after in 0..=6 {
        let (root, stage, files, obsolete) = promotion_fixture();
        let journal =
            prepare_promotion(&stage, root.path(), &files, &obsolete).expect("journal");
        for (from, to) in promotion_moves(&journal, root.path(), &stage)
            .into_iter()
            .take(after)
        {
            fs::rename(from, to).expect("simulate interrupted rename");
        }
        let recovered = prepare_workspace(root.path()).expect("recovery");
        assert_original_promotion_files(root.path());
        assert_eq!(recovered, stage, "recovery created another history directory");
        cleanup_workspace(&recovered).expect("cleanup");
        assert_no_workspace(root.path());
    }
}

#[test]
fn interrupted_cleanup_keeps_committed_installation() {
    let (root, stage, files, obsolete) = promotion_fixture();
    promote(&stage, root.path(), &files, &obsolete).expect("promote");
    fs::remove_file(stage.join(".committed")).expect("simulate killed cleanup");
    prepare_workspace(root.path()).expect("recover committed install");
    for file in &files {
        assert_content(
            &root.path().join(&file.path),
            format!("new {}", file.path).as_bytes(),
        );
    }
    assert_absent(&root.path().join(&obsolete[0]));
    assert_content(&root.path().join(".config/user.json"), b"private fixture");
}

#[test]
fn failed_recovery_retains_one_backup_and_can_resume() {
    let (root, stage, files, obsolete) = promotion_fixture();
    let journal = prepare_promotion(&stage, root.path(), &files, &obsolete).expect("journal");
    for (from, to) in promotion_moves(&journal, root.path(), &stage) {
        fs::rename(from, to).expect("simulate interrupted rename");
    }
    let blocked = root.path().join("bin/a");
    fs::remove_file(&blocked).unwrap();
    fs::create_dir(&blocked).unwrap();
    for _ in 0..3 {
        assert!(
            prepare_workspace(root.path()).is_err(),
            "blocked recovery was hidden"
        );
        assert_content(&stage.join(".rollback/bin/a"), b"old-a");
    }
    fs::remove_dir(&blocked).unwrap();
    prepare_workspace(root.path()).expect("resume recovery");
    assert_original_promotion_files(root.path());
}

#[test]
fn interrupted_download_worker() {
    if std::env::var("MAGICNET_INSTALLER_TEST_WORKER").as_deref() != Ok("1") {
        return;
    }
    let config = Config {
        archive: std::env::var("MAGICNET_INSTALLER_TEST_ARCHIVE")
            .expect("worker archive")
            .into(),
        module_dir: std::env::var("MAGICNET_INSTALLER_TEST_MODULE")
            .expect("worker module")
            .into(),
        previous_dir: PathBuf::new(),
        cache_dir: None,
        offline: false,
        mirrors: Some(Vec::new()),
        base_url: Some(std::env::var("MAGICNET_INSTALLER_TEST_ENDPOINT").expect("worker endpoint")),
        log: Log::Discard,
        body_idle_timeout: Some(Duration::from_secs(10)),
        probe_timeout: Some(Duration::from_millis(100)),
        slow_window: None,
        request_timeout: Some(Duration::from_secs(30)),
        allow_http: true,
        curl: None,
    };
    if Installer::new(&config).run().is_err() {
        std::process::exit(93);
    }
}

#[test]
fn killed_download_releases_lock_and_reclaims_partial_on_retry() {
    let content = b"engine".repeat(1000);
    let (component, blob) = part("engine", "bin/sing-box", &content);
    let declared = component.size.to_string();
    let half = blob.len() / 2;
    let blob = Arc::new(blob);
    let held = blob.clone();
    let server = start_server(move |_request, mut stream| {
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), declared.clone())],
        );
        let _ = stream.write_all(&held[..half]);
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(5));
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component.clone()]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    config.probe_timeout = Some(Duration::from_millis(100));
    config.body_idle_timeout = Some(Duration::from_secs(10));
    config.request_timeout = Some(Duration::from_secs(30));
    let module_dir = config.module_dir.clone();
    let archive_path = archive.clone();
    let endpoint = format!("{}/", server.url());
    let exec = std::env::current_exe().expect("test binary");
    let mut child = std::process::Command::new(exec)
        .args([
            "--exact",
            "installer::tests::interrupted_download_worker",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MAGICNET_INSTALLER_TEST_WORKER", "1")
        .env("MAGICNET_INSTALLER_TEST_ARCHIVE", &archive_path)
        .env("MAGICNET_INSTALLER_TEST_MODULE", &module_dir)
        .env("MAGICNET_INSTALLER_TEST_ENDPOINT", &endpoint)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn interrupted download worker");
    let deadline = Instant::now() + Duration::from_secs(10);
    let partial = module_dir.join(WORKSPACE_NAME).join(".downloads");
    loop {
        let started = fs::read_dir(&partial).ok().is_some_and(|entries| {
            entries.flatten().any(|entry| {
                entry.file_name().to_string_lossy().starts_with(".download-")
                    && entry.metadata().is_ok_and(|meta| meta.len() > 0)
            })
        });
        if started {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("worker never started a partial component download");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("kill download worker");
    assert!(
        child.wait().expect("wait").code() != Some(0),
        "download worker was not interrupted"
    );
    let bundled = TempDir::new();
    let bundled_archive =
        archive_fixture(bundled.path(), &manifest_fixture(vec![component]), &[("bin/sing-box", &content)]);
    config.archive = bundled_archive;
    config.cache_dir = None;
    config.offline = true;
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    install(&fixture).expect("retry after killed worker");
    assert_no_workspace(&fixture.config.module_dir);
    assert_content(&fixture.config.module_dir.join("bin/sing-box"), &content);
}

// ---------------------------------------------------------------------------
// Go: parallel_test.go, download_progress_test.go, ux_test.go
// ---------------------------------------------------------------------------

fn parallel_fixture(id: &str, size: usize) -> (Component, Vec<u8>) {
    let mut payload = vec![0u8; size];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = id.as_bytes()[index % id.len()];
    }
    payload[..4].copy_from_slice(b"PK\x03\x04");
    (
        Component {
            id: id.into(),
            sha256: sha256_hex(&payload),
            size: size as i64,
            asset: format!("{id}.zip"),
            files: Vec::new(),
        },
        payload,
    )
}

#[test]
fn component_downloads_use_bounded_parallelism() {
    const COUNT: usize = 6;
    let mut components = Vec::new();
    let mut payloads = HashMap::new();
    for index in 0..COUNT {
        let id = format!("part-{index}");
        let (component, payload) = parallel_fixture(&id, 64 << 10);
        payloads.insert(format!("/{id}"), payload);
        components.push(component);
    }
    let payloads = Arc::new(payloads);
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (calls_handler, active_handler, peak_handler) =
        (calls.clone(), active.clone(), peak.clone());
    let server = start_server(move |request, mut stream| {
        let current = active_handler.fetch_add(1, Ordering::SeqCst) + 1;
        peak_handler.fetch_max(current, Ordering::SeqCst);
        calls_handler.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        match payloads.get(&request.path) {
            Some(payload) => respond_payload(&mut stream, payload),
            None => send_response(&mut stream, 404, Vec::new(), b""),
        }
        active_handler.fetch_sub(1, Ordering::SeqCst);
    });
    let cache = TempDir::new();
    let (log, buffer) = log_buffer();
    let mut config = probe_config(cache.path());
    config.log = log;
    config.cache_dir = Some(cache.path().to_path_buf());
    let installer = Installer::new(&config);
    *installer.route.lock().unwrap() = RouteHint {
        set: true,
        prefix: String::new(),
        name: "GitHub".into(),
        verified_at: None,
        sample_size: ROUTE_PROBE_BYTES,
    };
    let jobs: Vec<DownloadJob> = components
        .iter()
        .map(|component| DownloadJob {
            component: component.clone(),
            direct: format!("{}/{}", server.url(), component.id),
            destination: cache.path().join(format!("{}.zip", component.sha256)),
        })
        .collect();
    installer.download_components(jobs).expect("parallel downloads");
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        (2..=COMPONENT_DOWNLOAD_WORKERS).contains(&peak),
        "unexpected component download concurrency: {peak}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), COUNT);
    for component in &components {
        assert!(valid_file(
            &cache.path().join(format!("{}.zip", component.sha256)),
            component.size,
            &component.sha256
        ));
    }
    for entry in fs::read_dir(cache.path()).unwrap().flatten() {
        assert!(
            !entry.file_name().to_string_lossy().starts_with(".download-"),
            "partial downloads survived"
        );
    }
    assert!(!buffer.lock().unwrap().is_empty(), "parallel logging disappeared");
}

#[test]
fn component_download_failure_stops_queue_and_cleans_partials() {
    let (good, payload) = parallel_fixture("good", 32 << 10);
    let mut bad = good.clone();
    bad.id = "bad".into();
    bad.sha256 = sha256_hex(b"different");
    let payload = Arc::new(payload);
    let server = start_server(move |_request, mut stream| {
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), payload.len().to_string())],
        );
        let _ = stream.write_all(&payload);
    });
    let cache = TempDir::new();
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    let installer = Installer::new(&config);
    *installer.route.lock().unwrap() = RouteHint {
        set: true,
        prefix: String::new(),
        name: "GitHub".into(),
        verified_at: None,
        sample_size: ROUTE_PROBE_BYTES,
    };
    let jobs = vec![
        DownloadJob {
            component: bad,
            direct: format!("{}/bad", server.url()),
            destination: cache.path().join("bad.zip"),
        },
        DownloadJob {
            component: good,
            direct: format!("{}/good", server.url()),
            destination: cache.path().join("good.zip"),
        },
    ];
    assert!(installer.download_components(jobs).is_err(), "checksum failure was hidden");
    for entry in fs::read_dir(cache.path()).unwrap().flatten() {
        assert!(
            !entry.file_name().to_string_lossy().starts_with(".download-"),
            "failed parallel download left partials"
        );
    }
}

#[test]
fn stalled_cached_route_falls_back_and_cleans_partial() {
    let (component, payload) = part("engine", "bin/sing-box", b"verified content");
    let mirror_payload = payload.clone();
    let server = start_server(move |request, mut stream| {
        if request.path.starts_with("/mirror/") {
            respond_payload(&mut stream, &mirror_payload);
            return;
        }
        // Headers alone must not keep a stalled response alive.
        send_head(&mut stream, 200, &[]);
        std::thread::sleep(Duration::from_secs(2));
    });
    let cache = TempDir::new();
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    config.mirrors = Some(vec![format!("{}/mirror/", server.url())]);
    config.body_idle_timeout = Some(Duration::from_millis(80));
    config.request_timeout = Some(Duration::from_secs(3));
    let installer = Installer::new(&config);
    *installer.route.lock().unwrap() = RouteHint {
        set: true,
        prefix: String::new(),
        name: "direct".into(),
        verified_at: None,
        sample_size: 0,
    };
    let target = cache.path().join("verified.zip");
    installer
        .download(&component, &format!("{}/component", server.url()), &target)
        .expect("fallback download");
    assert_content(&target, &payload);
    assert_eq!(
        installer.route.lock().unwrap().prefix,
        format!("{}/mirror/", server.url())
    );
    for entry in fs::read_dir(cache.path()).unwrap().flatten() {
        assert!(
            !entry.file_name().to_string_lossy().starts_with(".download-"),
            "leaked partial downloads"
        );
    }
}

#[test]
fn progress_resets_body_idle_deadline() {
    let payload = b"abcdefgh".repeat(12);
    let component = Component {
        id: "test".into(),
        size: payload.len() as i64,
        sha256: sha256_hex(&payload),
        asset: String::new(),
        files: Vec::new(),
    };
    let payload = Arc::new(payload);
    let body = payload.clone();
    let server = start_server(move |_request, mut stream| {
        send_head(
            &mut stream,
            200,
            &[("Content-Length".into(), body.len().to_string())],
        );
        for chunk in body.chunks(16) {
            if stream.write_all(chunk).is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    let cache = TempDir::new();
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    config.body_idle_timeout = Some(Duration::from_millis(120));
    let installer = Installer::new(&config);
    installer
        .download_attempt(&component, &server.url(), &cache.path().join("good"))
        .expect("progress reset the idle deadline");
    assert!(valid_file(
        &cache.path().join("good"),
        component.size,
        &component.sha256
    ));
}

#[test]
fn idle_deadline_never_publishes_truncated_body() {
    let server = start_server(move |_request, mut stream| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\npart");
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(2));
    });
    let cache = TempDir::new();
    let mut config = probe_config(cache.path());
    config.cache_dir = Some(cache.path().to_path_buf());
    config.body_idle_timeout = Some(Duration::from_millis(50));
    let installer = Installer::new(&config);
    let component = Component {
        id: "test".into(),
        size: 100,
        sha256: "0".repeat(64),
        asset: String::new(),
        files: Vec::new(),
    };
    let target = cache.path().join("bad");
    assert!(
        installer
            .download_attempt(&component, &server.url(), &target)
            .is_err(),
        "stalled partial passed verification"
    );
    assert_absent(&target);
    assert_eq!(
        fs::read_dir(cache.path()).unwrap().count(),
        0,
        "failed attempt leaked staging files"
    );
}

#[test]
fn download_progress_is_human_readable() {
    let (log, buffer) = log_buffer();
    let mut progress = super::log::Progress::new(2048, &log, "engine");
    progress.write(2048);
    let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    for want in ["100%", "2.0 KiB/2.0 KiB", "KiB/s", "ETA 0s"] {
        assert!(text.contains(want), "progress output {text:?} missing {want}");
    }
}

#[test]
fn download_route_is_reused_across_changed_components() {
    let (a, payload_a) = part("a", "bin/a", b"new-a");
    let (b, payload_b) = part("b", "bin/b", b"new-b");
    let mut payloads = HashMap::new();
    payloads.insert(format!("/{}", a.asset), payload_a);
    payloads.insert(format!("/{}", b.asset), payload_b);
    let payloads = Arc::new(payloads);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = start_server(move |request, mut stream| {
        counter.fetch_add(1, Ordering::SeqCst);
        match payloads.get(&request.path) {
            Some(payload) => respond_payload(&mut stream, payload),
            None => send_response(&mut stream, 404, Vec::new(), b""),
        }
    });
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![a, b]), &[]);
    let fixture = options_fixture(&archive);
    let mut config = fixture.config;
    config.offline = false;
    config.base_url = Some(format!("{}/", server.url()));
    config.mirrors = Some(Vec::new());
    let fixture = Fixture {
        root: fixture.root,
        config,
        log: fixture.log,
    };
    install(&fixture).expect("two component install");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected one route probe and two downloads"
    );
}

// __APPEND__




// ---------------------------------------------------------------------------
// Legacy cleanup regressions (Go: legacy_cleanup_test.go)
// ---------------------------------------------------------------------------

fn legacy_layout(root: &Path) -> (PathBuf, PathBuf) {
    let adb = root.join("data/adb");
    let module = adb.join("modules_update/MagicNet");
    write_file(&module.join("module.prop"), b"id=MagicNet\nversion=v1.6.3\n");
    (adb, module)
}

fn seed_legacy_cache(adb: &Path) -> PathBuf {
    let cache = adb.join("magicnet-components");
    write_file(&cache.join("install.lock"), b"");
    for version in ["a", "b", "c", "d"] {
        let name = format!("{}.zip", version.repeat(64));
        write_file(&cache.join(name), b"old component");
    }
    write_file(&cache.join(".download-12345"), b"interrupted transfer");
    cache
}

fn assert_legacy_present(path: &Path, present: bool) {
    assert_eq!(
        fs::symlink_metadata(path).is_ok(),
        present,
        "path {} presence",
        path.display()
    );
}

fn set_directory_age_past(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(path.as_os_str().as_bytes()).expect("cstring path");
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .saturating_sub(3600) as libc::time_t;
    let times = [libc::timespec { tv_sec: past, tv_nsec: 0 }; 2];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), 0) },
        0,
        "utimensat {}",
        path.display()
    );
}

fn replace_root_with_symlink(path: &Path) -> PathBuf {
    let text = path.to_str().expect("ascii test path").to_string();
    let target = PathBuf::from(text + "-target");
    fs::rename(path, &target).expect("rename root");
    std::os::unix::fs::symlink(&target, path).expect("symlink root");
    target
}

#[test]
fn legacy_cleanup_removes_old_versions_without_touching_current_state() {
    let root = TempDir::new();
    let (adb, module) = legacy_layout(root.path());
    let cache = seed_legacy_cache(&adb);
    let log = adb.join("cache/MagicNet");
    write_file(&log.join("kam.log"), b"old log");
    write_file(&log.join("kam.log.1"), b"older log");
    for parent in ["modules", "modules_update"] {
        let backup = adb.join(parent).join("MagicNet.install-backup.2147483647");
        write_file(
            &backup.join(".magicnet-install-backup-v1"),
            b"magicnet-install-backup-v1\n",
        );
        let readonly = backup.join("readonly");
        write_file(&readonly.join("old-secret"), b"old backup");
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500))
            .expect("chmod readonly");
    }
    let protected: Vec<PathBuf> = vec![
        module.join(".config/sing-box/subscription.url"),
        module.join(".config/sing-box/config.json.pre-upgrade"),
        module.join(".state/network-access-recovery/record.json"),
        adb.join("modules/AnotherVPN/keep"),
        adb.join("modules/.magicnet-components-123/.rollback/bin/sing-box"),
    ];
    for name in &protected {
        write_file(name, b"preserve these bytes");
    }
    for attempt in 0..3 {
        cleanup_legacy_storage(&module)
            .unwrap_or_else(|error| panic!("legacy cleanup attempt {}: {}", attempt, error.message()));
    }
    assert_legacy_present(&cache, false);
    assert_legacy_present(&log, false);
    for parent in ["modules", "modules_update"] {
        assert_legacy_present(
            &adb.join(parent).join("MagicNet.install-backup.2147483647"),
            false,
        );
    }
    for name in &protected {
        assert_content(name, b"preserve these bytes");
    }
}

#[test]
fn legacy_cleanup_respects_cache_lock() {
    let root = TempDir::new();
    let (adb, module) = legacy_layout(root.path());
    let cache = seed_legacy_cache(&adb);
    let lock = open_no_follow(&cache.join("install.lock")).expect("legacy lock file");
    let raw = std::os::fd::AsRawFd::as_raw_fd(&lock);
    assert_eq!(unsafe { libc::flock(raw, libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let error = cleanup_legacy_storage(&module)
        .expect_err("busy legacy cache was reported as cleaned");
    assert!(
        error.message().contains("legacy component cache is busy"),
        "unexpected error: {}",
        error.message()
    );
    assert_legacy_present(&cache.join(format!("{}.zip", "a".repeat(64))), true);
    assert_eq!(unsafe { libc::flock(raw, libc::LOCK_UN) }, 0);
    drop(lock);
    cleanup_legacy_storage(&module).expect("cleanup after lock release");
    assert_legacy_present(&cache, false);
}

#[test]
fn legacy_cleanup_preserves_unknown_and_linked_files() {
    let root = TempDir::new();
    let (adb, module) = legacy_layout(root.path());
    let cache = seed_legacy_cache(&adb);
    let outside = root.join("data/outside");
    write_file(&outside, b"keep outside");
    let linked = cache.join(format!("{}.zip", "e".repeat(64)));
    std::os::unix::fs::symlink(&outside, &linked).expect("symlink archive");
    let unknown = cache.join("personal.zip");
    write_file(&unknown, b"keep unknown");
    let log = adb.join("cache/MagicNet");
    write_file(&log.join("keep"), b"unrelated log");
    std::os::unix::fs::symlink(&outside, log.join("kam.log")).expect("symlink log");
    cleanup_legacy_storage(&module).expect("legacy cleanup");
    for path in [
        linked.clone(),
        outside.clone(),
        unknown.clone(),
        cache.join("install.lock"),
        log.join("keep"),
        log.join("kam.log"),
    ] {
        assert_legacy_present(&path, true);
    }
    assert_legacy_present(&cache.join(format!("{}.zip", "a".repeat(64))), false);
}

#[test]
fn legacy_cleanup_rejects_symlink_roots_and_wrong_identity() {
    for mode in [
        "wrong-id",
        "duplicate-id",
        "module-link",
        "cache-link",
        "lock-link",
        "log-parent-link",
    ] {
        let root = TempDir::new();
        let (adb, module) = legacy_layout(root.path());
        let cache = seed_legacy_cache(&adb);
        let mut watched = cache.join(format!("{}.zip", "a".repeat(64)));
        let want_error = match mode {
            "wrong-id" | "duplicate-id" => {
                let props = if mode == "wrong-id" {
                    b"id=AnotherVPN\n".to_vec()
                } else {
                    b"id=MagicNet\nid=MagicNet\n".to_vec()
                };
                write_file(&module.join("module.prop"), &props);
                true
            }
            "module-link" => {
                replace_root_with_symlink(&module);
                true
            }
            "cache-link" => {
                let target = replace_root_with_symlink(&cache);
                watched = target.join(format!("{}.zip", "a".repeat(64)));
                false
            }
            "lock-link" => {
                let lock = cache.join("install.lock");
                fs::remove_file(&lock).expect("remove lock");
                std::os::unix::fs::symlink(&watched, &lock).expect("lock symlink");
                false
            }
            "log-parent-link" => {
                let logs_root = adb.join("cache");
                write_file(&logs_root.join("MagicNet/kam.log"), b"keep");
                let target = replace_root_with_symlink(&logs_root);
                watched = target.join("MagicNet/kam.log");
                false
            }
            _ => unreachable!(),
        };
        let result = cleanup_legacy_storage(&module);
        assert_eq!(
            result.is_err(),
            want_error,
            "mode {mode}: unexpected cleanup result: {:?}",
            result.map(|()| "ok").map_err(|error| error.message())
        );
        assert_legacy_present(&watched, true);
    }
}

#[test]
fn legacy_cleanup_requires_exact_backup_marker_and_dead_owner() {
    let root = TempDir::new();
    let (adb, module) = legacy_layout(root.path());
    let parents = adb.join("modules");
    let markers: [&[u8]; 4] = [
        b"",
        b"magicnet-install-backup-v1",
        b"magicnet-install-backup-v1\nextra",
        b"magicnet-install-backup-v1\n\n",
    ];
    for (index, marker) in markers.iter().enumerate() {
        let pid = 2_147_483_646u32 - index as u32;
        let dir = parents.join(format!("MagicNet.install-backup.{pid}"));
        write_file(&dir.join(".magicnet-install-backup-v1"), marker);
    }
    let active = parents.join(format!("MagicNet.install-backup.{}", std::process::id()));
    write_file(
        &active.join(".magicnet-install-backup-v1"),
        b"magicnet-install-backup-v1\n",
    );
    cleanup_legacy_storage(&module).expect("legacy cleanup");
    let entries: Vec<fs::DirEntry> = fs::read_dir(&parents).expect("read modules").flatten().collect();
    assert_eq!(entries.len(), 5, "unknown or active backup was changed");
}

fn seed_legacy_farewell(adb: &Path, suffix: &str) -> PathBuf {
    let dir = adb.join(format!(".magicnet-farewell.{suffix}"));
    let worker = concat!(
        "#!/system/bin/sh\n",
        "MN_FAREWELL_RUN=",
        "$",
        "{0%/*}\n",
        ". \"$MN_FAREWELL_RUN/uninstall.sh\" || exit 1\n",
        "magicnet_farewell_worker \"$MN_FAREWELL_RUN\"\n",
    );
    write_file(&dir.join("worker.sh"), worker.as_bytes());
    write_file(&dir.join("httpd.conf"), b"A:127.0.0.1\nD:*\n");
    let token = "a".repeat(32);
    write_file(&dir.join("token"), format!("{token}\n").as_bytes());
    write_file(
        &dir.join(format!("www/{token}/index.html")),
        b"<main id=\"farewell\">Goodbye</main>",
    );
    dir
}

#[test]
fn legacy_cleanup_only_removes_idle_recognized_farewell() {
    let root = TempDir::new();
    let (adb, module) = legacy_layout(root.path());
    let stale = seed_legacy_farewell(&adb, "STALE1");
    let recent = seed_legacy_farewell(&adb, "RECENT");
    let unknown = seed_legacy_farewell(&adb, "EXTRA1");
    write_file(&unknown.join("keep"), b"user file");
    for dir in [&stale, &unknown] {
        set_directory_age_past(dir);
    }
    cleanup_legacy_storage(&module).expect("legacy cleanup");
    assert_legacy_present(&stale, false);
    assert_legacy_present(&recent, true);
    assert_legacy_present(&unknown, true);
}

// ---------------------------------------------------------------------------
// Network resolution boundary (Go: TestAndroidBootstrapDoesNotDependOnLibcDNS)
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_resolves_nothing_in_process() {
    // The Go bootstrap installed a pure-Go DNS resolver on Android so the
    // download path never needed libc name resolution. The Rust port is
    // stricter: name resolution never happens in this process at all - every
    // transfer is delegated to an explicitly argv'd curl child whose
    // environment is cleared. This test pins that boundary.
    let (component, _) = part("engine", "bin/sing-box", b"payload");
    let temp = TempDir::new();
    let archive = archive_fixture(temp.path(), &manifest_fixture(vec![component]), &[]);
    let fixture = options_fixture(&archive);
    let installer = Installer::new(&fixture.config);
    let command = installer.curl_command(
        "https://example.invalid/asset.zip",
        None,
        Duration::from_secs(10),
        Duration::from_secs(5),
    );
    let program = command.get_program().to_string_lossy().to_string();
    assert!(
        program == "/system/bin/curl" || program == "/usr/bin/curl",
        "unexpected curl program: {program}"
    );
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    assert!(args.iter().any(|arg| arg == "-q"), "curlrc must not be read");
    assert!(args.windows(2).any(|pair| pair[0] == "--noproxy" && pair[1] == "*"));
    assert!(args.iter().any(|arg| arg == "--connect-timeout"));
    assert!(args.iter().any(|arg| arg == "--max-time"));
    let envs: Vec<_> = command.get_envs().collect();
    assert_eq!(envs.len(), 2, "only PATH and LC_ALL survive env_clear");
}