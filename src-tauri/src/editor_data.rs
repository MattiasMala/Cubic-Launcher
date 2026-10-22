use std::path::Path;

use anyhow::{bail, Context, Result};
use tauri::State;

// Editor command surface for mod-list rule data.
//
// This module owns the Tauri commands that mutate the rule tree and editor
// snapshot. Payload structs live in `editor_data_models.rs`, and focused tests
// live in `editor_data_tests.rs`, so new editor behavior should usually extend
// those files instead of growing unrelated modules.
use crate::launcher_paths::LauncherPaths;
use crate::path_safety::{normalize_relative_dir, validate_path_component};
use crate::rules::{
    CustomConfig, ModList, ModSource, Rule, VersionRule, VersionRuleKind, RULES_FILENAME,
};

#[path = "editor_data_models.rs"]
mod models;

#[cfg(test)]
#[path = "editor_data_tests.rs"]
mod tests;

pub use models::*;

#[tauri::command]
pub fn load_modlist_editor_command(
    launcher_paths: State<'_, LauncherPaths>,
    selected_modlist_name: String,
) -> Result<EditorSnapshot, String> {
    load_editor_snapshot_from_root(launcher_paths.root_dir(), &selected_modlist_name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_mod_rule_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: AddModRuleInput,
) -> Result<(), String> {
    add_mod_rule_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_rules_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: DeleteRulesInput,
) -> Result<(), String> {
    delete_rules_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn reorder_rules_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ReorderRulesInput,
) -> Result<(), String> {
    reorder_rules_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_alternative_order_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveAlternativeOrderInput,
) -> Result<(), String> {
    save_alternative_order_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_alternative_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: AddAlternativeInput,
) -> Result<(), String> {
    add_alternative_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_nested_alternative_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: AddNestedAlternativeInput,
) -> Result<(), String> {
    add_nested_alternative_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_alternative_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: RemoveAlternativeInput,
) -> Result<(), String> {
    remove_alternative_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_incompatibilities_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveIncompatibilitiesInput,
) -> Result<(), String> {
    save_incompatibilities_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_rule_advanced_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveRuleAdvancedInput,
) -> Result<(), String> {
    save_rule_advanced_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_advanced_batch_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveAdvancedBatchInput,
) -> Result<(), String> {
    save_advanced_batch_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn import_custom_config_files_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ImportCustomConfigFilesInput,
) -> Result<Vec<String>, String> {
    import_custom_config_files_from_root(launcher_paths.root_dir(), &input)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn toggle_rule_enabled_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ToggleRuleEnabledInput,
) -> Result<(), String> {
    toggle_rule_enabled_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

fn toggle_rule_enabled_from_root(root_dir: &Path, input: &ToggleRuleEnabledInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;
    let rule = modlist
        .find_rule_mut(&input.mod_id)
        .with_context(|| format!("rule '{}' not found", input.mod_id))?;
    set_enabled_recursive(rule, input.enabled);
    save_modlist(root_dir, &input.modlist_name, &modlist)
}

fn set_enabled_recursive(rule: &mut Rule, enabled: bool) {
    rule.enabled = enabled;
    for alt in &mut rule.alternatives {
        set_enabled_recursive(alt, enabled);
    }
}

fn load_modlist(root_dir: &Path, modlist_name: &str) -> Result<ModList> {
    validate_path_component(modlist_name)?;
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let rules_path = launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(RULES_FILENAME);
    ModList::read_from_file(&rules_path).with_context(|| {
        format!(
            "failed to load modlist '{}' from {}",
            modlist_name,
            rules_path.display()
        )
    })
}

fn save_modlist(root_dir: &Path, modlist_name: &str, modlist: &ModList) -> Result<()> {
    validate_path_component(modlist_name)?;
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let rules_path = launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(RULES_FILENAME);
    modlist.write_to_file(&rules_path)?;

    // todo.md:1291: the bytes of a file taken off a custom config, or of a
    // config that is gone, leave `config-files/` with it. The rules are saved
    // by now, so a cleanup that fails is a line in the log, not a failed save.
    let modlist_dir = launcher_paths.modlists_dir().join(modlist_name);
    match crate::instance_configs::prune_orphan_custom_config_files(&modlist_dir, &modlist.rules)
    {
        Ok(removed) => {
            for path in removed {
                eprintln!(
                    "[config-files] removed {}: no custom config lists it any more",
                    path.display()
                );
            }
        }
        Err(error) => eprintln!(
            "[config-files] could not clean up the files of removed custom configs in '{modlist_name}': {error:#}"
        ),
    }
    Ok(())
}

pub fn load_editor_snapshot_from_root(
    root_dir: &Path,
    modlist_name: &str,
) -> Result<EditorSnapshot> {
    let mut modlist = load_modlist(root_dir, modlist_name)?;
    // D130: a `rules.json` written before custom configs had an id gets one
    // here, and it is saved at once, so the next load sees the same id.
    if modlist.assign_missing_custom_config_ids() {
        save_modlist(root_dir, modlist_name, &modlist)?;
    }

    let modlist_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name);
    let rows: Vec<EditorRow> = modlist
        .rules
        .iter()
        .map(|rule| build_editor_row(rule, &modlist_dir))
        .collect();
    let incompatibilities = derive_incompatibilities(&modlist.rules);

    Ok(EditorSnapshot {
        modlist_name: modlist.modlist_name.clone(),
        rows,
        incompatibilities,
    })
}

pub fn add_mod_rule_from_root(root_dir: &Path, input: &AddModRuleInput) -> Result<()> {
    validate_path_component(&input.mod_id)?;
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    if modlist.contains_mod_id(&input.mod_id) {
        bail!(
            "a rule with mod_id '{}' already exists in modlist '{}'",
            input.mod_id,
            input.modlist_name
        );
    }

    let source = parse_mod_source(&input.source)?;

    if source == ModSource::Local {
        if let Some(file_name) = &input.file_name {
            let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
            let local_jars_dir = launcher_paths
                .modlists_dir()
                .join(&input.modlist_name)
                .join("local-jars");
            std::fs::create_dir_all(&local_jars_dir).with_context(|| {
                format!(
                    "failed to create local-jars directory at {}",
                    local_jars_dir.display()
                )
            })?;
            let source_path = Path::new(file_name);
            if source_path.exists() {
                let dest = local_jars_dir.join(format!("{}.jar", input.mod_id));
                std::fs::copy(source_path, &dest)
                    .with_context(|| format!("failed to copy JAR to {}", dest.display()))?;
            }
        }
    }

    modlist.rules.push(Rule {
        mod_id: input.mod_id.clone(),
        source,
        enabled: true,
        exclude_if: vec![],
        requires: vec![],
        version_rules: vec![],
        custom_configs: vec![],
        alternatives: vec![],
    });

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn delete_rules_from_root(root_dir: &Path, input: &DeleteRulesInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let ids_to_remove: std::collections::HashSet<&str> =
        input.mod_ids.iter().map(|s| s.as_str()).collect();
    let mut original_ids = std::collections::HashSet::new();
    collect_tree_ids(&modlist.rules, &mut original_ids);

    modlist
        .rules
        .retain(|r| !ids_to_remove.contains(r.mod_id.as_str()));

    for rule in &mut modlist.rules {
        remove_from_alternatives(rule, &ids_to_remove);
    }

    // Un `requires` o un `exclude_if` verso un mod che non c'è più resta nel
    // file e nessuno lo vede: X requires Y, si cancella Y, e X resta rossa per
    // sempre senza nessun link visibile (audit 101, editor-logica-5). Si
    // tolgono, in tutto l'albero, i riferimenti a ogni id che non c'è più: i
    // mod cancellati e le alternative che spariscono insieme a loro.
    let mut remaining = std::collections::HashSet::new();
    collect_tree_ids(&modlist.rules, &mut remaining);
    let gone = original_ids
        .into_iter()
        .filter(|mod_id| !remaining.contains(mod_id))
        .collect::<std::collections::HashSet<_>>();
    remove_references(&mut modlist.rules, &gone);

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

fn remove_from_alternatives(rule: &mut Rule, ids: &std::collections::HashSet<&str>) {
    rule.alternatives
        .retain(|alt| !ids.contains(alt.mod_id.as_str()));
    for alt in &mut rule.alternatives {
        remove_from_alternatives(alt, ids);
    }
}

fn collect_tree_ids(rules: &[Rule], ids: &mut std::collections::HashSet<String>) {
    for rule in rules {
        ids.insert(rule.mod_id.clone());
        collect_tree_ids(&rule.alternatives, ids);
    }
}

fn remove_references(rules: &mut [Rule], gone: &std::collections::HashSet<String>) {
    for rule in rules {
        rule.requires.retain(|mod_id| !gone.contains(mod_id));
        rule.exclude_if.retain(|mod_id| !gone.contains(mod_id));
        remove_references(&mut rule.alternatives, gone);
    }
}

pub fn reorder_rules_from_root(root_dir: &Path, input: &ReorderRulesInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let mut reordered = Vec::with_capacity(input.ordered_mod_ids.len());
    for mod_id in &input.ordered_mod_ids {
        let pos = modlist
            .rules
            .iter()
            .position(|r| r.mod_id == *mod_id)
            .with_context(|| format!("rule '{}' not found in top-level rules", mod_id))?;
        reordered.push(modlist.rules.remove(pos));
    }

    reordered.append(&mut modlist.rules);
    modlist.rules = reordered;

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn save_alternative_order_from_root(
    root_dir: &Path,
    input: &SaveAlternativeOrderInput,
) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let parent = modlist
        .find_rule_mut(&input.parent_mod_id)
        .with_context(|| format!("parent rule '{}' not found", input.parent_mod_id))?;

    let mut reordered = Vec::with_capacity(input.ordered_alt_ids.len());
    for alt_id in &input.ordered_alt_ids {
        let pos = parent
            .alternatives
            .iter()
            .position(|a| a.mod_id == *alt_id)
            .with_context(|| {
                format!(
                    "alternative '{}' not found under parent '{}'",
                    alt_id, input.parent_mod_id
                )
            })?;
        reordered.push(parent.alternatives.remove(pos));
    }
    reordered.append(&mut parent.alternatives);
    parent.alternatives = reordered;

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn add_alternative_from_root(root_dir: &Path, input: &AddAlternativeInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let rule_to_add = if modlist.contains_mod_id(&input.mod_id) {
        extract_rule_anywhere(&mut modlist, &input.mod_id)?
    } else {
        let source = parse_mod_source(&input.source)?;
        Rule {
            mod_id: input.mod_id.clone(),
            source,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        }
    };

    let parent = modlist
        .rules
        .iter_mut()
        .find(|r| r.mod_id == input.parent_mod_id)
        .with_context(|| format!("top-level rule '{}' not found", input.parent_mod_id))?;

    parent.alternatives.push(rule_to_add);

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn add_nested_alternative_from_root(
    root_dir: &Path,
    input: &AddNestedAlternativeInput,
) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let rule_to_add = if modlist.contains_mod_id(&input.mod_id) {
        extract_rule_anywhere(&mut modlist, &input.mod_id)?
    } else {
        let source = parse_mod_source(&input.source)?;
        Rule {
            mod_id: input.mod_id.clone(),
            source,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        }
    };

    let parent = modlist
        .find_rule_mut(&input.parent_mod_id)
        .with_context(|| format!("rule '{}' not found", input.parent_mod_id))?;

    parent.alternatives.push(rule_to_add);

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn remove_alternative_from_root(root_dir: &Path, input: &RemoveAlternativeInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let extracted = extract_alternative(&mut modlist, &input.parent_mod_id, &input.alt_mod_id)?;

    if let Some(pos) = modlist
        .rules
        .iter()
        .position(|r| r.mod_id == input.parent_mod_id)
    {
        modlist.rules.insert(pos + 1, extracted);
    } else {
        insert_as_sibling(&mut modlist.rules, &input.parent_mod_id, extracted)?;
    }

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

fn extract_rule_anywhere(modlist: &mut ModList, mod_id: &str) -> Result<Rule> {
    if let Some(pos) = modlist.rules.iter().position(|r| r.mod_id == mod_id) {
        return Ok(modlist.rules.remove(pos));
    }
    for rule in &mut modlist.rules {
        if let Some(extracted) = extract_from_alternatives(rule, mod_id) {
            return Ok(extracted);
        }
    }
    bail!("rule '{}' not found anywhere in the modlist", mod_id)
}

fn extract_from_alternatives(rule: &mut Rule, mod_id: &str) -> Option<Rule> {
    if let Some(pos) = rule.alternatives.iter().position(|a| a.mod_id == mod_id) {
        return Some(rule.alternatives.remove(pos));
    }
    for alt in &mut rule.alternatives {
        if let Some(extracted) = extract_from_alternatives(alt, mod_id) {
            return Some(extracted);
        }
    }
    None
}

fn extract_alternative(
    modlist: &mut ModList,
    parent_mod_id: &str,
    alt_mod_id: &str,
) -> Result<Rule> {
    let parent = modlist
        .find_rule_mut(parent_mod_id)
        .with_context(|| format!("rule '{}' not found", parent_mod_id))?;

    let pos = parent
        .alternatives
        .iter()
        .position(|alt| alt.mod_id == alt_mod_id)
        .with_context(|| {
            format!(
                "alternative '{}' not found under rule '{}'",
                alt_mod_id, parent_mod_id
            )
        })?;

    Ok(parent.alternatives.remove(pos))
}

fn insert_as_sibling(rules: &mut [Rule], sibling_mod_id: &str, new_rule: Rule) -> Result<()> {
    for rule in rules.iter_mut() {
        if let Some(pos) = rule
            .alternatives
            .iter()
            .position(|a| a.mod_id == sibling_mod_id)
        {
            rule.alternatives.insert(pos + 1, new_rule);
            return Ok(());
        }
        if insert_as_sibling(&mut rule.alternatives, sibling_mod_id, new_rule.clone()).is_ok() {
            return Ok(());
        }
    }
    bail!("could not find parent of '{}'", sibling_mod_id)
}

pub fn save_incompatibilities_from_root(
    root_dir: &Path,
    input: &SaveIncompatibilitiesInput,
) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    for incompat in &input.rules {
        if modlist.find_rule(&incompat.loser_id).is_none() {
            bail!(
                "unknown loser rule id in incompatibilities batch: {}",
                incompat.loser_id
            );
        }
    }

    for rule in &mut modlist.rules {
        clear_exclude_if_tree(rule);
    }

    for incompat in &input.rules {
        if let Some(loser_rule) = modlist.find_rule_mut(&incompat.loser_id) {
            if !loser_rule.exclude_if.contains(&incompat.winner_id) {
                loser_rule.exclude_if.push(incompat.winner_id.clone());
            }
        }
    }

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

fn clear_exclude_if_tree(rule: &mut Rule) {
    rule.exclude_if.clear();
    for alt in &mut rule.alternatives {
        clear_exclude_if_tree(alt);
    }
}

pub fn save_rule_advanced_from_root(root_dir: &Path, input: &SaveRuleAdvancedInput) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    let rule = modlist
        .find_rule_mut(&input.mod_id)
        .with_context(|| format!("rule '{}' not found", input.mod_id))?;

    rule.exclude_if = input.exclude_if.clone();

    rule.requires = input.requires.clone();
    rule.version_rules = input
        .version_rules
        .iter()
        .map(|vr| VersionRule {
            kind: match vr.kind.as_str() {
                "only" => VersionRuleKind::Only,
                _ => VersionRuleKind::Exclude,
            },
            mc_versions: vr.mc_versions.clone(),
            loader: vr.loader.clone(),
        })
        .collect();
    rule.custom_configs = input
        .custom_configs
        .iter()
        .map(custom_config_from_input)
        .collect::<Result<_>>()?;

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

pub fn save_advanced_batch_from_root(
    root_dir: &Path,
    input: &SaveAdvancedBatchInput,
) -> Result<()> {
    let mut modlist = load_modlist(root_dir, &input.modlist_name)?;

    for mod_id in input
        .requires_entries
        .iter()
        .map(|entry| entry.mod_id.as_str())
        .chain(
            input
                .version_rules_entries
                .iter()
                .map(|entry| entry.mod_id.as_str()),
        )
        .chain(
            input
                .custom_configs_entries
                .iter()
                .map(|entry| entry.mod_id.as_str()),
        )
    {
        if modlist.find_rule(mod_id).is_none() {
            bail!("unknown rule id in advanced batch: {}", mod_id);
        }
    }

    fn clear_advanced(rule: &mut Rule) {
        rule.requires.clear();
        rule.version_rules.clear();
        rule.custom_configs.clear();
        for alt in &mut rule.alternatives {
            clear_advanced(alt);
        }
    }
    for rule in &mut modlist.rules {
        clear_advanced(rule);
    }

    for entry in &input.requires_entries {
        if let Some(rule) = modlist.find_rule_mut(&entry.mod_id) {
            rule.requires = entry.requires.clone();
        }
    }

    for entry in &input.version_rules_entries {
        if let Some(rule) = modlist.find_rule_mut(&entry.mod_id) {
            rule.version_rules = entry
                .version_rules
                .iter()
                .map(|vr| VersionRule {
                    kind: match vr.kind.as_str() {
                        "only" => VersionRuleKind::Only,
                        _ => VersionRuleKind::Exclude,
                    },
                    mc_versions: vr.mc_versions.clone(),
                    loader: vr.loader.clone(),
                })
                .collect();
        }
    }

    for entry in &input.custom_configs_entries {
        if let Some(rule) = modlist.find_rule_mut(&entry.mod_id) {
            rule.custom_configs = entry
                .custom_configs
                .iter()
                .map(custom_config_from_input)
                .collect::<Result<_>>()?;
        }
    }

    save_modlist(root_dir, &input.modlist_name, &modlist)
}

/// The saved custom config keeps the id the frontend sends (D130). An empty
/// one gets a fresh id; one that is not a single path component is refused,
/// because it becomes the directory of the copied files.
fn custom_config_from_input(input: &SaveCustomConfigInput) -> Result<CustomConfig> {
    let id = if input.id.is_empty() {
        crate::rules::new_custom_config_id()
    } else {
        validate_path_component(&input.id)
            .with_context(|| format!("invalid custom config id '{}'", input.id))?;
        input.id.clone()
    };
    // `modlist-10`: the «Path» is checked here, where the user can still fix
    // it, and stored in one form (`/`, no `.`), so a launch never meets one
    // that leaves the instance.
    let target_path = normalize_relative_dir(&input.target_path).with_context(|| {
        format!(
            "the Path '{}' of a custom config must be a folder inside the instance, like 'config' or 'config/create'",
            input.target_path
        )
    })?;
    Ok(CustomConfig {
        id,
        mc_versions: input.mc_versions.clone(),
        loader: input.loader.clone(),
        target_path,
        files: input.files.clone(),
    })
}

/// Copy the files picked for a custom config into the mod list (D112) and
/// return the names to list in its `files`, one per distinct name.
///
/// Same job as the jar of a local mod in [`add_mod_rule_from_root`]: the mod
/// list directory, a plain `fs::copy`, and the source is only read — never
/// moved, never removed. The destination is
/// [`custom_config_files_dir`](crate::rules::custom_config_files_dir).
///
/// A name never starts pointing at other bytes in silence. Every file of a
/// custom config lands in the instance under its own name, so two of them
/// cannot share one: two picked files with one name and different bytes, or a
/// picked file whose name the config already lists with different bytes,
/// refuse the whole pick before anything is copied, naming the file. The same
/// bytes again are not a conflict.
pub fn import_custom_config_files_from_root(
    root_dir: &Path,
    input: &ImportCustomConfigFilesInput,
) -> Result<Vec<String>> {
    let modlist = load_modlist(root_dir, &input.modlist_name)?;
    if !modlist.contains_mod_id(&input.mod_id) {
        bail!(
            "rule '{}' not found in modlist '{}'",
            input.mod_id,
            input.modlist_name
        );
    }
    let modlist_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(&input.modlist_name);
    let files_dir =
        crate::rules::custom_config_files_dir(&modlist_dir, &input.mod_id, &input.config_id)?;

    let mut picked: Vec<(String, &Path, Vec<u8>)> = Vec::new();
    for source in &input.source_paths {
        let source_path = Path::new(source);
        let name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("'{source}' has no usable file name"))?;
        validate_path_component(name)?;
        let bytes = std::fs::read(source_path)
            .with_context(|| format!("failed to read {}", source_path.display()))?;

        if let Some((_, first, first_bytes)) = picked.iter().find(|(seen, _, _)| seen == name) {
            if *first_bytes != bytes {
                bail!(
                    "two different files named '{name}' were picked ({} and {}): \
                     a custom config places each file under its own name, so pick one",
                    first.display(),
                    source_path.display()
                );
            }
            continue;
        }
        if input.current_files.iter().any(|listed| listed == name) {
            if let Ok(stored) = std::fs::read(files_dir.join(name)) {
                if stored != bytes {
                    bail!(
                        "this custom config already has a different file named '{name}': \
                         remove it from the list first to replace it"
                    );
                }
            }
        }
        picked.push((name.to_string(), source_path, bytes));
    }

    std::fs::create_dir_all(&files_dir)
        .with_context(|| format!("failed to create {}", files_dir.display()))?;
    let mut names = Vec::with_capacity(picked.len());
    for (name, source_path, _) in picked {
        let dest = files_dir.join(&name);
        std::fs::copy(source_path, &dest).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.display(),
                dest.display()
            )
        })?;
        names.push(name);
    }
    Ok(names)
}

/// One row of the editor snapshot.
///
/// `modlist_dir` is only there for the icon of a local mod: it is read from the
/// jar the first time and from `.cubic/icons/local-jars/` afterwards
/// ([`crate::mod_icons`]), which is why building a row can touch the disk.
fn build_editor_row(rule: &Rule, modlist_dir: &Path) -> EditorRow {
    EditorRow {
        mod_id: rule.mod_id.clone(),
        name: rule.mod_id.clone(),
        source: match &rule.source {
            ModSource::Modrinth => "modrinth".into(),
            ModSource::Local => "local".into(),
        },
        icon_image: match rule.source {
            ModSource::Local => {
                crate::mod_icons::local_mod_icon_data_url(modlist_dir, &rule.mod_id)
            }
            ModSource::Modrinth => None,
        },
        pin: crate::mod_version_pin::pin_kind(rule, modlist_dir).map(|kind| EditorPin {
            owns_jar: kind == crate::mod_version_pin::PinKind::Marked,
        }),
        enabled: rule.enabled,
        exclude_if: rule.exclude_if.clone(),
        requires: rule.requires.clone(),
        version_rules: rule
            .version_rules
            .iter()
            .map(|vr| EditorVersionRule {
                kind: match vr.kind {
                    VersionRuleKind::Exclude => "exclude".into(),
                    VersionRuleKind::Only => "only".into(),
                },
                mc_versions: vr.mc_versions.clone(),
                loader: vr.loader.clone(),
            })
            .collect(),
        custom_configs: rule
            .custom_configs
            .iter()
            .map(|cc| EditorCustomConfig {
                id: cc.id.clone(),
                mc_versions: cc.mc_versions.clone(),
                loader: cc.loader.clone(),
                target_path: cc.target_path.clone(),
                files: cc.files.clone(),
            })
            .collect(),
        alternatives: rule
            .alternatives
            .iter()
            .map(|alt| build_editor_row(alt, modlist_dir))
            .collect(),
    }
}

fn derive_incompatibilities(rules: &[Rule]) -> Vec<IncompatibilityEntry> {
    let mut entries = Vec::new();
    for rule in rules {
        collect_incompatibilities_from_tree(rule, &mut entries);
    }
    entries
}

fn collect_incompatibilities_from_tree(rule: &Rule, entries: &mut Vec<IncompatibilityEntry>) {
    for excl_id in &rule.exclude_if {
        entries.push(IncompatibilityEntry {
            winner_id: excl_id.clone(),
            loser_id: rule.mod_id.clone(),
        });
    }
    for alt in &rule.alternatives {
        collect_incompatibilities_from_tree(alt, entries);
    }
}

fn parse_mod_source(source: &str) -> Result<ModSource> {
    match source {
        "modrinth" => Ok(ModSource::Modrinth),
        "local" => Ok(ModSource::Local),
        other => bail!("unknown mod source '{}'", other),
    }
}
