//! Manages non-mod content: resource packs, data packs, and shaders.
//! Each content type is stored in its own JSON file within the modlist directory.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;
use crate::rules::VersionRule;

// ── Schema ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentList {
    pub content_type: String,
    #[serde(default)]
    pub entries: Vec<ContentEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ContentGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub entry_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentEntry {
    pub id: String,
    pub source: String, // "modrinth" or "local"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_rules: Vec<VersionRule>,
}

// ── File names ──────────────────────────────────────────────────────────────

pub fn filename_for_type(content_type: &str) -> &'static str {
    match content_type {
        "resourcepack" => "resourcepacks.json",
        "datapack" => "datapacks.json",
        "shader" => "shaders.json",
        _ => "unknown_content.json",
    }
}

// ── Read / Write ────────────────────────────────────────────────────────────

pub fn load_content_list(modlist_dir: &Path, content_type: &str) -> Result<ContentList> {
    let path = modlist_dir.join(filename_for_type(content_type));
    if !path.exists() {
        return Ok(ContentList {
            content_type: content_type.to_string(),
            entries: vec![],
            groups: vec![],
        });
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save_content_list(modlist_dir: &Path, list: &ContentList) -> Result<()> {
    let path = modlist_dir.join(filename_for_type(&list.content_type));
    let json = serde_json::to_string_pretty(list).context("failed to serialize content list")?;
    fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

// ── Tauri commands ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadContentInput {
    pub modlist_name: String,
    pub content_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddContentInput {
    pub modlist_name: String,
    pub content_type: String,
    pub id: String,
    pub source: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveContentInput {
    pub modlist_name: String,
    pub content_type: String,
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderContentInput {
    pub modlist_name: String,
    pub content_type: String,
    pub ordered_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveContentGroupsInput {
    pub modlist_name: String,
    pub content_type: String,
    pub groups: Vec<ContentGroupSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentGroupSnapshot {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub entry_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSnapshot {
    pub content_type: String,
    pub entries: Vec<ContentEntrySnapshot>,
    pub groups: Vec<ContentGroupSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveContentVersionRulesInput {
    pub modlist_name: String,
    pub content_type: String,
    pub entry_id: String,
    pub version_rules: Vec<VersionRule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentEntrySnapshot {
    pub id: String,
    pub source: String,
    pub version_rules: Vec<VersionRuleSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionRuleSnapshot {
    pub kind: String,
    pub mc_versions: Vec<String>,
    pub loader: String,
}

fn validate_content_modlist_name(name: &str) -> Result<(), String> {
    validate_path_component(name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_content_list_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: LoadContentInput,
) -> Result<ContentSnapshot, String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;
    Ok(ContentSnapshot {
        content_type: list.content_type,
        entries: list
            .entries
            .iter()
            .map(|e| ContentEntrySnapshot {
                id: e.id.clone(),
                source: e.source.clone(),
                version_rules: e
                    .version_rules
                    .iter()
                    .map(|vr| {
                        let kind_str = match vr.kind {
                            crate::rules::VersionRuleKind::Exclude => "exclude",
                            crate::rules::VersionRuleKind::Only => "only",
                        };
                        VersionRuleSnapshot {
                            kind: kind_str.to_string(),
                            mc_versions: vr.mc_versions.clone(),
                            loader: vr.loader.clone(),
                        }
                    })
                    .collect(),
            })
            .collect(),
        groups: list
            .groups
            .iter()
            .map(|g| ContentGroupSnapshot {
                id: g.id.clone(),
                name: g.name.clone(),
                collapsed: g.collapsed,
                entry_ids: g.entry_ids.clone(),
            })
            .collect(),
    })
}

#[tauri::command]
pub fn add_content_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: AddContentInput,
) -> Result<(), String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let mut list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;

    if list.entries.iter().any(|e| e.id == input.id) {
        return Ok(()); // already exists
    }

    list.entries.push(ContentEntry {
        id: input.id,
        source: input.source,
        version_rules: vec![],
    });
    save_content_list(&dir, &list).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_content_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: RemoveContentInput,
) -> Result<(), String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let mut list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;
    list.entries.retain(|e| e.id != input.id);
    // Also remove from any groups
    for g in &mut list.groups {
        g.entry_ids.retain(|eid| eid != &input.id);
    }
    list.groups.retain(|g| !g.entry_ids.is_empty());
    save_content_list(&dir, &list).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn reorder_content_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ReorderContentInput,
) -> Result<(), String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let mut list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;

    // Reorder entries according to ordered_ids
    let mut reordered = Vec::with_capacity(list.entries.len());
    for id in &input.ordered_ids {
        if let Some(pos) = list.entries.iter().position(|e| &e.id == id) {
            reordered.push(list.entries.remove(pos));
        }
    }
    // Append any entries not in the ordered list (safety net)
    reordered.append(&mut list.entries);
    list.entries = reordered;

    save_content_list(&dir, &list).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_content_groups_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveContentGroupsInput,
) -> Result<(), String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let mut list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;

    list.groups = input
        .groups
        .into_iter()
        .map(|g| ContentGroup {
            id: g.id,
            name: g.name,
            collapsed: g.collapsed,
            entry_ids: g.entry_ids,
        })
        .collect();

    save_content_list(&dir, &list).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_content_version_rules_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveContentVersionRulesInput,
) -> Result<(), String> {
    validate_content_modlist_name(&input.modlist_name)?;
    let dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    let mut list = load_content_list(&dir, &input.content_type).map_err(|e| e.to_string())?;

    if let Some(entry) = list.entries.iter_mut().find(|e| e.id == input.entry_id) {
        entry.version_rules = input.version_rules;
    }

    save_content_list(&dir, &list).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::validate_content_modlist_name;

    #[test]
    fn content_command_rejects_traversal_modlist_name() {
        assert!(validate_content_modlist_name("../x").is_err());
        assert!(validate_content_modlist_name("nested/x").is_err());
        assert!(validate_content_modlist_name(r"nested\x").is_err());
        assert!(validate_content_modlist_name("/absolute").is_err());
        assert!(validate_content_modlist_name("safe modlist").is_ok());
    }
}
