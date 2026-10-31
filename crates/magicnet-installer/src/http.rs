//! HTTP transport for the installer, built on the CLI's trusted `curl`.
//!
//! Go uses `net/http` in process (`main.go:308-331,377-470`). The Rust CLI has
//! no TLS stack and already treats `curl` as the trusted downloader
//! (`module_update.rs`), so this module spawns `curl` per attempt and keeps
//! the Go transfer policy in Rust:
//!
//! * the response header block is read from `curl --include` output, so status,
//!   `Content-Length`, `Content-Range` and `Content-Encoding` are checked
//!   before any body bytes are accepted;
//! * a watchdog aborts the attempt after 20 s without received bytes, while
//!   every received chunk resets that deadline;
//! * an optional 8 s sliding window aborts a sustained transfer below
//!   128 KiB/s, unless the file is nearly complete;
//! * the curl child is always killed and reaped, so no partial reader leaks.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::constants::{MAX_HEADER_BYTES, SLOW_FLOOR_BYTES_PER_SEC, SLOW_REMAINING_BYTES};
use super::Error;

#[derive(Debug)]
pub enum TransferError {
    /// The soft speed floor rejected an otherwise progressing transfer.
    Slow,
    /// No bytes arrived within the resetting idle deadline.
    Idle,
    /// The attempt's overall deadline expired.
    Timeout,
    /// The transport failed (curl error, pipe error, invalid header, ...).
    Transport(String),
}

impl TransferError {
    pub fn message(&self) -> String {
        match self {
            TransferError::Slow => "sustained download speed below 128 KiB/s".to_string(),
            TransferError::Idle => "download stalled past the idle deadline".to_string(),
            TransferError::Timeout => "download attempt deadline expired".to_string(),
            TransferError::Transport(message) => message.clone(),
        }
    }
}

impl From<TransferError> for Error {
    fn from(value: TransferError) -> Self {
        Error(value.message())
    }
}

pub struct Headers {
    pub status: u16,
    fields: Vec<(String, String)>,
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn content_length(&self) -> Option<i64> {
        self.get("content-length")
            .and_then(|value| value.trim().parse::<i64>().ok())
    }
}

pub struct Curl {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stderr_output: Option<String>,
    pending: Vec<u8>,
    reaped: bool,
}

impl Curl {
    pub fn spawn(mut command: Command) -> super::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(Error::from_io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error("curl stdout is unavailable".into()))?;
        let stderr = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut raw = Vec::new();
                let _ = stderr.by_ref().take(8192).read_to_end(&mut raw);
                raw
            })
        });
        Ok(Self {
            child,
            stdout,
            stderr,
            stderr_output: None,
            pending: Vec::new(),
            reaped: false,
        })
    }

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

    /// One chunk from the child, bounded by `deadline`. Pending bytes from the
    /// header read are served first.
    pub fn read_chunk(&mut self, buffer: &mut [u8], deadline: Instant) -> std::result::Result<usize, TransferError> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if !self.pending.is_empty() {
            let length = buffer.len().min(self.pending.len());
            buffer[..length].copy_from_slice(&self.pending[..length]);
            self.pending.drain(..length);
            return Ok(length);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransferError::Timeout);
            }
            let readable = self
                .wait_readable(remaining.min(Duration::from_secs(5)))
                .map_err(|error| TransferError::Transport(error.to_string()))?;
            if !readable {
                continue;
            }
            match self.stdout.read(buffer) {
                Ok(0) => return Ok(0),
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(TransferError::Transport(error.to_string())),
            }
        }
    }

    /// Drain bytes that are already buffered in the pipe (or in our header
    /// pending buffer) without waiting. Used after a probe deadline so bytes
    /// delivered before the kill still count as a measured sample.
    pub fn read_available(&mut self, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        if buffer.is_empty() {
            return Ok(Some(0));
        }
        if !self.pending.is_empty() {
            let length = buffer.len().min(self.pending.len());
            buffer[..length].copy_from_slice(&self.pending[..length]);
            self.pending.drain(..length);
            return Ok(Some(length));
        }
        match self.wait_readable(Duration::ZERO)? {
            false => Ok(None),
            true => self.stdout.read(buffer).map(Some),
        }
    }

    /// Go: `http.Response` parsing. curl follows redirects itself
    /// (`--location --max-redirs 10 --proto-redir =https`), and `--include`
    /// emits every response block; intermediate 1xx/3xx blocks are skipped.
    pub fn read_headers(&mut self, deadline: Instant) -> std::result::Result<Headers, TransferError> {
        'blocks: loop {
            let mut collected = Vec::new();
            let mut scan_from = 0usize;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(TransferError::Timeout);
                }
                if collected.len() >= MAX_HEADER_BYTES {
                    return Err(TransferError::Transport(
                        "response header block is too large".into(),
                    ));
                }
                let mut chunk = [0u8; 4096];
                let n = self.read_chunk(&mut chunk, deadline)?;
                if n == 0 {
                    if collected.is_empty() {
                        return Err(TransferError::Transport(
                            "curl produced no response headers".into(),
                        ));
                    }
                    return Err(TransferError::Transport(
                        "truncated response headers".into(),
                    ));
                }
                collected.extend_from_slice(&chunk[..n]);
                if let Some(relative) = collected[scan_from..]
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                {
                    let end = scan_from + relative + 4;
                    let headers = parse_headers(&collected[..end])?;
                    // Body bytes that arrived with the header block stay pending.
                    self.pending.extend_from_slice(&collected[end..]);
                    if (100..200).contains(&headers.status)
                        || matches!(headers.status, 301 | 302 | 303 | 307 | 308)
                    {
                        // curl discards redirect bodies itself; any leftover is
                        // the next response block and must be parsed next.
                        continue 'blocks;
                    }
                    return Ok(headers);
                }
                scan_from = collected.len().saturating_sub(3);
            }
        }
    }

    fn collect_stderr(&mut self) {
        if let Some(handle) = self.stderr.take() {
            let raw = handle.join().unwrap_or_default();
            self.stderr_output = Some(String::from_utf8_lossy(&raw).trim().to_string());
        }
    }

    /// Drain stderr and reap the child.
    pub fn wait(&mut self) -> super::Result<ExitStatus> {
        self.reaped = true;
        let status = self.child.wait().map_err(Error::from_io);
        self.collect_stderr();
        status
    }

    pub fn stderr_text(&self) -> String {
        self.stderr_output.clone().unwrap_or_default()
    }

    /// Kill the child, reap it, and return whatever curl printed on stderr.
    pub fn abort_with_stderr(&mut self) -> String {
        let _ = self.child.kill();
        self.reaped = true;
        let _ = self.child.wait();
        self.collect_stderr();
        self.stderr_text()
    }
}

impl Drop for Curl {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(handle) = self.stderr.take() {
                let _ = handle.join();
            }
        }
    }
}

fn parse_headers(block: &[u8]) -> std::result::Result<Headers, TransferError> {
    let text = std::str::from_utf8(block)
        .map_err(|_| TransferError::Transport("non-UTF-8 response headers".into()))?;
    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| TransferError::Transport("missing response status line".into()))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(TransferError::Transport(format!(
            "invalid response status line: {status_line}"
        )));
    }
    let status = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| TransferError::Transport("invalid response status code".into()))?;
    let mut fields = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(TransferError::Transport(
                "invalid response header line".into(),
            ));
        };
        fields.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Headers { status, fields })
}

/// Go: main.go:377-409 — `activityReader`. Reads body bytes, enforcing the
/// resettable idle deadline and the optional sliding speed window.
pub struct BodyReader<'a> {
    curl: &'a mut Curl,
    idle: Duration,
    window: Duration,
    deadline: Instant,
    window_start: Instant,
    window_bytes: i64,
    pub received: i64,
    total: i64,
    slow: bool,
}

impl<'a> BodyReader<'a> {
    pub fn new(
        curl: &'a mut Curl,
        idle: Duration,
        window: Duration,
        deadline: Instant,
        total: i64,
    ) -> Self {
        Self {
            curl,
            idle,
            window,
            deadline,
            window_start: Instant::now(),
            window_bytes: 0,
            received: 0,
            total,
            slow: false,
        }
    }

    pub fn take_slow(&mut self) -> bool {
        std::mem::take(&mut self.slow)
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> std::result::Result<usize, TransferError> {
        if self.slow {
            return Err(TransferError::Slow);
        }
        let now = Instant::now();
        let remaining = self.deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(TransferError::Timeout);
        }
        let mut chunk = [0u8; 32 * 1024];
        let length = buffer.len().min(chunk.len());
        let read_deadline = now + remaining.min(self.idle);
        let n = self
            .curl
            .read_chunk(&mut chunk[..length], read_deadline)
            .map_err(|error| match error {
                TransferError::Timeout if Instant::now() < self.deadline => TransferError::Idle,
                other => other,
            })?;
        if n == 0 {
            return Ok(0);
        }
        buffer[..n].copy_from_slice(&chunk[..n]);
        self.received += n as i64;
        self.window_bytes += n as i64;
        if self.window > Duration::ZERO {
            let elapsed = Instant::now().duration_since(self.window_start);
            if elapsed >= self.window {
                let seconds = elapsed.as_secs_f64().max(1e-9);
                if self.total - self.received > SLOW_REMAINING_BYTES
                    && self.window_bytes as f64 / seconds < SLOW_FLOOR_BYTES_PER_SEC
                {
                    self.slow = true;
                }
                self.window_start = Instant::now();
                self.window_bytes = 0;
            }
        }
        Ok(n)
    }
}
