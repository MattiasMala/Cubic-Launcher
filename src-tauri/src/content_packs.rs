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

/// One pack in a category list.
///
/// Every field added after the first release is `#[serde(default)]` and
/// `skip_serializing_if`: a `resourcepacks.json` written before those fields
/// existed keeps loading, and rewriting it does not grow null keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentEntry {
    pub id: String,
    pub source: String, // "modrinth" or "local"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_rules: Vec<VersionRule>,
    /// Display name for entries the launcher cannot look up remotely. Modrinth
    /// entries leave it empty and keep taking their title from the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Name of the pack inside the mod-list category directory
    /// (`mod-lists/<name>/resourcepacks/`, `datapacks/`, `shaders/`): the file
    /// for a zipped pack, the directory for an unpacked one. This is what the
    /// launcher links into the instance, and it is deliberately not `id`: the
    /// id is identity, this is location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Where the extracted `pack.png` lives, **relative to the mod-list
    /// directory** (`.cubic/icons/<category>/<file_name>.png`). Absent when the
    /// pack ships no `pack.png`, which is normal and not an error.
    ///
    /// The bytes are deliberately not stored here: this file is parsed on every
    /// launch, and inlining base64 icons took the real mod list's
    /// `resourcepacks.json` from 411 bytes to 372 KB for four packs. The
    /// frontend still gets a ready `data:` URL — [`entry_snapshot`] builds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
    /// `pack.mcmeta`'s `pack.description`, but only when it is a plain string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    pub name: Option<String>,
    pub file_name: Option<String>,
    pub icon_image: Option<String>,
    pub description: Option<String>,
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

/// The frontend-facing view of one entry. Shared by `load_content_list_command`
/// and by the local-pack import, which returns the row it just created so the
/// caller can insert it without reloading the whole list.
///
/// `icon_path` becomes `iconImage`, a `data:image/png;base64,…` the caller can
/// put straight into an `<img>`: the icon is read here, off the launch path, and
/// a missing or unreadable icon file simply yields no icon.
pub fn entry_snapshot(entry: &ContentEntry, modlist_dir: &Path) -> ContentEntrySnapshot {
    ContentEntrySnapshot {
        id: entry.id.clone(),
        source: entry.source.clone(),
        version_rules: entry
            .version_rules
            .iter()
            .map(|vr| VersionRuleSnapshot {
                kind: match vr.kind {
                    crate::rules::VersionRuleKind::Exclude => "exclude".to_string(),
                    crate::rules::VersionRuleKind::Only => "only".to_string(),
                },
                mc_versions: vr.mc_versions.clone(),
                loader: vr.loader.clone(),
            })
            .collect(),
        name: entry.name.clone(),
        file_name: entry.file_name.clone(),
        icon_image: entry
            .icon_path
            .as_deref()
            .and_then(|path| crate::local_content_packs::read_icon_data_url(modlist_dir, path)),
        description: entry.description.clone(),
    }
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
            .map(|entry| entry_snapshot(entry, &dir))
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
        name: None,
        file_name: None,
        icon_path: None,
        description: None,
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
