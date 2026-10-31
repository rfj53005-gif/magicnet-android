//! Constants for the Rust component installer.
//!
//! Every value here is transcribed from the Go reference implementation at
//! `installer/components/`. The Go file and line are cited on each item so a
//! reviewer can diff the two implementations without searching.
//!
//! The Go sources are intentionally kept in the tree during the migration; this
//! module is the single place Rust-side tunables may live.

use std::time::Duration;

/// Go: main.go:32 — `maxPayload = int64(512 << 20)`.
pub(super) const MAX_PAYLOAD: i64 = 512 * 1024 * 1024;

/// Go: main.go:33 — `componentDownloadWorkers = 3`.
///
/// The parallel unit is a whole component archive, never byte ranges: the
/// manifest authenticates complete ZIP bytes. Mirrors are compared with
/// 1 MiB range samples (ROUTE_PROBE_BYTES) before the real transfer.
pub(super) const COMPONENT_DOWNLOAD_WORKERS: usize = 3;

/// Go: mirrors.go:16 — `routeProbeBytes = int64(1 << 20)`.
pub(super) const ROUTE_PROBE_BYTES: i64 = 1 << 20;

/// Go: mirrors.go:17 — `routeProbeWorkers = 4`.
pub(super) const ROUTE_PROBE_WORKERS: usize = 4;

/// Go: mirrors.go:18 — `maxMirrorRoutes = 16`.
pub(super) const MAX_MIRROR_ROUTES: usize = 16;

/// Go: mirrors.go:24-34 — built-in candidate prefixes.
///
/// Candidates are not a speed ranking or a trust root. Only the manifest
/// embedded in the trusted module archive authenticates bytes.
pub(super) const DEFAULT_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/",
    "https://ghfast.top/",
    "https://ghproxy.net/",
    "https://gh.ddlc.top/",
    "https://gh.llkk.cc/",
    "https://gh.jasonzeng.dev/",
    "https://ghproxy.site/",
];

/// Go: main.go:444 — a response body is cancelled after 20 s without bytes.
/// Continued progress resets this idle deadline; the overall request timeout
/// below remains in force.
pub(super) const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Go: main.go:450-453 — the soft speed floor is only evaluated over an 8 s
/// window and only when another route exists.
pub(super) const SLOW_WINDOW: Duration = Duration::from_secs(8);

/// Go: main.go:400 — do not abandon a nearly complete file...
pub(super) const SLOW_REMAINING_BYTES: i64 = 256 * 1024;

/// Go: main.go:400/409 — sustained window throughput below 128 KiB/s.
pub(super) const SLOW_FLOOR_BYTES_PER_SEC: f64 = 128.0 * 1024.0;

/// Go: mirrors.go:83-85 — probe budget defaults to 5 s.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Go: mirrors.go:48 — a saved route expires after 5 minutes.
pub(super) const ROUTE_HINT_TTL: Duration = Duration::from_secs(5 * 60);

/// Go: mirrors.go:52 — a tiny fast ZIP says nothing about a later large asset.
pub(super) const ROUTE_HINT_SAMPLE: i64 = ROUTE_PROBE_BYTES;

/// Go: mirrors.go:240-245 — keep cheap direct access only for a genuinely fast
/// full 1 MiB sample; the old 64 KiB / 256 KiB/s shortcut hid faster mirrors.
pub(super) const FAST_DIRECT_BYTES: i64 = ROUTE_PROBE_BYTES;
pub(super) const FAST_DIRECT_SPEED: f64 = 4.0 * 1024.0 * 1024.0;

/// Go: main.go:321 — overall HTTP client timeout.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Go: main.go:322 — dial timeout (also used for the connect phase by curl).
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Go: main.go:324 — `ResponseHeaderTimeout: 15 * time.Second`.
pub(super) const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// Go: main.go:326 — at most 10 redirects; every hop must stay HTTPS.
pub(super) const MAX_REDIRECTS: u32 = 10;

/// Go: main.go:683 — embedded `components.json` is limited to 4 MiB.
pub(super) const MANIFEST_LIMIT: u64 = 4 << 20;

/// Go: main.go:705 — installed-state manifest read bound.
pub(super) const INSTALLED_MANIFEST_LIMIT: u64 = 4 << 20;

/// Go: main.go:101 — at most 128 components per manifest.
pub(super) const MAX_COMPONENTS: usize = 128;

/// Go: main.go:108 — at most 20000 files per component.
pub(super) const MAX_COMPONENT_FILES: usize = 20000;

/// Go: main.go:120 — all declared files together may not exceed 2 GiB.
pub(super) const MAX_TOTAL_FILE_BYTES: i64 = 2 << 30;

/// Go: storage.go:16 — `workspaceName = ".components-install"`.
pub(super) const WORKSPACE_NAME: &str = ".components-install";

/// Go: storage.go:17 — `workspaceOwner = ".magicnet-components-v1"`.
pub(super) const WORKSPACE_OWNER: &str = ".magicnet-components-v1";

/// Go: storage.go:18 — `promotionFile = ".promotion.json"`.
pub(super) const PROMOTION_FILE: &str = ".promotion.json";

/// Go: storage.go:124 — journal change cap.
pub(super) const MAX_PROMOTION_CHANGES: usize = 200_000;

/// Go: storage.go:229 — recovery journal read bound.
pub(super) const PROMOTION_LIMIT: u64 = 8 << 20;

/// Go: main.go:750 — downloaded component archives live in
/// `<workspace>/.downloads/<sha256>.zip` and are removed after staging.
pub(super) const DOWNLOADS_DIR: &str = ".downloads";

/// Go: main.go:434 — partial downloads are created with `CreateTemp(dir,
/// ".download-")`; tests and cleanup code match `.download-*`.
pub(super) const DOWNLOAD_TEMP_PREFIX: &str = ".download-";

/// Go: main.go:804 — `components.installed.json` is written with mode 0600.
pub(super) const INSTALLED_MANIFEST_PATH: &str = "components.installed.json";
pub(super) const INSTALLED_MANIFEST_MODE: u32 = 0o600;

/// Rust-specific: grace window for transient flock EAGAIN caused by child
/// processes inheriting the lock's open file description during fork→exec.
/// Not present in the Go reference; see storage.rs lock_module.
pub(super) const LOCK_FORK_GRACE_ATTEMPTS: usize = 30;
pub(super) const LOCK_FORK_GRACE_INTERVAL: Duration = Duration::from_millis(10);

/// Go: legacy_cleanup.go:161-163 — the old farewell worker can live 480 s;
/// directories are considered idle only after 10 minutes.
pub(super) const LEGACY_FAREWELL_IDLE: Duration = Duration::from_secs(10 * 60);

/// Go: main.go:422 — component download User-Agent.
pub(super) const USER_AGENT: &str = "MagicNet-components/1";

/// Go: mirrors.go:139 — probe copy buffer size.
pub(super) const PROBE_COPY_BUFFER: usize = 32 * 1024;

/// Header blocks accepted while parsing `curl --include` output.
pub(super) const MAX_HEADER_BYTES: usize = 64 * 1024;
