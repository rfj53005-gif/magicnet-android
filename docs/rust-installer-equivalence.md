# Rust installer equivalence (Go: installer/components -> crates/magicnet-cli)

Branch `rust/p1-installer-pilot` (worktree `magicnet/work/installer-rust-20261031`),
base commit `96993ea5`. Source Go tree kept in place; the Rust command is
`magicnet-cli installer <action>`.

## Command surface

| Go binary flag / mode | Rust CLI |
|---|---|
| default install run | `installer install --archive <zip> --module-dir <dir> [--previous-dir <dir>] [--cache-dir <dir>] [--offline]` |
| (no equivalent; new) | `installer fetch --archive ... --cache-dir ...` (prefetch payloads into cache-dir, Go download policy) |
| verify-staged semantics of manifest path | `installer verify --archive ...` (read-only checks, no lock/network) |
| offline apply | `installer apply --archive ... --cache-dir ...` (= install + forced offline) |
| `--cleanup-legacy` | `installer cleanup-legacy --module-dir <dir>` |

Filesystem layout, the owned workspace (`.magicnet-components`), the promotion
journal, `components.installed.json` and legacy ownership proofs are byte-for-byte
compatible: a Go installer can finish a Rust installer's interrupted transaction
and vice versa (both run under the same directory-inode flock).

## Constants

All tunables live in `crates/magicnet-cli/src/installer/constants.rs` and each
item cites the exact Go file and line (`main.go`/`storage.go`/`mirrors.go`/
`legacy_cleanup.go`) it was transcribed from: max payload 512 MiB, 3 component
download workers, 1 MiB route probes, 4 probe workers, 16 mirror routes, 20 s
idle deadlines, built-in HTTPS mirror prefixes, 480 s farewell idle window, etc.

## Test mapping (54 Go tests -> 54 Rust tests)

`crates/magicnet-cli/src/installer/tests.rs`; Go names become snake_case Rust
names with the identical scenario. 47 tests ported directly; the seven below
have notes.

| Go test | Rust test | Note |
|---|---|---|
| TestAndroidBootstrapDoesNotDependOnLibcDNS | bootstrap_resolves_nothing_in_process | Stronger boundary: Rust never resolves in-process; all transfers run through an explicit, environment-cleared curl child (`-q`, `--noproxy *`, timeouts pinned). |
| TestLegacyCleanupRemovesOldVersionsWithoutTouchingCurrentState | legacy_cleanup_removes_old_versions_without_touching_current_state | Idempotence across 3 runs and protected-path parity. |
| TestLegacyCleanupRespectsCacheLock | legacy_cleanup_respects_cache_lock | Same busy error, lock released then cleanup proceeds. |
| TestLegacyCleanupPreservesUnknownAndLinkedFiles | legacy_cleanup_preserves_unknown_and_linked_files | Symlinks/unknown names never deleted. |
| TestLegacyCleanupRejectsSymlinkRootsAndWrongIdentity | legacy_cleanup_rejects_symlink_roots_and_wrong_identity | 6 modes incl. module-link error. |
| TestLegacyCleanupRequiresExactBackupMarkerAndDeadOwner | legacy_cleanup_requires_exact_backup_marker_and_dead_owner | Bad markers and live-owner backups survive. |
| TestLegacyCleanupOnlyRemovesIdleRecognizedFarewell | legacy_cleanup_only_removes_idle_recognized_farewell | utimensat ages dirs; only exact worker contract + idle removed. |

## Deliberate deviations from Go

1. `lock_module` retries a bounded 30x10 ms grace on EAGAIN (Rust-only).
   flock is owned by the open file description; Rust's process spawn forks the
   descriptor table, so curl children transiently hold inherited lock copies
   during fork->exec, which surfaced as spurious contention in the parallel
   test suite. Real concurrent installers still get the same rejection error
   (covered by module_lock_cannot_be_bypassed_with_another_archive_source and
   legacy_cleanup_respects_cache_lock).
2. `installer fetch` (cache prefetch) has no Go counterpart; it reuses Go's
   exact download policy and verification order, and caches only whole,
   SHA-256-verified archives.
3. Logging: Go printed to stdout; Rust keeps the same line format but routes
   through the CLI log writer for --json consistency.

## Validation state (2026-10-31)

- `cargo test -p magicnet-cli`: 805 passed / 0 failed (bin), installer:: 54/54,
  incl. 6 consecutive parallel stress rounds clean.
- Known pre-existing repo flake (not installer): tests/ebpf_observation.rs
  timing-sensitive under full-suite parallel load; passes 4/4 standalone.
- Device-level acceptance (fake-magisk smoke, package smoke, onboarding,
  downloader parity) is tracked in board task-1 and runs after the hooks
  integration step.
