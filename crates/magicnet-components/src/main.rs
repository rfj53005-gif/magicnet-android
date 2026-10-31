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

fn value_of(arg: &str, name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    if let Some(rest) = arg.strip_prefix(&prefix) {
        return Some(rest.to_string());
    }
    let dashed = format!("-{name}");
    if arg == format!("--{name}") || arg == dashed {
        return Some(String::new());
    }
    None
}

fn take_value(args: &[String], index: &mut usize, name: &str) -> Result<String, String> {
    let arg = args[*index].as_str();
    if let Some(inlined) = value_of(arg, name) {
        if !inlined.is_empty() {
            return Ok(inlined);
        }
    }
    *index += 1;
    let value = args
        .get(*index)
        .ok_or_else(|| format!("flag needs an argument: {name}"))?;
    if value.starts_with('-') {
        return Err(format!("flag needs an argument: {name}"));
    }
    Ok(value.clone())
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
        let name = raw.trim_start_matches('-');
        if takes_value(name) {
            let value = take_value(args, &mut index, name)?;
            match name {
                "archive" => flags.archive = value,
                "module-dir" => flags.module_dir = value,
                "previous-dir" => flags.previous_dir = value,
                "cache-dir" => flags.cache_dir = value,
                _ => unreachable!(),
            }
        } else {
            match name {
                "offline" => flags.offline = true,
                "cleanup-legacy" => flags.cleanup_legacy = true,
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
