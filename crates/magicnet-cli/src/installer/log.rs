//! Installer logging and human-readable progress.
//!
//! Go writes through an `io.Writer` guarded by a process-wide mutex
//! (`main.go:41,332-338`) so parallel workers cannot interleave lines. The
//! Rust installer keeps the same one-line-at-a-time serialization, with a
//! buffer sink for tests and stdout for the CLI.

use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
#[allow(dead_code)] // the buffer sink is used by tests
pub(super) enum Log {
    Stdout,
    Buffer(Arc<Mutex<Vec<u8>>>),
    Discard,
}

impl Log {
    pub(super) fn line(&self, arguments: fmt::Arguments<'_>) {
        let mut text = arguments.to_string();
        text.push('\n');
        match self {
            Log::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            Log::Buffer(buffer) => {
                if let Ok(mut buffer) = buffer.lock() {
                    buffer.extend_from_slice(text.as_bytes());
                }
            }
            Log::Discard => {}
        }
    }
}

/// Go: main.go:340-349 — `humanBytes`.
pub(super) fn human_bytes(n: i64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Go: `time.Duration.String()` after `Round(time.Second)`.
fn go_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if minutes > 0 || hours > 0 {
        out.push_str(&format!("{minutes}m"));
    }
    out.push_str(&format!("{secs}s"));
    out
}

/// Go: main.go:351-375 — one progress line per second and at completion.
pub(super) struct Progress<'a> {
    pub(super) current: i64,
    pub(super) total: i64,
    last: Option<Instant>,
    start: Option<Instant>,
    log: &'a Log,
    id: &'a str,
}

impl<'a> Progress<'a> {
    pub(super) fn new(total: i64, log: &'a Log, id: &'a str) -> Self {
        Self {
            current: 0,
            total,
            last: None,
            start: None,
            log,
            id,
        }
    }

    pub(super) fn write(&mut self, count: i64) {
        if self.start.is_none() {
            self.start = Some(Instant::now());
        }
        self.current += count;
        let now = Instant::now();
        let due = self.last.is_none_or(|last| now.duration_since(last) > Duration::from_secs(1))
            || (self.total > 0 && self.current >= self.total);
        if !due {
            return;
        }
        let elapsed = self
            .start
            .map(|start| now.duration_since(start).as_secs_f64())
            .unwrap_or(0.0)
            .max(1e-9);
        let speed = self.current as f64 / elapsed;
        let eta = if speed > 0.0 && self.current < self.total {
            Duration::from_secs_f64((self.total - self.current) as f64 / speed)
                .as_secs()
        } else {
            0
        };
        let percent = if self.total > 0 {
            self.current * 100 / self.total
        } else {
            100
        };
        self.log.line(format_args!(
            "[download] {}: {}% {}/{} {:.0} KiB/s ETA {}",
            self.id,
            percent,
            human_bytes(self.current),
            human_bytes(self.total),
            speed / 1024.0,
            go_duration(eta)
        ));
        self.last = Some(now);
    }
}
