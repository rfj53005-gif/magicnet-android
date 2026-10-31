//! Mirror probing, route ranking, and verified component downloads.
//!
//! Ported from `installer/components/mirrors.go` and `main.go:377-627`.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::constants::{
    BODY_IDLE_TIMEOUT, COMPONENT_DOWNLOAD_WORKERS, CONNECT_TIMEOUT, DEFAULT_MIRRORS,
    FAST_DIRECT_BYTES, FAST_DIRECT_SPEED, MAX_MIRROR_ROUTES, MAX_REDIRECTS, PROBE_COPY_BUFFER,
    PROBE_TIMEOUT, REQUEST_TIMEOUT, RESPONSE_HEADER_TIMEOUT, ROUTE_HINT_SAMPLE, ROUTE_HINT_TTL,
    ROUTE_PROBE_BYTES, ROUTE_PROBE_WORKERS, SLOW_WINDOW, USER_AGENT,
};
use super::data::Component;
use super::http::{BodyReader, Curl, TransferError};
use super::storage::create_download_temp;
use super::{Error, Installer, Result};

#[derive(Clone, Debug, Default)]
pub struct RouteHint {
    pub set: bool,
    pub prefix: String,
    pub name: String,
    pub verified_at: Option<Instant>,
    pub sample_size: i64,
}

impl RouteHint {
    /// Go: mirrors.go:43-53 — `routeHint.usable`.
    pub fn usable(&self, component: &Component) -> bool {
        if !self.set {
            return false;
        }
        if let Some(verified_at) = self.verified_at {
            if verified_at.elapsed() >= ROUTE_HINT_TTL {
                return false;
            }
        }
        self.sample_size == 0
            || component.size <= self.sample_size
            || self.sample_size >= ROUTE_HINT_SAMPLE
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    pub url: String,
    pub name: String,
    pub prefix: String,
    pub speed: f64,
    pub bytes: i64,
    pub elapsed: Duration,
    pub reason: String,
    pub retryable: bool,
}

/// Go: mirrors.go:64-76 — `routeName`.
fn route_name(prefix: &str) -> String {
    if prefix.is_empty() {
        return "GitHub".to_string();
    }
    let name = prefix.strip_prefix("https://").unwrap_or(prefix);
    let name = name.split('/').next().unwrap_or("");
    if name.is_empty() {
        "mirror".to_string()
    } else {
        name.to_string()
    }
}

pub struct DownloadJob {
    pub component: Component,
    pub direct: String,
    pub destination: PathBuf,
}

fn seconds(value: Duration) -> String {
    format!("{:.3}", value.as_secs_f64())
}

fn curl_program(configured: Option<&PathBuf>, module_dir: &Path) -> PathBuf {
    if let Some(configured) = configured {
        return configured.clone();
    }
    let mut candidates = vec![
        module_dir.join("bin/curl"),
        module_dir.join("system/bin/curl"),
    ];
    if cfg!(target_os = "android") {
        candidates.push(PathBuf::from("/system/bin/curl"));
        candidates.push(PathBuf::from("/system/xbin/curl"));
        candidates.push(PathBuf::from("/vendor/bin/curl"));
    } else {
        candidates.push(PathBuf::from("/usr/bin/curl"));
        candidates.push(PathBuf::from("/bin/curl"));
        candidates.push(PathBuf::from("/usr/local/bin/curl"));
    }
    for path in &candidates {
        if path.is_absolute() && path.is_file() {
            return path.clone();
        }
    }
    if cfg!(target_os = "android") {
        PathBuf::from("/system/bin/curl")
    } else {
        PathBuf::from("/usr/bin/curl")
    }
}

impl Installer<'_> {
    /// Build one trusted curl invocation. Production only permits HTTPS, for
    /// both the initial URL and every redirect; tests may opt into loopback
    /// HTTP through the private `allow_http` config field.
    pub fn curl_command(
        &self,
        url: &str,
        range: Option<(i64, i64)>,
        max_time: Duration,
        connect_timeout: Duration,
    ) -> Command {
        let program = curl_program(self.cfg.curl.as_ref(), &self.cfg.module_dir);
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/system/bin:/system/xbin:/vendor/bin:/usr/bin:/bin")
            .env("LC_ALL", "C")
            .args([
                "-q",
                "-sS",
                "--no-buffer",
                "--noproxy",
                "*",
                "--max-redirs",
                &MAX_REDIRECTS.to_string(),
                "--connect-timeout",
                &seconds(connect_timeout),
                "--max-time",
                &seconds(max_time),
                "--user-agent",
                USER_AGENT,
                "--header",
                "Accept-Encoding: identity",
                "--include",
            ]);
        let proto = if self.cfg.allow_http {
            "=http,https"
        } else {
            "=https"
        };
        command.args(["--proto", proto, "--proto-redir", proto]);
        if let Some((start, end)) = range {
            command.args(["--range", &format!("{start}-{end}")]);
        }
        command.args(["--url", url]);
        command
    }

    pub fn cached_route(&self, component: &Component, direct: &str) -> Option<Route> {
        let hint = self.route.lock().ok()?;
        if !hint.usable(component) {
            return None;
        }
        Some(Route {
            url: format!("{}{direct}", hint.prefix),
            name: hint.name.clone(),
            prefix: hint.prefix.clone(),
            speed: 0.0,
            bytes: 0,
            elapsed: Duration::ZERO,
            reason: String::new(),
            retryable: false,
        })
    }

    fn remember_route(&self, candidate: &Route, component: &Component) {
        if let Ok(mut hint) = self.route.lock() {
            *hint = RouteHint {
                set: true,
                prefix: candidate.prefix.clone(),
                name: candidate.name.clone(),
                verified_at: Some(Instant::now()),
                sample_size: component.size,
            };
        }
    }

    fn remember_probed_route(&self, candidate: &Route, component: &Component) {
        if let Ok(mut hint) = self.route.lock() {
            *hint = RouteHint {
                set: true,
                prefix: candidate.prefix.clone(),
                name: candidate.name.clone(),
                verified_at: Some(Instant::now()),
                sample_size: component.size.min(ROUTE_PROBE_BYTES),
            };
        }
    }

    fn invalidate_route(&self, prefix: &str) {
        if let Ok(mut hint) = self.route.lock() {
            if hint.prefix == prefix {
                hint.set = false;
            }
        }
    }

    /// Go: mirrors.go:78-159 — `probe`.
    pub fn probe(&self, component: &Component, raw_url: &str, name: &str, prefix: &str) -> Route {
        let started = Instant::now();
        let (bytes, speed, reason, retryable) = self.probe_sample(component, raw_url, started);
        Route {
            url: raw_url.to_string(),
            name: name.to_string(),
            prefix: prefix.to_string(),
            speed,
            bytes,
            elapsed: started.elapsed(),
            reason,
            retryable,
        }
    }

    fn probe_sample(
        &self,
        component: &Component,
        raw_url: &str,
        started: Instant,
    ) -> (i64, f64, String, bool) {
        let budget = self.cfg.probe_timeout.unwrap_or(PROBE_TIMEOUT);
        let deadline = started + budget;
        let size = component.size.min(ROUTE_PROBE_BYTES);
        if size < 4 {
            return (0, 0.0, "unavailable".into(), false);
        }
        let command = self.curl_command(
            raw_url,
            Some((0, size - 1)),
            budget,
            CONNECT_TIMEOUT.min(budget),
        );
        let mut curl = match Curl::spawn(command) {
            Ok(curl) => curl,
            Err(_) => return (0, 0.0, "unavailable".into(), true),
        };
        let headers = match curl.read_headers(deadline) {
            Ok(headers) => headers,
            Err(_) => {
                curl.abort_with_stderr();
                return (0, 0.0, "unavailable".into(), true);
            }
        };
        match headers.status {
            200 => {
                if let Some(length) = headers.content_length() {
                    if length != component.size {
                        curl.abort_with_stderr();
                        return (0, 0.0, "size mismatch".into(), false);
                    }
                }
            }
            206 => {
                let expected = format!("bytes 0-{}/{}", size - 1, component.size);
                if headers.get("content-range") != Some(expected.as_str())
                    || headers
                        .content_length()
                        .is_some_and(|length| length != size)
                {
                    curl.abort_with_stderr();
                    return (0, 0.0, "invalid range".into(), false);
                }
            }
            status => {
                curl.abort_with_stderr();
                return (0, 0.0, format!("HTTP {status}"), false);
            }
        }
        if let Some(encoding) = headers.get("content-encoding") {
            if !encoding.is_empty() && encoding != "identity" {
                curl.abort_with_stderr();
                return (0, 0.0, "unexpected encoding".into(), false);
            }
        }
        let mut header = [0u8; 4];
        let mut got = 0usize;
        let mut timed_out = false;
        while got < 4 {
            match curl.read_chunk(&mut header[got..], deadline) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(TransferError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Err(_) => {
                    curl.abort_with_stderr();
                    return (0, 0.0, "unavailable".into(), true);
                }
            }
        }
        if got < 4 {
            curl.abort_with_stderr();
            return (0, 0.0, "incomplete sample".into(), true);
        }
        if &header != b"PK\x03\x04" {
            curl.abort_with_stderr();
            return (0, 0.0, "not a component ZIP".into(), false);
        }
        let mut hasher = Sha256::new();
        hasher.update(&header);
        let mut received: i64 = 4;
        let mut buffer = [0u8; PROBE_COPY_BUFFER];
        let mut read_error = false;
        while received < size {
            let want = (size - received).min(buffer.len() as i64) as usize;
            match curl.read_chunk(&mut buffer[..want], deadline) {
                Ok(0) => break,
                Ok(n) => {
                    let n = n as i64;
                    hasher.update(&buffer[..n as usize]);
                    received += n;
                }
                Err(TransferError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Err(_) => {
                    read_error = true;
                    break;
                }
            }
        }
        // curl's own --max-time may close the pipe a few milliseconds before
        // our deadline check; a partial sample at the budget edge still counts
        // as a measured route, exactly like the Go context-deadline path.
        if !timed_out && received < size && started.elapsed() + Duration::from_millis(20) >= budget {
            timed_out = true;
        }
        if timed_out && received < size {
            // Count bytes curl already delivered into the pipe before we killed
            // it. The Go sampler sees these through its final Read.
            let mut drain = [0u8; PROBE_COPY_BUFFER];
            while received < size {
                match curl.read_available(&mut drain) {
                    Ok(Some(0)) | Ok(None) => break,
                    Ok(Some(n)) => {
                        hasher.update(&drain[..n]);
                        received += n as i64;
                    }
                    Err(_) => break,
                }
            }
        }
        curl.abort_with_stderr();
        if read_error {
            return (0, 0.0, "read error".into(), true);
        }
        let complete = received == size;
        if !complete && !(timed_out && received >= size.min(4096)) {
            return (0, 0.0, "incomplete sample".into(), true);
        }
        if complete && size == component.size && sha256_hex_from(&hasher) != component.sha256 {
            return (0, 0.0, "checksum mismatch".into(), false);
        }
        let elapsed = started.elapsed().as_secs_f64().max(1e-9);
        (received, received as f64 / elapsed, String::new(), false)
    }

    pub fn report_route(&self, route: &Route) {
        if !route.reason.is_empty() {
            self.cfg
                .log
                .line(format_args!("[route] {}: {}", route.name, route.reason));
            return;
        }
        self.cfg.log.line(format_args!(
            "[route] {}: {:.0} KiB/s ({} in {:.2}s)",
            route.name,
            route.speed / 1024.0,
            super::log::human_bytes(route.bytes),
            route.elapsed.as_secs_f64()
        ));
    }

    /// Go: mirrors.go:169-190 — `routeCandidates`.
    pub fn route_candidates(&self, direct: &str, attempted: &HashSet<String>) -> Vec<Route> {
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        let mut prefixes: Vec<&str> = vec![""];
        prefixes.extend(self.mirrors.iter().map(String::as_str));
        for raw in prefixes {
            let prefix = if raw.is_empty() {
                String::new()
            } else {
                format!("{}/", raw.trim_end_matches('/'))
            };
            let target = format!("{prefix}{direct}");
            if !seen.insert(target.clone()) || attempted.contains(&target) {
                continue;
            }
            candidates.push(Route {
                url: target,
                name: route_name(&prefix),
                prefix,
                speed: 0.0,
                bytes: 0,
                elapsed: Duration::ZERO,
                reason: String::new(),
                retryable: false,
            });
            if candidates.len() > MAX_MIRROR_ROUTES {
                break;
            }
        }
        candidates
    }

    /// Go: mirrors.go:192-224 — `rankRoutes`. Probe at most four mirrors in
    /// parallel, log from the collecting thread, and rank deterministically.
    pub fn rank_routes(
        &self,
        component: &Component,
        direct: &str,
        attempted: &HashSet<String>,
    ) -> Vec<Route> {
        let candidates = self.route_candidates(direct, attempted);
        if candidates.is_empty() {
            return Vec::new();
        }
        let count = candidates.len();
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<Route>>> = Mutex::new((0..count).map(|_| None).collect());
        let workers = ROUTE_PROBE_WORKERS.min(count);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let candidate = &candidates[index];
                    let route = self.probe(component, &candidate.url, &candidate.name, &candidate.prefix);
                    if let Ok(mut results) = results.lock() {
                        results[index] = Some(route);
                    }
                });
            }
        });
        let ranked = results.into_inner().unwrap_or_default();
        let mut routes = Vec::with_capacity(count);
        // Only the collecting thread writes logs; this keeps test buffers sane.
        for route in ranked.into_iter().flatten() {
            self.report_route(&route);
            routes.push(route);
        }
        ranked_download_routes(routes)
    }

    /// Go: mirrors.go:239-250 — `routes`.
    pub fn routes(&self, component: &Component, direct: &str) -> Vec<Route> {
        let first = self.probe(component, direct, "GitHub", "");
        self.report_route(&first);
        if first.bytes >= FAST_DIRECT_BYTES && first.speed >= FAST_DIRECT_SPEED {
            return vec![first];
        }
        self.cfg.log.line(format_args!(
            "[route] comparing GitHub mirrors: at most {} parallel, 1 MiB per path",
            ROUTE_PROBE_WORKERS
        ));
        let mut attempted = HashSet::new();
        attempted.insert(direct.to_string());
        let mut results = vec![first];
        results.extend(self.rank_routes(component, direct, &attempted));
        ranked_download_routes(results)
    }

    /// Go: main.go:512-582 — `download`.
    pub fn download(
        &self,
        component: &Component,
        direct: &str,
        destination: &Path,
    ) -> Result<()> {
        let mut candidates;
        if let Some(cached) = self.cached_route(component, direct) {
            candidates = vec![cached];
            self.cfg
                .log
                .line(format_args!("[route] {}: reusing recently selected route", candidates[0].name));
        } else {
            let _select = self
                .route_select
                .lock()
                .map_err(|_| Error("route selection lock poisoned".into()))?;
            if let Some(cached) = self.cached_route(component, direct) {
                candidates = vec![cached];
                self.cfg.log.line(format_args!(
                    "[route] {}: sharing concurrently selected route",
                    candidates[0].name
                ));
            } else {
                candidates = self.routes(component, direct);
                if let Some(first) = candidates.first() {
                    self.remember_probed_route(first, component);
                }
            }
        }
        let mut attempted: HashSet<String> = HashSet::new();
        let mut slow: Option<Route> = None;
        let mut last = "no usable download route".to_string();
        let mut index = 0usize;
        while index < candidates.len() {
            let candidate = candidates[index].clone();
            index += 1;
            if !attempted.insert(candidate.url.clone()) {
                continue;
            }
            // A soft speed floor may try alternatives, but never makes the only
            // progressing path unusable. Hard idle/TLS/hash checks always apply.
            let switch_slow = !self.mirrors.is_empty() || candidates.len() > 1;
            match self.download_attempt_policy(component, &candidate.url, destination, switch_slow) {
                Ok(()) => {
                    self.remember_route(&candidate, component);
                    self.cfg.log.line(format_args!(
                        "[download] {}: verified via {}",
                        component.id, candidate.name
                    ));
                    return Ok(());
                }
                Err(error) => {
                    last = error.message();
                    if matches!(error, TransferError::Slow) && slow.is_none() {
                        slow = Some(candidate.clone());
                    }
                    self.cfg.log.line(format_args!(
                        "[download] {} via {} failed: {}",
                        component.id,
                        candidate.name,
                        last
                    ));
                    self.invalidate_route(&candidate.prefix);
                    if candidates.len() == 1 {
                        let fallbacks = self.rank_routes(component, direct, &attempted);
                        candidates.extend(fallbacks);
                    }
                }
            }
        }
        if let Some(slow) = slow {
            self.cfg.log.line(format_args!(
                "[route] alternatives exhausted; retaining slow path {}",
                slow.name
            ));
            match self.download_attempt(component, &slow.url, destination) {
                Ok(()) => {
                    self.remember_route(&slow, component);
                    self.invalidate_route(&slow.prefix);
                    return Ok(());
                }
                Err(error) => last = error.message(),
            }
        }
        Err(Error(format!(
            "{}: all download routes failed: {last}",
            component.id
        )))
    }

    pub fn download_attempt(
        &self,
        component: &Component,
        url: &str,
        destination: &Path,
    ) -> std::result::Result<(), TransferError> {
        self.download_attempt_policy(component, url, destination, false)
    }

    /// Go: main.go:411-470 — one full, hash-verified transfer attempt.
    fn download_attempt_policy(
        &self,
        component: &Component,
        url: &str,
        destination: &Path,
        switch_slow: bool,
    ) -> std::result::Result<(), TransferError> {
        let started = Instant::now();
        let request_timeout = self.cfg.request_timeout.unwrap_or(REQUEST_TIMEOUT);
        let overall_deadline = started + request_timeout;
        let header_deadline = (started + RESPONSE_HEADER_TIMEOUT).min(overall_deadline);
        let idle = self.cfg.body_idle_timeout.unwrap_or(BODY_IDLE_TIMEOUT);
        let window = if switch_slow {
            self.cfg.slow_window.unwrap_or(SLOW_WINDOW)
        } else {
            Duration::ZERO
        };
        let command = self.curl_command(url, None, request_timeout, CONNECT_TIMEOUT);
        let mut curl = Curl::spawn(command).map_err(|error| TransferError::Transport(error.message()))?;
        let headers = curl.read_headers(header_deadline).map_err(|error| {
            let stderr = match &error {
                TransferError::Timeout | TransferError::Idle => curl.abort_with_stderr(),
                _ => curl.abort_with_stderr(),
            };
            transfer_with_stderr(error, stderr)
        })?;
        if headers.status != 200 {
            let stderr = curl.abort_with_stderr();
            return Err(transfer_with_stderr(
                TransferError::Transport(format!("HTTP {}", headers.status)),
                stderr,
            ));
        }
        if let Some(length) = headers.content_length() {
            if length != component.size {
                let stderr = curl.abort_with_stderr();
                return Err(transfer_with_stderr(
                    TransferError::Transport("download size mismatch".into()),
                    stderr,
                ));
            }
        }
        let directory = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = create_download_temp(directory).map_err(|error| {
            let stderr = curl.abort_with_stderr();
            transfer_with_stderr(TransferError::Transport(error.message()), stderr)
        })?;
        let mut hasher = Sha256::new();
        let mut progress = super::log::Progress::new(component.size, &self.cfg.log, &component.id);
        let mut total: i64 = 0;
        let mut buffer = [0u8; 32 * 1024];
        let mut failure: Option<TransferError> = None;
        {
            let mut reader = BodyReader::new(&mut curl, idle, window, overall_deadline, component.size);
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n as i64;
                        if let Err(error) = temp.file.write_all(&buffer[..n]) {
                            failure = Some(TransferError::Transport(error.to_string()));
                            break;
                        }
                        hasher.update(&buffer[..n]);
                        progress.write(n as i64);
                        if total > component.size {
                            failure = Some(TransferError::Transport(
                                "download exceeded its declared size".into(),
                            ));
                            break;
                        }
                        if reader.take_slow() {
                            failure = Some(TransferError::Slow);
                            break;
                        }
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        if let Some(error) = failure {
            let stderr = curl.abort_with_stderr();
            return Err(transfer_with_stderr(error, stderr));
        }
        if let Err(error) = temp.file.sync_all() {
            let stderr = curl.abort_with_stderr();
            return Err(transfer_with_stderr(
                TransferError::Transport(error.to_string()),
                stderr,
            ));
        }
        let status = curl.wait().map_err(|error| {
            transfer_with_stderr(TransferError::Transport(error.message()), curl.stderr_text())
        })?;
        if !status.success() {
            let stderr = curl.stderr_text();
            return Err(transfer_with_stderr(
                TransferError::Transport(format!("curl exited with {status}")),
                stderr,
            ));
        }
        if total != component.size || super::storage::hex_digest(&hasher) != component.sha256 {
            return Err(TransferError::Transport(
                "download checksum mismatch".into(),
            ));
        }
        fs::rename(&temp.path, destination)
            .map_err(|error| TransferError::Transport(error.to_string()))?;
        temp.disarm();
        Ok(())
    }

    /// Go: main.go:590-627 — at most three component transfers in flight.
    pub fn download_components(&self, items: Vec<DownloadJob>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let workers = COMPONENT_DOWNLOAD_WORKERS.min(items.len());
        self.cfg.log.line(format_args!(
            "[download] fetching {} components with up to {} parallel transfers",
            items.len(),
            workers
        ));
        let queue = Mutex::new(VecDeque::from(items));
        let failed = AtomicBool::new(false);
        let first: Mutex<Option<Error>> = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    if failed.load(Ordering::SeqCst) {
                        break;
                    }
                    let job = match queue.lock() {
                        Ok(mut queue) => queue.pop_front(),
                        Err(_) => break,
                    };
                    let Some(job) = job else { break };
                    if failed.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Err(error) = self.download(&job.component, &job.direct, &job.destination) {
                        if let Ok(mut first) = first.lock() {
                            if first.is_none() {
                                *first = Some(error);
                            }
                        }
                        failed.store(true, Ordering::SeqCst);
                    }
                });
            }
        });
        match first.into_inner() {
            Ok(Some(error)) => Err(error),
            _ => Ok(()),
        }
    }
}

/// Go: mirrors.go:226-237 — keep measured routes and retryable unknown
/// throughput, stable-sorted fastest first.
pub fn ranked_download_routes(candidates: Vec<Route>) -> Vec<Route> {
    let mut results: Vec<Route> = candidates
        .into_iter()
        .filter(|route| route.speed > 0.0 || route.retryable)
        .collect();
    results.sort_by(|a, b| {
        b.speed
            .partial_cmp(&a.speed)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

fn transfer_with_stderr(error: TransferError, stderr: String) -> TransferError {
    if let TransferError::Transport(message) = &error {
        if !stderr.is_empty() {
            return TransferError::Transport(format!("{message}: {stderr}"));
        }
    }
    error
}

fn sha256_hex_from(hasher: &Sha256) -> String {
    let digest = hasher.clone().finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn default_mirrors() -> Vec<String> {
    DEFAULT_MIRRORS.iter().map(|value| value.to_string()).collect()
}
