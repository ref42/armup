use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::state::{legacy_tool_versions_dir, tool_versions_dir};
use crate::tool::ToolKind;
use crate::types::{EnvPlan, EnvPreview, InstalledTool};

const MANAGED_ENV_VARS: &[&str] = &[
    "ARMUP_HOME",
    "ARM_GNU_TOOLCHAIN_ROOT",
    "CLANGD_ROOT",
    "CMAKE_ROOT",
    "NINJA_ROOT",
    "OPENOCD_ROOT",
    "OPENOCD_SCRIPTS",
    "PROBE_RS_ROOT",
];

pub fn build_env_plan(_root: &Path, tools: &[InstalledTool]) -> Result<EnvPlan> {
    let mut path_entries = Vec::new();

    for tool in tools {
        path_entries.push(tool.executable_dir.clone());
    }

    Ok(EnvPlan {
        path_entries: dedup_paths(path_entries),
    })
}

pub fn apply_user_environment(root: &Path, plan: &EnvPlan) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (environment, _) = hkcu
        .create_subkey("Environment")
        .context("failed to open HKCU\\Environment")?;

    remove_managed_environment_variables(&environment)?;

    let existing_path: String = environment.get_value("Path").unwrap_or_default();
    let merged_path = merge_user_path(root, &existing_path, &plan.path_entries);
    environment
        .set_value("Path", &merged_path)
        .context("failed to update HKCU\\Environment\\Path")?;

    Ok(())
}

pub fn preview_user_environment(root: &Path, plan: &EnvPlan) -> Result<EnvPreview> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (environment, _) = hkcu
        .create_subkey("Environment")
        .context("failed to open HKCU\\Environment")?;

    let existing_path: String = environment.get_value("Path").unwrap_or_default();
    let analysis = analyze_path_merge(root, &existing_path, &plan.path_entries);
    let legacy_variables_present = MANAGED_ENV_VARS
        .iter()
        .filter(|key| environment.get_raw_value(*key).is_ok())
        .map(|key| (*key).to_string())
        .collect();

    Ok(EnvPreview {
        removed_path_entries: analysis.removed_path_entries,
        added_path_entries: analysis.added_path_entries,
        final_path_entry_count: analysis.final_entries.len(),
        legacy_variables_present,
    })
}

fn remove_managed_environment_variables(environment: &RegKey) -> Result<()> {
    for key in MANAGED_ENV_VARS {
        match environment.delete_value(key) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove {key} from HKCU\\Environment"));
            }
        }
    }

    Ok(())
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        let key = normalize_path_key(&path);
        if seen.insert(key) {
            deduped.push(path);
        }
    }
    deduped
}

fn merge_user_path(root: &Path, current_path: &str, additions: &[PathBuf]) -> String {
    analyze_path_merge(root, current_path, additions)
        .final_entries
        .join(";")
}

struct PathMergeAnalysis {
    final_entries: Vec<String>,
    removed_path_entries: Vec<String>,
    added_path_entries: Vec<PathBuf>,
}

fn analyze_path_merge(root: &Path, current_path: &str, additions: &[PathBuf]) -> PathMergeAnalysis {
    let managed_roots: Vec<String> = ToolKind::all()
        .into_iter()
        .flat_map(|kind| {
            [
                tool_versions_dir(root, kind),
                legacy_tool_versions_dir(root, kind),
            ]
        })
        .flat_map(|path| managed_root_keys(&path))
        .collect();
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut removed_path_entries = Vec::new();
    let mut added_path_entries = Vec::new();

    for raw in current_path.split(';') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        let normalized = normalize_string_key(trimmed);
        if managed_roots
            .iter()
            .any(|managed| path_is_under_managed_root(&normalized, managed))
        {
            removed_path_entries.push(trimmed.to_string());
            continue;
        }

        if seen.insert(normalized) {
            entries.push(trimmed.to_string());
        }
    }

    for addition in additions {
        let rendered = addition.display().to_string();
        let normalized = normalize_path_key(addition);
        if seen.insert(normalized) {
            added_path_entries.push(addition.clone());
            entries.push(rendered);
        }
    }

    PathMergeAnalysis {
        final_entries: entries,
        removed_path_entries,
        added_path_entries,
    }
}

fn path_is_under_managed_root(path: &str, managed_root: &str) -> bool {
    path == managed_root
        || path
            .strip_prefix(managed_root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn managed_root_keys(path: &Path) -> Vec<String> {
    let normalized = normalize_path_key(path);
    let mut keys = vec![normalized.clone()];

    if normalized.len() >= 3 {
        let bytes = normalized.as_bytes();
        if bytes[1] == b':' && bytes[2] == b'\\' && bytes[0].is_ascii_alphabetic() {
            keys.push(format!("{}{}", &normalized[..2], &normalized[3..]));
        }
    }

    keys
}

fn normalize_path_key(path: &Path) -> String {
    normalize_string_key(&path.to_string_lossy())
}

fn normalize_string_key(value: &str) -> String {
    value
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{merge_user_path, path_is_under_managed_root};
    use std::path::PathBuf;

    #[test]
    fn managed_root_matching_requires_component_boundary() {
        assert!(path_is_under_managed_root(
            r"d:\embedded_toolchain\ninja\1.12.0",
            r"d:\embedded_toolchain\ninja"
        ));
        assert!(!path_is_under_managed_root(
            r"d:\embedded_toolchain\ninja2\bin",
            r"d:\embedded_toolchain\ninja"
        ));
    }

    #[test]
    fn merge_user_path_removes_old_managed_entries_and_adds_current_entries() {
        let root = PathBuf::from(r"D:\Embedded_Toolchain");
        let current =
            r"C:\Windows;D:\Embedded_Toolchain\ninja\1.11.0;D:\Embedded_Toolchain\ninja2\bin";
        let additions = vec![PathBuf::from(r"D:\Embedded_Toolchain\ninja\1.12.0")];

        let merged = merge_user_path(&root, current, &additions);

        assert_eq!(
            merged,
            r"C:\Windows;D:\Embedded_Toolchain\ninja2\bin;D:\Embedded_Toolchain\ninja\1.12.0"
        );
    }

    #[test]
    fn merge_user_path_removes_drive_relative_managed_entries() {
        let root = PathBuf::from(r"D:\Embedded_Toolchain");
        let current = r"C:\Windows;D:Embedded_Toolchain\ninja\1.13.2";
        let additions = vec![PathBuf::from(r"D:\Embedded_Toolchain\ninja\1.13.2")];

        let merged = merge_user_path(&root, current, &additions);

        assert_eq!(merged, r"C:\Windows;D:\Embedded_Toolchain\ninja\1.13.2");
    }
}
