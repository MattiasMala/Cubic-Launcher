use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::editor_data::{add_mod_rule_from_root, AddModRuleInput};
use crate::launcher_paths::LauncherPaths;
use crate::mod_icons::LOCAL_JARS_DIR;
use crate::path_safety::{contained_join, validate_path_component};
use crate::rules::{ModList, ModSource, RULES_FILENAME};
use std::io;

// ---------------------------------------------------------------------------
// Create Mod-list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateModlistInput {
    pub name: String,
    pub author: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateModlistResult {
    pub name: String,
    pub author: String,
    pub description: String,
}

#[tauri::command]
pub fn create_modlist_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: CreateModlistInput,
) -> Result<CreateModlistResult, String> {
    create_modlist_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

pub fn create_modlist_from_root(
    root_dir: &Path,
    input: &CreateModlistInput,
) -> Result<CreateModlistResult> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        bail!("mod-list name cannot be empty");
    }
    validate_path_component(&name)?;

    let author = if input.author.trim().is_empty() {
        "Author".to_string()
    } else {
        input.author.trim().to_string()
    };

    let description = input.description.trim().to_string();

    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlist_dir = launcher_paths.modlists_dir().join(&name);
    let rules_path = modlist_dir.join(RULES_FILENAME);

    if rules_path.exists() {
        bail!("a mod-list named '{}' already exists", name);
    }

    // Create subdirectories
    std::fs::create_dir_all(modlist_dir.join("local-jars")).with_context(|| {
        format!(
            "failed to create local-jars directory for modlist '{}'",
            name
        )
    })?;

    let modlist = ModList {
        modlist_name: name.clone(),
        author: author.clone(),
        description: description.clone(),
        rules: vec![],
    };

    modlist.write_to_file(&rules_path).with_context(|| {
        format!(
            "failed to create mod-list '{}' at {}",
            name,
            rules_path.display()
        )
    })?;

    // Il globale è il modello per una modlist nuova (D66). Se non c'è, o se
    // non si può filtrare in sicurezza, la modlist nasce senza `options.txt` e
    // nessuno se ne accorge finché non la apre: la creazione non fallisce per
    // questo, ma la ragione resta scritta nel log.
    let seed = crate::options_share::seed_modlist_from_global(&launcher_paths, &name);
    if let crate::options_share::SeedStatus::Refused { .. } = &seed {
        eprintln!("[options] {}", seed.describe());
    }

    Ok(CreateModlistResult {
        name,
        author,
        description,
    })
}

// ---------------------------------------------------------------------------
// Delete Mod-list
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn delete_modlist_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
) -> Result<(), String> {
    delete_modlist_from_root(launcher_paths.root_dir(), &modlist_name).map_err(|e| e.to_string())
}

pub fn delete_modlist_from_root(root_dir: &Path, modlist_name: &str) -> Result<()> {
    let name = modlist_name.trim();
    if name.is_empty() {
        bail!("mod-list name cannot be empty");
    }
    validate_path_component(name)?;

    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlist_dir = launcher_paths.modlists_dir().join(name);

    if !modlist_dir.exists() {
        bail!("mod-list '{}' does not exist", name);
    }

    std::fs::remove_dir_all(&modlist_dir).with_context(|| {
        format!(
            "failed to delete mod-list '{}' at {}",
            name,
            modlist_dir.display()
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Copy Local JAR
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyLocalJarInput {
    pub source_path: String,
    pub modlist_name: String,
}

#[tauri::command]
pub fn import_modlist_command(
    launcher_paths: State<'_, LauncherPaths>,
    source_path: String,
) -> Result<String, String> {
    import_modlist_from_root(launcher_paths.root_dir(), &source_path).map_err(|e| e.to_string())
}

/// Pick a unique name: if `base` already exists, try `base (1)`, `base (2)`, etc.
fn unique_modlist_name(modlists_dir: &Path, base: &str) -> String {
    if !modlists_dir.join(base).exists() {
        return base.to_string();
    }
    for n in 1.. {
        let candidate = format!("{base} ({n})");
        if !modlists_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// The start of the name of the folder an import extracts into, inside
/// `mod-lists/`. Hidden behind the dot, and never a mod list: the shell
/// snapshot skips it ([`is_import_staging_dir`]), so an import cut short —
/// the launcher killed in the middle — does not show up in the sidebar.
pub const IMPORT_STAGING_PREFIX: &str = ".cubic-import-";

/// Whether a folder of `mod-lists/` is an import still in progress, or one
/// cut short, rather than a mod list.
pub fn is_import_staging_dir(name: &str) -> bool {
    name.starts_with(IMPORT_STAGING_PREFIX)
}

/// The staging folder of one import: removed when dropped, unless the import
/// got to the end and moved it into place. Every `?` of the import is a
/// cleanup, not a half mod list (`modlist-3`).
struct StagingDir {
    path: std::path::PathBuf,
    keep: bool,
}

impl StagingDir {
    fn create(modlists_dir: &Path) -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = modlists_dir.join(format!(
            "{IMPORT_STAGING_PREFIX}{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self { path, keep: false })
    }

    /// Move the extracted mod list to `destination`: the only step that makes
    /// it visible.
    fn finish(mut self, destination: &Path) -> Result<()> {
        if destination.exists() {
            bail!(
                "a mod list folder appeared at {} during the import",
                destination.display()
            );
        }
        std::fs::rename(&self.path, destination).with_context(|| {
            format!(
                "failed to move the imported mod list to {}",
                destination.display()
            )
        })?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep {
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                eprintln!(
                    "[import] could not remove the unfinished import at {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

/// How the entries of an exported zip are laid out. Cubic's export wraps
/// everything in one folder named after the mod list; a zip made by hand may
/// not (`sicurezza-4`): then nothing is stripped, and the name comes from the
/// archive's file name, never from whatever folder happens to come first.
fn archive_root_folder(names: &[String]) -> Option<String> {
    let mut root: Option<&str> = None;
    for name in names {
        let (first, rest) = name.split_once('/')?;
        if first.is_empty() || rest.is_empty() {
            return None;
        }
        match root {
            None => root = Some(first),
            Some(known) if known == first => {}
            Some(_) => return None,
        }
    }
    root.map(str::to_string)
}

/// The `mod_id`s of the local mods anywhere in the tree.
fn local_mod_ids(rules: &[crate::rules::Rule], out: &mut HashSet<String>) {
    for rule in rules {
        if rule.source == ModSource::Local {
            out.insert(rule.mod_id.clone());
        }
        local_mod_ids(&rule.alternatives, out);
    }
}

/// `r2-aggiornamento-da-0.1.2-2`: a zip exported by v0.1.x with «Mod JAR
/// files from cache» carries the jar of a local mod only as
/// `cache/mods/<loader>/local/<mod_id>.jar`, and the mod list's own
/// `local-jars/` not at all. That jar is the only copy the recipient will ever
/// get, so it is kept, as `local-jars/<mod_id>.jar`; every other file of the
/// old global cache — the jars that can be downloaded again, and the local
/// jars of other mod lists — is still left out.
fn legacy_local_jar_name<'a>(relative: &'a str, local_ids: &HashSet<String>) -> Option<&'a str> {
    let parts: Vec<&str> = relative.split('/').collect();
    let ["cache", "mods", _loader, "local", file_name] = parts.as_slice() else {
        return None;
    };
    let mod_id = file_name.strip_suffix(".jar")?;
    if !local_ids.contains(mod_id) || validate_path_component(file_name).is_err() {
        return None;
    }
    Some(file_name)
}

fn import_modlist_from_root(root_dir: &Path, source_path: &str) -> Result<String> {
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlists_dir = launcher_paths.modlists_dir();
    std::fs::create_dir_all(modlists_dir)
        .with_context(|| format!("failed to create {}", modlists_dir.display()))?;

    let source = Path::new(source_path);
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let file_stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported")
        .to_string();

    if ext == "zip" {
        let file = std::fs::File::open(source)
            .with_context(|| format!("failed to open {}", source.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read zip archive {}", source.display()))?;

        let mut file_names = Vec::new();
        for i in 0..archive.len() {
            let entry = archive.by_index(i)?;
            if !entry.is_dir() {
                file_names.push(entry.name().to_string());
            }
        }
        let archive_root = archive_root_folder(&file_names);
        let relative_of = |raw_name: &str| -> String {
            match &archive_root {
                Some(root) => raw_name
                    .strip_prefix(root.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .unwrap_or(raw_name)
                    .to_string(),
                None => raw_name.to_string(),
            }
        };

        let base_name = archive_root.clone().unwrap_or(file_stem);
        validate_path_component(&base_name)?;
        if is_import_staging_dir(&base_name) {
            bail!("'{base_name}' cannot be the name of a mod list");
        }
        let rules_index = file_names
            .iter()
            .position(|name| relative_of(name) == RULES_FILENAME)
            .with_context(|| {
                format!(
                    "{} has no {RULES_FILENAME}: it is not a mod list exported by Cubic Launcher",
                    source.display()
                )
            })?;
        let rules_name = file_names[rules_index].clone();

        // `modlist-3`: everything goes into a hidden folder first, rules.json
        // before anything else and checked at once; the mod list appears
        // under its name only when the whole archive is in. Any error on the
        // way removes the folder, so a failed import leaves nothing behind.
        let staging = StagingDir::create(modlists_dir)?;
        let staged_rules = staging.path.join(RULES_FILENAME);
        {
            let mut entry = archive.by_name(&rules_name)?;
            let mut out = std::fs::File::create(&staged_rules)
                .with_context(|| format!("failed to create {}", staged_rules.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("failed to extract {rules_name}"))?;
        }
        let mut modlist = ModList::read_from_file(&staged_rules)
            .with_context(|| "the imported archive contains an invalid rules.json")?;
        let mut local_ids = HashSet::new();
        local_mod_ids(&modlist.rules, &mut local_ids);
        let has_local_jar = |file_name: &str| {
            file_names
                .iter()
                .any(|name| relative_of(name) == format!("{LOCAL_JARS_DIR}/{file_name}"))
        };

        for raw_name in &file_names {
            if *raw_name == rules_name {
                continue;
            }
            let relative = relative_of(raw_name);
            if relative.is_empty() {
                continue;
            }

            // The global cache of an old export: jars that are downloaded again
            // on launch. Only a local mod's jar is kept, see
            // `legacy_local_jar_name`, and only when the zip has no copy of it
            // in `local-jars/` already.
            let destination_relative = if relative.starts_with("cache/") {
                match legacy_local_jar_name(&relative, &local_ids) {
                    Some(file_name) if !has_local_jar(file_name) => {
                        format!("{LOCAL_JARS_DIR}/{file_name}")
                    }
                    _ => continue,
                }
            } else {
                relative.clone()
            };

            let dest_path = contained_join(&staging.path, &destination_relative)?;
            if dest_path.exists() {
                continue;
            }
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            let mut entry = archive.by_name(raw_name)?;
            let mut out = std::fs::File::create(&dest_path)
                .with_context(|| format!("failed to create {}", dest_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("failed to extract {relative}"))?;
        }

        // The folder's name and the one in rules.json are the same thing; a
        // folder that had to become «Pack (1)» also drops the display name it
        // carried, or the sidebar shows two «Pack» (`r2-ciclo-vita-nome-2`).
        let modlist_name = unique_modlist_name(modlists_dir, &base_name);
        modlist.modlist_name = modlist_name.clone();
        modlist.write_to_file(&staged_rules)?;
        if modlist_name != base_name {
            crate::modlist_assets::forget_display_name(&staging.path)?;
        }

        staging.finish(&modlists_dir.join(&modlist_name))?;
        Ok(modlist_name)
    } else {
        // Legacy: plain rules.json import — use filename as base name
        let mut modlist = ModList::read_from_file(source).with_context(|| "invalid rules file")?;

        validate_path_component(&file_stem)?;
        if is_import_staging_dir(&file_stem) {
            bail!("'{file_stem}' cannot be the name of a mod list");
        }
        let modlist_name = unique_modlist_name(modlists_dir, &file_stem);
        let staging = StagingDir::create(modlists_dir)?;
        modlist.modlist_name = modlist_name.clone();
        modlist.write_to_file(&staging.path.join(RULES_FILENAME))?;
        staging.finish(&modlists_dir.join(&modlist_name))?;

        Ok(modlist_name)
    }
}

#[tauri::command]
pub fn copy_local_jar_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: CopyLocalJarInput,
) -> Result<(), String> {
    copy_local_jar_from_root(launcher_paths.root_dir(), &input).map_err(|e| e.to_string())
}

/// The `mod_id` a locally copied jar gets: its filename without the `.jar`
/// extension.
///
/// Shared with the version pin (`mod_version_pin.rs`), which has to know the
/// id before it writes anything — two copies of this derivation would let the
/// pin plan and the copy disagree about which rule was just created.
pub fn local_mod_id_from_jar_filename(file_name: &str) -> Result<String> {
    if !file_name.to_ascii_lowercase().ends_with(".jar") {
        bail!(
            "only .jar files are accepted for local mod upload, got '{}'",
            file_name
        );
    }

    let mod_id = file_name
        .trim_end_matches(".jar")
        .trim_end_matches(".JAR")
        .to_string();

    if mod_id.is_empty() {
        bail!("JAR filename has no stem: '{}'", file_name);
    }
    validate_path_component(&mod_id)?;

    Ok(mod_id)
}

pub fn copy_local_jar_from_root(root_dir: &Path, input: &CopyLocalJarInput) -> Result<()> {
    validate_path_component(&input.modlist_name)?;
    let source_path = Path::new(&input.source_path);

    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("source path '{}' has no valid filename", input.source_path))?;

    let mod_id = local_mod_id_from_jar_filename(file_name)?;

    // Check if a rule with this mod_id already exists
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let rules_path = launcher_paths
        .modlists_dir()
        .join(&input.modlist_name)
        .join(RULES_FILENAME);
    let modlist = ModList::read_from_file(&rules_path).with_context(|| {
        format!(
            "failed to load modlist '{}' for local JAR copy",
            input.modlist_name
        )
    })?;

    if modlist.contains_mod_id(&mod_id) {
        bail!(
            "a rule with mod_id '{}' already exists in modlist '{}'",
            mod_id,
            input.modlist_name
        );
    }

    // Copy the JAR to local-jars/
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

    let dest_path = local_jars_dir.join(format!("{}.jar", mod_id));
    std::fs::copy(source_path, &dest_path).with_context(|| {
        format!(
            "failed to copy '{}' to '{}'",
            source_path.display(),
            dest_path.display()
        )
    })?;

    // Append a new top-level rule
    add_mod_rule_from_root(
        root_dir,
        &AddModRuleInput {
            modlist_name: input.modlist_name.clone(),
            mod_id,
            source: "local".into(),
            file_name: None,
        },
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use zip::write::FileOptions;

    use crate::editor_data::load_editor_snapshot_from_root;
    use crate::rules::ModList;

    use super::{
        copy_local_jar_from_root, create_modlist_from_root, delete_modlist_from_root,
        import_modlist_from_root, CopyLocalJarInput, CreateModlistInput,
    };

    fn unique_test_root() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("cubic-modlist-mgr-test-{ts}"))
    }

    fn write_test_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            archive.start_file(*name, FileOptions::default()).unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn create_modlist_writes_skeleton_rules_json() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "My New Pack".into(),
                author: "PlayerLine".into(),
                description: "A fresh mod-list".into(),
            },
        )
        .unwrap();

        let rules_path = root
            .join("mod-lists")
            .join("My New Pack")
            .join("rules.json");
        assert!(rules_path.exists());

        // local-jars exists; custom_configs/ is not made any more: nothing
        // ever used it (todo.md:1323), the files of a custom config live in
        // config-files/ and only once there is one.
        assert!(root
            .join("mod-lists")
            .join("My New Pack")
            .join("local-jars")
            .exists());
        assert!(!root
            .join("mod-lists")
            .join("My New Pack")
            .join("custom_configs")
            .exists());

        let snapshot = load_editor_snapshot_from_root(&root, "My New Pack").unwrap();
        assert_eq!(snapshot.modlist_name, "My New Pack");
        assert!(snapshot.rows.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn create_modlist_uses_default_author_when_blank() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        let result = create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "Blank Author Pack".into(),
                author: "   ".into(),
                description: String::new(),
            },
        )
        .unwrap();

        assert_eq!(result.author, "Author");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn create_modlist_rejects_duplicate_name() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "Dup Pack".into(),
                author: "Author".into(),
                description: String::new(),
            },
        )
        .unwrap();

        let result = create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "Dup Pack".into(),
                author: "Author".into(),
                description: String::new(),
            },
        );
        assert!(result.is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn create_rejects_traversal_name() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        let result = create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "../evil".into(),
                author: "Author".into(),
                description: String::new(),
            },
        );
        assert!(result.is_err());
        assert!(!root.join("evil").exists());

        create_modlist_from_root(
            &root,
            &CreateModlistInput {
                name: "Safe Pack".into(),
                author: "Author".into(),
                description: String::new(),
            },
        )
        .unwrap();
        assert!(root.join("mod-lists").join("Safe Pack").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_rejects_traversal_name() {
        let test_root = unique_test_root();
        let root = test_root.join("launcher").join("root");
        let victim = test_root.join("launcher").join("victim");
        fs::create_dir_all(root.join("mod-lists").join("Safe Pack")).unwrap();
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("marker.txt"), b"keep me").unwrap();

        let result = delete_modlist_from_root(&root, "../../victim");
        assert!(result.is_err());
        assert!(victim.join("marker.txt").exists());

        delete_modlist_from_root(&root, "Safe Pack").unwrap();
        assert!(!root.join("mod-lists").join("Safe Pack").exists());

        fs::remove_dir_all(&test_root).unwrap();
    }

    const VALID_RULES: &[u8] = br#"{"schema_version":4,"modlist_name":"Pack","author":"Author","description":"","rules":[]}"#;

    /// What is in `mod-lists/`, sorted: a failed import must leave it as it was.
    fn modlist_folders(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join("mod-lists"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn import_zip_rejects_slip_entry() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        for (tag, entries) in [
            (
                "wrapped",
                vec![
                    ("Safe Pack/rules.json", VALID_RULES),
                    ("Safe Pack/../../outside.txt", b"escaped" as &[u8]),
                ],
            ),
            (
                "flat",
                vec![
                    ("rules.json", VALID_RULES),
                    ("../outside.txt", b"escaped" as &[u8]),
                ],
            ),
        ] {
            let malicious_zip = root.join(format!("malicious-{tag}.zip"));
            write_test_zip(&malicious_zip, &entries);

            let result = import_modlist_from_root(&root, malicious_zip.to_str().unwrap());
            assert!(result.is_err(), "{tag}");
            assert!(!root.join("mod-lists").join("outside.txt").exists());
            assert!(!root.join("outside.txt").exists());
            assert!(
                modlist_folders(&root).is_empty(),
                "{tag}: a failed import leaves nothing behind: {:?}",
                modlist_folders(&root)
            );
        }

        let valid_zip = root.join("valid.zip");
        write_test_zip(
            &valid_zip,
            &[
                ("Valid Pack/rules.json", VALID_RULES),
                ("Valid Pack/note.txt", b"valid" as &[u8]),
            ],
        );
        let imported_name = import_modlist_from_root(&root, valid_zip.to_str().unwrap()).unwrap();
        assert_eq!(imported_name, "Valid Pack");
        assert!(root
            .join("mod-lists")
            .join("Valid Pack")
            .join("note.txt")
            .exists());
        assert_eq!(modlist_folders(&root), vec!["Valid Pack".to_string()]);

        fs::remove_dir_all(&root).unwrap();
    }

    /// `modlist-3`: a rules.json the launcher cannot read — a newer schema, a
    /// duplicate mod_id — refuses the import before anything else is
    /// extracted, and leaves no folder that would empty the sidebar.
    #[test]
    fn import_with_an_invalid_rules_json_leaves_nothing_behind() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        for (tag, rules) in [
            (
                "schema",
                br#"{"schema_version":5,"modlist_name":"Pack","author":"A","description":"","rules":[]}"#
                    as &[u8],
            ),
            (
                "duplicate",
                br#"{"schema_version":4,"modlist_name":"Pack","author":"A","description":"","rules":[{"mod_id":"sodium","source":"modrinth"},{"mod_id":"sodium","source":"modrinth"}]}"#,
            ),
            ("missing", b"" as &[u8]),
        ] {
            let archive = root.join(format!("{tag}.zip"));
            let mut entries = vec![("Pack/local-jars/x.jar", b"jar" as &[u8])];
            if tag != "missing" {
                entries.push(("Pack/rules.json", rules));
            }
            write_test_zip(&archive, &entries);

            let error = import_modlist_from_root(&root, archive.to_str().unwrap())
                .expect_err("an invalid mod list must not import");
            assert!(format!("{error:#}").contains("rules.json"), "{tag}: {error:#}");
            assert!(
                modlist_folders(&root).is_empty(),
                "{tag}: {:?}",
                modlist_folders(&root)
            );
        }

        fs::remove_dir_all(&root).unwrap();
    }

    /// `sicurezza-4`: a zip without one wrapping folder keeps its paths, and
    /// takes its name from the archive's file name, not from the first folder.
    #[test]
    fn a_flat_zip_keeps_its_paths_and_takes_the_archive_name() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();
        let archive = root.join("My Pack.zip");
        write_test_zip(
            &archive,
            &[
                ("local-jars/custom-patch.jar", b"jar" as &[u8]),
                ("rules.json", VALID_RULES),
                ("local-jars-notes.txt", b"notes" as &[u8]),
            ],
        );

        let name = import_modlist_from_root(&root, archive.to_str().unwrap()).unwrap();

        assert_eq!(name, "My Pack");
        let modlist_dir = root.join("mod-lists").join("My Pack");
        assert_eq!(
            fs::read(modlist_dir.join("local-jars").join("custom-patch.jar")).unwrap(),
            b"jar"
        );
        assert!(modlist_dir.join("local-jars-notes.txt").is_file());
        assert_eq!(
            ModList::read_from_file(&modlist_dir.join("rules.json"))
                .unwrap()
                .modlist_name,
            "My Pack"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    /// `r2-ciclo-vita-nome-2`: an import that has to become «Pack (1)» does not
    /// keep showing «Pack».
    #[test]
    fn a_renamed_import_drops_the_display_name_it_carried() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists").join("Pack")).unwrap();
        let archive = root.join("pack.zip");
        write_test_zip(
            &archive,
            &[
                ("Pack/rules.json", VALID_RULES),
                (
                    "Pack/modlist-presentation.json",
                    br#"{"displayName":"Pack","iconLabel":"PK","iconAccent":"","notes":"keep me"}"#
                        as &[u8],
                ),
            ],
        );

        let name = import_modlist_from_root(&root, archive.to_str().unwrap()).unwrap();

        assert_eq!(name, "Pack (1)");
        let presentation =
            crate::modlist_assets::load_modlist_presentation_from_root(&root, &name).unwrap();
        assert_eq!(presentation.display_name, None);
        assert_eq!(presentation.notes, "keep me");
        assert_eq!(
            ModList::read_from_file(&root.join("mod-lists").join(&name).join("rules.json"))
                .unwrap()
                .modlist_name,
            "Pack (1)"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    /// `r2-aggiornamento-da-0.1.2-2`: a v0.1.x export with «Mod JAR files
    /// from cache» carries a local mod's jar only in the global cache. That
    /// jar is kept, in local-jars/; the rest of the old cache is not.
    #[test]
    fn a_v0_1_export_keeps_the_local_jar_it_carried_in_the_cache() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();
        let rules = br#"{"schema_version":4,"modlist_name":"Old Pack","author":"A","description":"","rules":[{"mod_id":"sodium","source":"modrinth","alternatives":[{"mod_id":"custom-patch","source":"local"}]}]}"#;
        let archive = root.join("old.zip");
        write_test_zip(
            &archive,
            &[
                ("Old Pack/rules.json", rules as &[u8]),
                (
                    "Old Pack/cache/mods/fabric/local/custom-patch.jar",
                    b"the only copy" as &[u8],
                ),
                (
                    "Old Pack/cache/mods/forge/local/custom-patch.jar",
                    b"the only copy" as &[u8],
                ),
                (
                    "Old Pack/cache/mods/fabric/local/another-lists-mod.jar",
                    b"not ours" as &[u8],
                ),
                (
                    "Old Pack/cache/mods/fabric/AbCdEf12/sodium.jar",
                    b"downloadable" as &[u8],
                ),
            ],
        );

        let name = import_modlist_from_root(&root, archive.to_str().unwrap()).unwrap();

        let modlist_dir = root.join("mod-lists").join(&name);
        assert_eq!(
            fs::read(modlist_dir.join("local-jars").join("custom-patch.jar")).unwrap(),
            b"the only copy"
        );
        assert!(!modlist_dir
            .join("local-jars")
            .join("another-lists-mod.jar")
            .exists());
        assert!(!modlist_dir.join("cache").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    /// 085 scheda 5, todo.md:1306, :1349: the files of a custom config go out
    /// with «Custom config files» and come back where the launch looks for
    /// them, so the imported mod list places them like the original does.
    #[test]
    fn custom_config_files_survive_export_and_import() {
        use crate::instance_configs::materialize_custom_configs;
        use crate::modlist_assets::{export_modlist_from_root, ExportModlistInput};
        use crate::rules::{CustomConfig, ModSource, Rule};

        let sender = unique_test_root();
        let modlist_dir = sender.join("mod-lists").join("Shared Pack");
        let files_dir = modlist_dir.join("config-files").join("sodium").join("cc-1");
        fs::create_dir_all(&files_dir).unwrap();
        fs::write(files_dir.join("sodium-options.json"), b"{\"fancy\":true}").unwrap();
        ModList {
            modlist_name: "Shared Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![CustomConfig {
                    id: "cc-1".into(),
                    mc_versions: vec![],
                    loader: "any".into(),
                    target_path: String::new(),
                    files: vec!["sodium-options.json".into()],
                }],
                alternatives: vec![],
            }],
        }
        .write_to_file(&modlist_dir.join("rules.json"))
        .unwrap();

        let archive_path = sender.join("shared-pack.zip");
        export_modlist_from_root(
            &sender,
            &ExportModlistInput {
                modlist_name: "Shared Pack".into(),
                destination_path: archive_path.display().to_string(),
                rules_json: true,
                config_files: true,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                selected_content_paths: Vec::new(),
                other_files: false,
                selected_other_paths: Vec::new(),
            },
        )
        .unwrap();

        let recipient = unique_test_root();
        fs::create_dir_all(recipient.join("mod-lists")).unwrap();
        let name = import_modlist_from_root(&recipient, archive_path.to_str().unwrap()).unwrap();
        let imported_dir = recipient.join("mod-lists").join(&name);
        let imported = ModList::read_from_file(&imported_dir.join("rules.json")).unwrap();

        let instance = recipient.join("instance");
        let placed = materialize_custom_configs(
            &imported_dir,
            &instance,
            &imported.rules,
            "1.21.1",
            "fabric",
        )
        .unwrap();
        assert!(placed.missing_files.is_empty(), "{placed:?}");
        assert_eq!(
            fs::read(instance.join("config").join("sodium-options.json")).unwrap(),
            b"{\"fancy\":true}"
        );

        fs::remove_dir_all(&sender).unwrap();
        fs::remove_dir_all(&recipient).unwrap();
    }

    #[test]
    fn import_zip_rejects_unsafe_archive_base_name() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();
        let archive_path = root.join("...zip");
        write_test_zip(&archive_path, &[]);

        let result = import_modlist_from_root(&root, archive_path.to_str().unwrap());
        assert!(result.is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_local_jar_places_file_and_adds_rule() {
        let root = unique_test_root();
        let modlist_dir = root.join("mod-lists").join("Test Pack");
        fs::create_dir_all(modlist_dir.join("local-jars")).unwrap();

        ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![],
        }
        .write_to_file(&modlist_dir.join("rules.json"))
        .unwrap();

        // Create a fake JAR source file
        let source_dir = root.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let source_jar = source_dir.join("custom-patch-1.0.jar");
        fs::write(&source_jar, b"fake jar content").unwrap();

        copy_local_jar_from_root(
            &root,
            &CopyLocalJarInput {
                source_path: source_jar.to_string_lossy().into_owned(),
                modlist_name: "Test Pack".into(),
            },
        )
        .unwrap();

        assert!(modlist_dir
            .join("local-jars")
            .join("custom-patch-1.0.jar")
            .exists());

        let snapshot = load_editor_snapshot_from_root(&root, "Test Pack").unwrap();
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].mod_id, "custom-patch-1.0");
        assert_eq!(snapshot.rows[0].source, "local");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_local_jar_rejects_unsafe_components() {
        let root = unique_test_root();
        let source_dir = root.join("source");
        fs::create_dir_all(&source_dir).unwrap();

        let ordinary_jar = source_dir.join("ordinary.jar");
        fs::write(&ordinary_jar, b"fake").unwrap();
        let escaped_modlist_dir = root.join("escape");
        fs::create_dir_all(escaped_modlist_dir.join("local-jars")).unwrap();
        ModList {
            modlist_name: "../escape".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![],
        }
        .write_to_file(&escaped_modlist_dir.join("rules.json"))
        .unwrap();
        let traversal_result = copy_local_jar_from_root(
            &root,
            &CopyLocalJarInput {
                source_path: ordinary_jar.to_string_lossy().into_owned(),
                modlist_name: "../escape".into(),
            },
        );
        assert!(traversal_result.is_err());
        assert!(!escaped_modlist_dir
            .join("local-jars")
            .join("ordinary.jar")
            .exists());

        let modlist_dir = root.join("mod-lists").join("Safe Pack");
        fs::create_dir_all(modlist_dir.join("local-jars")).unwrap();
        ModList {
            modlist_name: "Safe Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![],
        }
        .write_to_file(&modlist_dir.join("rules.json"))
        .unwrap();
        let unsafe_id_jar = source_dir.join("...jar");
        fs::write(&unsafe_id_jar, b"fake").unwrap();
        let unsafe_id_result = copy_local_jar_from_root(
            &root,
            &CopyLocalJarInput {
                source_path: unsafe_id_jar.to_string_lossy().into_owned(),
                modlist_name: "Safe Pack".into(),
            },
        );
        assert!(unsafe_id_result.is_err());
        assert!(!modlist_dir.join("local-jars").join("...jar").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_local_jar_rejects_non_jar_files() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists").join("Any Pack")).unwrap();

        let result = copy_local_jar_from_root(
            &root,
            &CopyLocalJarInput {
                source_path: "/tmp/not-a-jar.zip".into(),
                modlist_name: "Any Pack".into(),
            },
        );

        assert!(result.is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_local_jar_rejects_duplicate_mod_id() {
        let root = unique_test_root();
        let modlist_dir = root.join("mod-lists").join("Test Pack");
        fs::create_dir_all(modlist_dir.join("local-jars")).unwrap();

        use crate::rules::{ModSource, Rule};
        ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![Rule {
                mod_id: "existing-mod".into(),
                source: ModSource::Local,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            }],
        }
        .write_to_file(&modlist_dir.join("rules.json"))
        .unwrap();

        let source_dir = root.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let source_jar = source_dir.join("existing-mod.jar");
        fs::write(&source_jar, b"fake").unwrap();

        let result = copy_local_jar_from_root(
            &root,
            &CopyLocalJarInput {
                source_path: source_jar.to_string_lossy().into_owned(),
                modlist_name: "Test Pack".into(),
            },
        );
        assert!(result.is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    /// D107, il caso che interessa a chi riceve la mod list: un mod locale
    /// (o pinnato, D14) esportato da una root e importato in un'altra arriva
    /// con il suo jar, byte per byte. Senza, l'import produce una voce locale
    /// senza jar, cioè `CacheProbe::JarMissingUnrecoverable`.
    #[test]
    fn local_jar_survives_export_and_import_into_another_root() {
        use crate::modlist_assets::{export_modlist_from_root, ExportModlistInput};

        let sender = unique_test_root();
        fs::create_dir_all(sender.join("mod-lists")).unwrap();
        create_modlist_from_root(
            &sender,
            &CreateModlistInput {
                name: "Shared Pack".into(),
                author: "Author".into(),
                description: String::new(),
            },
        )
        .unwrap();
        let source_jar = sender.join("source").join("custom-patch-1.0.jar");
        fs::create_dir_all(source_jar.parent().unwrap()).unwrap();
        let jar_bytes = b"PK\x03\x04 the sender's only copy".to_vec();
        fs::write(&source_jar, &jar_bytes).unwrap();
        copy_local_jar_from_root(
            &sender,
            &CopyLocalJarInput {
                source_path: source_jar.to_string_lossy().into_owned(),
                modlist_name: "Shared Pack".into(),
            },
        )
        .unwrap();

        let archive_path = sender.join("shared-pack.zip");
        export_modlist_from_root(
            &sender,
            &ExportModlistInput {
                modlist_name: "Shared Pack".into(),
                destination_path: archive_path.display().to_string(),
                rules_json: true,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: false,
                selected_content_paths: Vec::new(),
                selected_other_paths: Vec::new(),
            },
        )
        .unwrap();

        let recipient = unique_test_root();
        fs::create_dir_all(recipient.join("mod-lists")).unwrap();
        let imported_name =
            import_modlist_from_root(&recipient, archive_path.to_str().unwrap()).unwrap();

        let imported_jar = recipient
            .join("mod-lists")
            .join(&imported_name)
            .join("local-jars")
            .join("custom-patch-1.0.jar");
        assert!(
            imported_jar.exists(),
            "the imported mod list must carry its local jar at {}",
            imported_jar.display()
        );
        assert_eq!(fs::read(&imported_jar).unwrap(), jar_bytes);

        fs::remove_dir_all(&sender).unwrap();
        fs::remove_dir_all(&recipient).unwrap();
    }
}
