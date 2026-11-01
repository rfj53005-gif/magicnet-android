//! magicnet-components: standalone component bootstrap helper.
//!
//! Flag surface mirrors `installer/components/main.go` exactly (Go's flag
//! package accepts both `-name` and `--name`, plus `--name=value`):
//!   --archive, --module-dir, --previous-dir (default
//!   /data/adb/modules/MagicNet), --cache-dir, --offline, --cleanup-legacy.
//! Errors print as `[error] <message>` on stderr with exit code 1, matching
//! the Go helper's contract with update-binary and binstall scripts.

use std::path::PathBuf;
use std::process::exit;

use magicnet_installer::{cleanup_legacy_storage, Config, Installer, Log};

#[derive(Default)]
struct Flags {
    archive: String,
    module_dir: String,
    previous_dir: String,
    cache_dir: String,
    offline: bool,
    cleanup_legacy: bool,
}

fn takes_value(name: &str) -> bool {
    matches!(
        name,
        "archive" | "module-dir" | "previous-dir" | "cache-dir"
    )
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags {
        previous_dir: "/data/adb/modules/MagicNet".to_string(),
        ..Flags::default()
    };
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        let Some(raw) = arg.strip_prefix('-') else {
            return Err(format!("magicnet-components: unexpected argument {arg}"));
        };
        // Go's flag package accepts -name value, -name=value and --name=value.
        let raw = raw.trim_start_matches('-');
        let (name, inline) = match raw.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (raw, None),
        };
        if takes_value(name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| format!("flag needs an argument: {name}"))?;
                    if value.starts_with('-') {
                        return Err(format!("flag needs an argument: {name}"));
                    }
                    value.clone()
                }
            };
            match name {
                "archive" => flags.archive = value,
                "module-dir" => flags.module_dir = value,
                "previous-dir" => flags.previous_dir = value,
                "cache-dir" => flags.cache_dir = value,
                _ => unreachable!(),
            }
        } else {
            let enabled = match inline.as_deref() {
                None | Some("true") | Some("1") => true,
                Some("false") | Some("0") => false,
                Some(other) => {
                    return Err(format!("invalid boolean value {other:?} for -{name}"));
                }
            };
            match name {
                "offline" => flags.offline = enabled,
                "cleanup-legacy" => flags.cleanup_legacy = enabled,
                "help" | "h" => {
                    usage();
                    std::process::exit(0);
                }
                other => return Err(format!("flag provided but not defined: -{other}")),
            }
        }
        index += 1;
    }
    Ok(flags)
}

fn usage() {
    println!(
        "usage: magicnet-components [--archive <zip>] --module-dir <dir> [--previous-dir <dir>] [--cache-dir <dir>] [--offline] [--cleanup-legacy]"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flags = match parse(&args) {
        Ok(flags) => flags,
        Err(message) => {
            eprintln!("[error] {message}");
            usage();
            exit(2);
        }
    };
    let result = if flags.cleanup_legacy {
        if flags.module_dir.is_empty() {
            Err("module directory is required".to_string())
        } else {
            cleanup_legacy_storage(PathBuf::from(&flags.module_dir).as_path())
                .map_err(|error| error.message())
        }
    } else {
        let config = Config {
            archive: PathBuf::from(&flags.archive),
            module_dir: PathBuf::from(&flags.module_dir),
            previous_dir: PathBuf::from(&flags.previous_dir),
            cache_dir: if flags.cache_dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(&flags.cache_dir))
            },
            offline: flags.offline,
            log: Log::Stdout,
            ..Config::default()
        };
        Installer::new(&config).run().map_err(|error| error.message())
    };
    if let Err(message) = result {
        eprintln!("[error] {message}");
        exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn accepts_equals_forms_like_go_flag() {
        // Go's flag package accepts -name=value and --name=value; update-binary
        // and the binstall scripts use both spellings.
        let flags = parse(&args(&[
            "--archive=/tmp/components.zip",
            "--module-dir=/mnt/module",
            "--previous-dir=/mnt/old",
            "--cache-dir=/mnt/cache",
            "--offline=true",
            "--cleanup-legacy=false",
        ]))
        .expect("parse equals forms");
        assert_eq!(flags.archive, "/tmp/components.zip");
        assert_eq!(flags.module_dir, "/mnt/module");
        assert_eq!(flags.previous_dir, "/mnt/old");
        assert_eq!(flags.cache_dir, "/mnt/cache");
        assert!(flags.offline);
        assert!(!flags.cleanup_legacy);
    }

    #[test]
    fn accepts_separated_values_and_single_dash() {
        let flags = parse(&args(&[
            "-archive",
            "/tmp/a.zip",
            "--module-dir",
            "/mnt/m",
            "--offline",
        ]))
        .expect("parse separated forms");
        assert_eq!(flags.archive, "/tmp/a.zip");
        assert_eq!(flags.module_dir, "/mnt/m");
        assert!(flags.offline);
        assert_eq!(flags.previous_dir, "/data/adb/modules/MagicNet");
    }

    #[test]
    fn rejects_unknown_flags_and_missing_values() {
        assert!(parse(&args(&["--module-dir=/mnt", "--bogus"])).is_err());
        assert!(parse(&args(&["--archive"])).is_err());
        assert!(parse(&args(&["--offline=maybe"])).is_err());
    }
}
