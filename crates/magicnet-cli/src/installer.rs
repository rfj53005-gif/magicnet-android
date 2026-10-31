use std::path::PathBuf;

use magicnet_installer::{cleanup_legacy_storage, Config, Installer, Log};

use crate::App;

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
        return cleanup_legacy_storage(&module_dir).map_err(|error| error.message());
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
            return Err(format!("unknown installer action: {other}\n{}", usage()));
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