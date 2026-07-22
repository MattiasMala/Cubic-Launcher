use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::editor_data::{add_mod_rule_from_root, AddModRuleInput};
use crate::launcher_paths::LauncherPaths;
use crate::path_safety::{contained_join, validate_path_component};
use crate::rules::{ModList, RULES_FILENAME};
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
    std::fs::create_dir_all(modlist_dir.join("custom_configs")).with_context(|| {
        format!(
            "failed to create custom_configs directory for modlist '{}'",
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

fn import_modlist_from_root(root_dir: &Path, source_path: &str) -> Result<String> {
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlists_dir = launcher_paths.modlists_dir();

    let source = Path::new(source_path);
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "zip" {
        let file = std::fs::File::open(source)
            .with_context(|| format!("failed to open {}", source.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read zip archive {}", source.display()))?;

        // Derive name from the archive root folder
        let archive_root = {
            let mut root = String::new();
            for i in 0..archive.len() {
                let name = archive.by_index(i)?.name().to_string();
                if let Some(slash) = name.find('/') {
                    root = name[..slash].to_string();
                    break;
                }
            }
            root
        };

        let base_name = if archive_root.is_empty() {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Imported")
                .to_string()
        } else {
            archive_root.clone()
        };
        validate_path_component(&base_name)?;

        let modlist_name = unique_modlist_name(&modlists_dir, &base_name);
        let modlist_dir = modlists_dir.join(&modlist_name);
        std::fs::create_dir_all(&modlist_dir)
            .with_context(|| format!("failed to create modlist dir {}", modlist_dir.display()))?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let raw_name = entry.name().to_string();
            if entry.is_dir() {
                continue;
            }

            // Strip the archive root prefix to get relative path
            let relative = if !archive_root.is_empty() && raw_name.starts_with(&archive_root) {
                raw_name[archive_root.len()..]
                    .trim_start_matches('/')
                    .to_string()
            } else {
                raw_name.clone()
            };
            if relative.is_empty() {
                continue;
            }

            // Skip cached mod jars — they'll be re-downloaded on launch
            if relative.starts_with("cache/") {
                continue;
            }

            let dest_path = contained_join(&modlist_dir, &relative)?;
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&dest_path)
                .with_context(|| format!("failed to create {}", dest_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("failed to extract {}", relative))?;
        }

        // Patch modlist_name inside rules.json to match the new folder name
        let rules_path = modlist_dir.join(RULES_FILENAME);
        if rules_path.exists() {
            let mut modlist = ModList::read_from_file(&rules_path)
                .with_context(|| "imported zip contains an invalid rules.json")?;
            modlist.modlist_name = modlist_name.clone();
            modlist.write_to_file(&rules_path)?;
        }

        Ok(modlist_name)
    } else {
        // Legacy: plain rules.json import — use filename as base name
        let mut modlist = ModList::read_from_file(source).with_context(|| "invalid rules file")?;

        let base_name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Imported")
            .to_string();
        validate_path_component(&base_name)?;
        let modlist_name = unique_modlist_name(&modlists_dir, &base_name);
        let modlist_dir = modlists_dir.join(&modlist_name);
        std::fs::create_dir_all(&modlist_dir)
            .with_context(|| format!("failed to create modlist dir {}", modlist_dir.display()))?;

        modlist.modlist_name = modlist_name.clone();
        modlist.write_to_file(&modlist_dir.join(RULES_FILENAME))?;

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

pub fn copy_local_jar_from_root(root_dir: &Path, input: &CopyLocalJarInput) -> Result<()> {
    validate_path_component(&input.modlist_name)?;
    let source_path = Path::new(&input.source_path);

    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("source path '{}' has no valid filename", input.source_path))?;

    if !file_name.to_ascii_lowercase().ends_with(".jar") {
        bail!(
            "only .jar files are accepted for local mod upload, got '{}'",
            file_name
        );
    }

    // Derive mod_id from the filename stem (without .jar)
    let mod_id = file_name
        .trim_end_matches(".jar")
        .trim_end_matches(".JAR")
        .to_string();

    if mod_id.is_empty() {
        bail!("JAR filename has no stem: '{}'", file_name);
    }
    validate_path_component(&mod_id)?;

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
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::io::Write;

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
            archive
                .start_file(*name, FileOptions::default())
                .unwrap();
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

        // local-jars and custom_configs directories should exist
        assert!(root
            .join("mod-lists")
            .join("My New Pack")
            .join("local-jars")
            .exists());
        assert!(root
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

    #[test]
    fn import_zip_rejects_slip_entry() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();

        let malicious_zip = root.join("malicious.zip");
        write_test_zip(
            &malicious_zip,
            &[
                ("Safe Pack/note.txt", b"safe" as &[u8]),
                ("../outside.txt", b"escaped" as &[u8]),
            ],
        );

        let result =
            import_modlist_from_root(&root, malicious_zip.to_str().unwrap());
        assert!(result.is_err());
        assert!(!root.join("mod-lists").join("outside.txt").exists());

        let valid_zip = root.join("valid.zip");
        write_test_zip(
            &valid_zip,
            &[("Valid Pack/note.txt", b"valid" as &[u8])],
        );
        let imported_name =
            import_modlist_from_root(&root, valid_zip.to_str().unwrap()).unwrap();
        assert_eq!(imported_name, "Valid Pack");
        assert!(root
            .join("mod-lists")
            .join("Valid Pack")
            .join("note.txt")
            .exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn import_zip_rejects_unsafe_archive_base_name() {
        let root = unique_test_root();
        fs::create_dir_all(root.join("mod-lists")).unwrap();
        let archive_path = root.join("...zip");
        write_test_zip(&archive_path, &[]);

        let result =
            import_modlist_from_root(&root, archive_path.to_str().unwrap());
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
}
