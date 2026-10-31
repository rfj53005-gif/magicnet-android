//! The release-pinned `components.json` schema and its strict validation.
//!
//! Go: main.go:45-134. Parsing is manual rather than serde-derive so unknown
//! fields stay ignored (like `encoding/json`) while wrong JSON types, floats
//! for integer fields, and out-of-range modes fail exactly like the Go struct
//! decoder does.

use std::collections::HashSet;

use serde_json::Value;

use super::constants::{
    MAX_COMPONENT_FILES, MAX_COMPONENTS, MAX_PAYLOAD, MAX_TOTAL_FILE_BYTES,
};
use super::storage::{dir_name, is_hex_hash, managed_name};
use super::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PayloadFile {
    pub(super) path: String,
    pub(super) sha256: String,
    pub(super) size: i64,
    pub(super) mode: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Component {
    pub(super) id: String,
    pub(super) sha256: String,
    pub(super) size: i64,
    pub(super) asset: String,
    pub(super) files: Vec<PayloadFile>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Manifest {
    pub(super) schema: i64,
    pub(super) module: String,
    pub(super) version: String,
    pub(super) architecture: String,
    pub(super) repository: String,
    pub(super) components: Vec<Component>,
}

fn string_at<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error(format!("invalid component manifest field: {key}")))
}

fn i64_at(value: &Value, key: &str) -> Result<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| Error(format!("invalid component manifest field: {key}")))
}

fn u32_at(value: &Value, key: &str) -> Result<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| Error(format!("invalid component manifest field: {key}")))
}

/// Go: main.go:39 — `^v[0-9]+\.[0-9]+\.[0-9]+$`.
fn release_version_ok(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Go: main.go:38 — `^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`.
fn repository_id_ok(value: &str) -> bool {
    let mut parts = value.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, repo].iter().all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    })
}

/// Go: main.go:37 — `^[A-Za-z0-9_.-]+$`.
fn component_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Go: runtime.GOARCH names, which the packager writes ("arm64", "amd64").
pub(super) fn compiled_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        "x86" => "386",
        other => other,
    }
}

impl Manifest {
    /// Go: main.go:99-134 — `manifest.validate`.
    pub(super) fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.module != "MagicNet"
            || !release_version_ok(&self.version)
            || !repository_id_ok(&self.repository)
            || self.architecture != compiled_architecture()
            || self.components.is_empty()
            || self.components.len() > MAX_COMPONENTS
        {
            return Err(Error(
                "incompatible component manifest or architecture".into(),
            ));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut total: i64 = 0;
        for component in &self.components {
            if !component_id_ok(&component.id)
                || !ids.insert(component.id.clone())
                || !is_hex_hash(&component.sha256)
                || component.size <= 0
                || component.size > MAX_PAYLOAD
                || component.files.is_empty()
                || component.files.len() > MAX_COMPONENT_FILES
                || component.asset
                    != format!(
                        "MagicNet-component-{}-{}.zip",
                        component.id, component.sha256
                    )
            {
                return Err(Error(format!("invalid component: {:?}", component.id)));
            }
            for file in &component.files {
                if !managed_name(&file.path)
                    || !names.insert(file.path.clone())
                    || !is_hex_hash(&file.sha256)
                    || file.size < 0
                    || file.size > MAX_PAYLOAD
                    || !matches!(file.mode, 0o644 | 0o755 | 0o600 | 0o700)
                {
                    return Err(Error(format!(
                        "invalid component file: {:?}",
                        file.path
                    )));
                }
                total += file.size;
                if total > MAX_TOTAL_FILE_BYTES {
                    return Err(Error("component size limit exceeded".into()));
                }
            }
        }
        // A file may not be the parent of another file, even across components.
        for name in &names {
            let mut parent = dir_name(name);
            while parent != "." {
                if names.contains(&parent) {
                    return Err(Error(format!("overlapping component path: {name}")));
                }
                parent = dir_name(&parent);
            }
        }
        Ok(())
    }

    /// Go: main.go:695-700 — `json.Unmarshal` into the typed struct.
    pub(super) fn parse(raw: &[u8]) -> Result<Self> {
        let value: Value =
            serde_json::from_slice(raw).map_err(|error| Error(format!("invalid manifest: {error}")))?;
        if !value.is_object() {
            return Err(Error("invalid component manifest".into()));
        }
        let components = value
            .get("components")
            .and_then(Value::as_array)
            .ok_or_else(|| Error("invalid component manifest field: components".into()))?
            .iter()
            .map(|component| {
                let files = component
                    .get("files")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error("invalid component manifest field: files".into()))?
                    .iter()
                    .map(|file| {
                        Ok(PayloadFile {
                            path: string_at(file, "path")?.to_string(),
                            sha256: string_at(file, "sha256")?.to_string(),
                            size: i64_at(file, "size")?,
                            mode: u32_at(file, "mode")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Component {
                    id: string_at(component, "id")?.to_string(),
                    sha256: string_at(component, "sha256")?.to_string(),
                    size: i64_at(component, "size")?,
                    asset: string_at(component, "asset")?.to_string(),
                    files,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            schema: i64_at(&value, "schema")?,
            module: string_at(&value, "module")?.to_string(),
            version: string_at(&value, "version")?.to_string(),
            architecture: string_at(&value, "architecture")?.to_string(),
            repository: string_at(&value, "repository")?.to_string(),
            components,
        })
    }
}

/// Go: main.go:704-707 — an unreadable or malformed previous manifest simply
/// means "no obsolete files are known"; it never fails the install.
pub(super) fn old_managed_files(raw: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(raw) else {
        return Vec::new();
    };
    let Some(components) = value.get("components").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for file in components
        .iter()
        .filter_map(|component| component.get("files"))
        .filter_map(Value::as_array)
        .flatten()
    {
        if let Some(path) = file.get("path").and_then(Value::as_str) {
            if managed_name(path) && !files.iter().any(|seen| seen == path) {
                files.push(path.to_string());
            }
        }
    }
    files
}
