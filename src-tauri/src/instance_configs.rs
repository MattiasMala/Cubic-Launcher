use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::path_safety::{contained_join, normalize_relative_dir, validate_path_component};
use crate::rules::{custom_config_files_dir, CustomConfig, Rule, CUSTOM_CONFIG_FILES_DIRNAME};

/// One name in a custom config's `files` that has no file in the mod list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCustomConfigFile {
    pub mod_id: String,
    pub config_id: String,
    pub file_name: String,
}

/// A custom config whose «Path» cannot be used: absolute, with `..`, or
/// otherwise not a folder inside the instance. Nothing of it is placed; the
/// rest of the launch goes on (`modlist-10`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedCustomConfig {
    pub mod_id: String,
    pub config_id: String,
    pub target_path: String,
    pub reason: String,
}

/// Two custom configs that put a file with the same name in the same folder of
/// the instance. The later one in the tree order wins — the same order every
/// launch and every machine that imported the mod list uses — and the earlier
/// one is named here so the log says so (todo.md:1295, :1309).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomConfigCollision {
    pub target: PathBuf,
    pub overridden_mod_id: String,
    pub overridden_config_id: String,
    pub winner_mod_id: String,
    pub winner_config_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaterializedCustomConfigs {
    pub materialized_files: Vec<PathBuf>,
    pub missing_files: Vec<MissingCustomConfigFile>,
    pub skipped_configs: Vec<SkippedCustomConfig>,
    pub collisions: Vec<CustomConfigCollision>,
}

/// Put the custom configs of a mod list that apply to this Minecraft version
/// and loader into the instance, at every launch (D105).
///
/// Rooted on the instance, not on `config/`: `target_path` is a directory
/// relative to the instance root and empty means `config` (D129). Backslashes
/// in it are separators ([`normalize_relative_dir`]); one that leaves the
/// instance — absolute, or with `..` — is not placed and goes into
/// `skipped_configs`, and the other configs still are: a bad «Path», even on a
/// mod the launch does not use, never stops the launch (`modlist-10`).
/// Applies-to follows the repo's convention
/// (`launch_preview_content::is_content_entry_active`): no versions means any
/// version, `any` or a case-insensitive match for the loader.
///
/// Each file is the launcher's copy in
/// [`custom_config_files_dir`]; the instance file is removed and the copy
/// written with `fs::copy`, so an edit made in game is lost at the next launch
/// (D118). Never a symlink or a hard link: with those the instance file *is*
/// the launcher's file, a game edit would rewrite it, and the next launch
/// would "restore" the game's edit.
///
/// A name that does not resolve to a file there — gone, or a pre-D112
/// absolute path, which is never read as a path — goes into
/// `missing_files` and the rest is still placed: the caller logs it. Two
/// configs that write the same file: the later one in the tree wins, and the
/// pair goes into `collisions`.
pub fn materialize_custom_configs(
    modlist_dir: &Path,
    instance_root: &Path,
    rules: &[Rule],
    mc_version: &str,
    loader: &str,
) -> Result<MaterializedCustomConfigs> {
    fn collect<'a>(
        rules: &'a [Rule],
        mc_version: &str,
        loader: &str,
        out: &mut Vec<(&'a str, &'a CustomConfig)>,
    ) {
        for rule in rules {
            for config in &rule.custom_configs {
                let version_match = config.mc_versions.is_empty()
                    || config.mc_versions.iter().any(|v| v == mc_version);
                let loader_match =
                    config.loader == "any" || config.loader.eq_ignore_ascii_case(loader);
                if version_match && loader_match {
                    out.push((&rule.mod_id, config));
                }
            }
            collect(&rule.alternatives, mc_version, loader, out);
        }
    }
    let mut active = Vec::new();
    collect(rules, mc_version, loader, &mut active);

    let mut result = MaterializedCustomConfigs::default();
    let mut placements = Vec::with_capacity(active.len());
    for (mod_id, config) in active {
        let target_dir = normalize_relative_dir(&config.target_path).and_then(|relative| {
            let relative = if relative.is_empty() {
                "config".to_string()
            } else {
                relative
            };
            contained_join(instance_root, &relative)
        });
        match target_dir {
            Ok(target_dir) => placements.push((mod_id, config, target_dir)),
            Err(error) => result.skipped_configs.push(SkippedCustomConfig {
                mod_id: mod_id.to_string(),
                config_id: config.id.clone(),
                target_path: config.target_path.clone(),
                reason: format!("{error:#}"),
            }),
        }
    }

    // Who wrote each target last, to name the one it overrides.
    let mut written_by: HashMap<PathBuf, (String, String)> = HashMap::new();
    for (mod_id, config, target_dir) in placements {
        let source_dir = custom_config_files_dir(modlist_dir, mod_id, &config.id).ok();
        for name in &config.files {
            let source = source_dir
                .as_ref()
                .filter(|_| validate_path_component(name).is_ok())
                .map(|dir| dir.join(name))
                .filter(|path| path.is_file());
            let Some(source) = source else {
                result.missing_files.push(MissingCustomConfigFile {
                    mod_id: mod_id.to_string(),
                    config_id: config.id.clone(),
                    file_name: name.clone(),
                });
                continue;
            };
            fs::create_dir_all(&target_dir)
                .with_context(|| format!("failed to create {}", target_dir.display()))?;
            let target = target_dir.join(name);
            copy_replacing(&source, &target)?;
            if let Some((overridden_mod_id, overridden_config_id)) =
                written_by.insert(target.clone(), (mod_id.to_string(), config.id.clone()))
            {
                result.collisions.push(CustomConfigCollision {
                    target: target.clone(),
                    overridden_mod_id,
                    overridden_config_id,
                    winner_mod_id: mod_id.to_string(),
                    winner_config_id: config.id.clone(),
                });
                result.materialized_files.retain(|placed| placed != &target);
            }
            result.materialized_files.push(target);
        }
    }
    Ok(result)
}

/// Remove from the mod list's `config-files/` what no custom config lists any
/// more (todo.md:1291): a file taken off a config's list, the folder of a
/// config that was deleted. Returns what was removed, for the log.
///
/// A config folder is kept as long as a custom config with that id exists
/// anywhere in the tree, even under another mod: ids are unique, and a folder
/// that a config of another mod still names is a move to finish, not
/// garbage. Inside a folder whose config is under the same mod, only the
/// names the config does not list are removed. Folders that are not a custom
/// config's (`config-files/<mod_id>/` with nothing left) go when empty.
pub fn prune_orphan_custom_config_files(
    modlist_dir: &Path,
    rules: &[Rule],
) -> Result<Vec<PathBuf>> {
    fn index<'a>(rules: &'a [Rule], out: &mut HashMap<&'a str, Vec<(&'a str, &'a CustomConfig)>>) {
        for rule in rules {
            for config in &rule.custom_configs {
                if !config.id.is_empty() {
                    out.entry(config.id.as_str())
                        .or_default()
                        .push((rule.mod_id.as_str(), config));
                }
            }
            index(&rule.alternatives, out);
        }
    }
    let mut configs_by_id = HashMap::new();
    index(rules, &mut configs_by_id);

    let root = modlist_dir.join(CUSTOM_CONFIG_FILES_DIRNAME);
    let mut removed = Vec::new();
    for mod_dir in child_entries(&root)? {
        if !fs::symlink_metadata(&mod_dir)
            .with_context(|| format!("failed to inspect {}", mod_dir.display()))?
            .is_dir()
        {
            continue;
        }
        let mod_id = mod_dir.file_name().unwrap_or_default().to_string_lossy();
        for config_dir in child_entries(&mod_dir)? {
            let config_id = config_dir.file_name().unwrap_or_default().to_string_lossy();
            let Some(configs) = configs_by_id.get(config_id.as_ref()) else {
                remove_any(&config_dir)?;
                removed.push(config_dir);
                continue;
            };
            let Some((_, config)) = configs.iter().find(|(owner, _)| *owner == mod_id) else {
                continue;
            };
            if !fs::symlink_metadata(&config_dir)
                .with_context(|| format!("failed to inspect {}", config_dir.display()))?
                .is_dir()
            {
                continue;
            }
            for file in child_entries(&config_dir)? {
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                if !config.files.iter().any(|listed| *listed == name) {
                    remove_any(&file)?;
                    removed.push(file);
                }
            }
        }
        if child_entries(&mod_dir)?.is_empty() {
            fs::remove_dir(&mod_dir)
                .with_context(|| format!("failed to remove {}", mod_dir.display()))?;
        }
    }
    Ok(removed)
}

/// The entries of `dir`; a directory that is not there has none.
fn child_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()))
        }
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        entries.push(
            entry
                .with_context(|| format!("failed to read an entry of {}", dir.display()))?
                .path(),
        );
    }
    entries.sort();
    Ok(entries)
}

/// Remove a file, a link (never what it points to) or a whole directory.
fn remove_any(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("failed to remove {}", path.display()))
}

fn copy_replacing(source: &Path, target: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(target) {
        if metadata.is_dir() {
            bail!(
                "cannot place custom config file at {}: a directory is in the way",
                target.display()
            );
        }
        fs::remove_file(target)
            .with_context(|| format!("failed to remove {}", target.display()))?;
    }
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy custom config file {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod custom_config_tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::materialize_custom_configs;
    use crate::rules::{CustomConfig, ModSource, Rule};

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("cubic-launcher-custom-configs-test-{timestamp}"))
    }

    fn config(
        id: &str,
        mc_versions: &[&str],
        loader: &str,
        target_path: &str,
        files: &[&str],
    ) -> CustomConfig {
        CustomConfig {
            id: id.into(),
            mc_versions: mc_versions.iter().map(|v| v.to_string()).collect(),
            loader: loader.into(),
            target_path: target_path.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn rule_with(configs: Vec<CustomConfig>) -> Rule {
        Rule {
            mod_id: "sodium".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: configs,
            alternatives: vec![],
        }
    }

    /// The launcher's copy of one file, in `config-files/sodium/<id>/`.
    fn store(modlist_dir: &Path, config_id: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = modlist_dir
            .join("config-files")
            .join("sodium")
            .join(config_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_matching_custom_config_lands_in_its_target_directory() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        store(&modlist_dir, "cc-1", "shader.txt", b"shader settings");
        let rules = [rule_with(vec![config(
            "cc-1",
            &["1.21.1"],
            "Fabric",
            "shaderpacks",
            &["shader.txt"],
        )])];

        let placed =
            materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
                .expect("materialization should succeed");

        let target = instance.join("shaderpacks").join("shader.txt");
        assert_eq!(fs::read(&target).unwrap(), b"shader settings");
        assert_eq!(placed.materialized_files, vec![target]);
        assert!(placed.missing_files.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_custom_config_for_another_version_or_loader_is_not_placed() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        store(&modlist_dir, "cc-old", "old.json", b"old");
        store(&modlist_dir, "cc-forge", "forge.json", b"forge");
        let rules = [rule_with(vec![
            config("cc-old", &["1.20.1"], "any", "", &["old.json"]),
            config("cc-forge", &[], "forge", "", &["forge.json"]),
        ])];

        let placed =
            materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
                .expect("materialization should succeed");

        assert!(placed.materialized_files.is_empty());
        assert!(!instance.join("config").join("old.json").exists());
        assert!(!instance.join("config").join("forge.json").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_target_path_means_the_config_directory() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        store(&modlist_dir, "cc-1", "sodium-options.json", b"{}");
        let rules = [rule_with(vec![config(
            "cc-1",
            &[],
            "any",
            "",
            &["sodium-options.json"],
        )])];

        materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
            .expect("materialization should succeed");

        assert_eq!(
            fs::read(instance.join("config").join("sodium-options.json")).unwrap(),
            b"{}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// `modlist-10`: a «Path» that leaves the instance is not placed, and it
    /// does not take the other configs — or the launch — down with it.
    #[test]
    fn a_target_path_outside_the_instance_is_skipped_and_the_rest_is_placed() {
        for escaping in ["../outside", "/tmp/cubic-custom-config-absolute-target"] {
            let root = unique_test_root();
            let modlist_dir = root.join("Pack");
            let instance = root.join("instance");
            store(&modlist_dir, "cc-1", "evil.txt", b"evil");
            store(&modlist_dir, "cc-2", "good.json", b"good");
            let rules = [rule_with(vec![
                config("cc-1", &[], "any", escaping, &["evil.txt"]),
                config("cc-2", &[], "any", "", &["good.json"]),
            ])];

            let placed =
                materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
                    .expect("a bad path must not fail the launch");

            assert_eq!(placed.skipped_configs.len(), 1, "{placed:?}");
            let skipped = &placed.skipped_configs[0];
            assert_eq!(
                (
                    skipped.mod_id.as_str(),
                    skipped.config_id.as_str(),
                    skipped.target_path.as_str()
                ),
                ("sodium", "cc-1", escaping)
            );
            assert!(skipped.reason.contains(escaping), "{}", skipped.reason);
            assert!(!root.join("outside").join("evil.txt").exists());
            assert!(!Path::new(escaping).join("evil.txt").exists());
            assert_eq!(
                fs::read(instance.join("config").join("good.json")).unwrap(),
                b"good"
            );
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// `r2-percorsi-non-ascii-e-limiti-windows-4`: a «Path» typed with
    /// backslashes is a nested folder on every platform, not one folder with a
    /// backslash in its name.
    #[test]
    fn a_target_path_with_backslashes_is_a_nested_folder() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        store(&modlist_dir, "cc-1", "create.toml", b"create");
        let rules = [rule_with(vec![config(
            "cc-1",
            &[],
            "any",
            "config\\create",
            &["create.toml"],
        )])];

        materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
            .expect("materialization should succeed");

        assert_eq!(
            fs::read(instance.join("config").join("create").join("create.toml")).unwrap(),
            b"create"
        );
        assert!(!instance.join("config\\create").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// todo.md:1309: two configs that write the same file. The later one in
    /// the tree wins — an alternative after its primary — and the pair is
    /// reported, so the launch can log it instead of choosing in silence.
    #[test]
    fn two_configs_writing_the_same_file_are_reported_and_the_later_wins() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        store(&modlist_dir, "cc-primary", "shared.json", b"primary");
        let alt_dir = modlist_dir
            .join("config-files")
            .join("embeddium")
            .join("cc-alt");
        fs::create_dir_all(&alt_dir).unwrap();
        fs::write(alt_dir.join("shared.json"), b"alternative").unwrap();
        let mut primary = rule_with(vec![config(
            "cc-primary",
            &[],
            "any",
            "config",
            &["shared.json"],
        )]);
        let mut alternative = rule_with(vec![config("cc-alt", &[], "any", "", &["shared.json"])]);
        alternative.mod_id = "embeddium".into();
        primary.alternatives = vec![alternative];

        let placed =
            materialize_custom_configs(&modlist_dir, &instance, &[primary], "1.21.1", "fabric")
                .expect("materialization should succeed");

        let target = instance.join("config").join("shared.json");
        assert_eq!(fs::read(&target).unwrap(), b"alternative");
        assert_eq!(placed.materialized_files, vec![target.clone()]);
        assert_eq!(placed.collisions.len(), 1, "{placed:?}");
        let collision = &placed.collisions[0];
        assert_eq!(collision.target, target);
        assert_eq!(
            (
                collision.overridden_mod_id.as_str(),
                collision.overridden_config_id.as_str(),
                collision.winner_mod_id.as_str(),
                collision.winner_config_id.as_str()
            ),
            ("sodium", "cc-primary", "embeddium", "cc-alt")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// todo.md:1291: what no custom config lists any more leaves
    /// `config-files/`; what one lists stays, and so does a folder whose id a
    /// config of another mod still names.
    #[test]
    fn pruning_removes_only_what_no_custom_config_lists() {
        use super::prune_orphan_custom_config_files;

        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let kept = store(&modlist_dir, "cc-1", "kept.json", b"kept");
        let dropped = store(&modlist_dir, "cc-1", "dropped.json", b"dropped");
        let deleted_config = store(&modlist_dir, "cc-gone", "x.json", b"x");
        let moved_dir = modlist_dir
            .join("config-files")
            .join("old-mod")
            .join("cc-moved");
        fs::create_dir_all(&moved_dir).unwrap();
        fs::write(moved_dir.join("moved.json"), b"moved").unwrap();
        let removed_mod_dir = modlist_dir.join("config-files").join("removed-mod");
        fs::create_dir_all(removed_mod_dir.join("cc-old")).unwrap();
        fs::write(removed_mod_dir.join("cc-old").join("a.json"), b"a").unwrap();
        let mut other = rule_with(vec![config("cc-moved", &[], "any", "", &["moved.json"])]);
        other.mod_id = "new-mod".into();
        let rules = [
            rule_with(vec![config("cc-1", &[], "any", "", &["kept.json"])]),
            other,
        ];

        let removed =
            prune_orphan_custom_config_files(&modlist_dir, &rules).expect("pruning should succeed");

        assert!(kept.is_file());
        assert!(!dropped.exists());
        assert!(!deleted_config.exists());
        assert!(moved_dir.join("moved.json").is_file());
        assert!(!removed_mod_dir.exists());
        assert!(removed.contains(&dropped), "{removed:?}");
        assert!(
            removed.contains(&deleted_config.parent().unwrap().to_path_buf()),
            "{removed:?}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pruning_a_mod_list_without_config_files_does_nothing() {
        use super::prune_orphan_custom_config_files;

        let root = unique_test_root();
        let removed = prune_orphan_custom_config_files(&root.join("Pack"), &[])
            .expect("a missing config-files/ is not an error");
        assert!(removed.is_empty());
    }

    /// D118: the launcher's file is the truth, at every launch. And the vincolo
    /// 3 of the prompt: the instance gets a *copy*. With a symlink or a hard
    /// link the game would write through to the launcher's file, and the next
    /// launch would "restore" the game's edit.
    #[cfg(unix)]
    #[test]
    fn an_existing_instance_file_is_overwritten_with_a_copy_not_a_link() {
        use std::os::unix::fs::MetadataExt;

        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        let source = store(&modlist_dir, "cc-1", "options.json", b"launcher truth");
        let target = instance.join("config").join("options.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"edited in game").unwrap();
        let rules = [rule_with(vec![config(
            "cc-1",
            &[],
            "any",
            "config",
            &["options.json"],
        )])];

        materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
            .expect("materialization should succeed");

        assert_eq!(fs::read(&target).unwrap(), b"launcher truth");
        let metadata = fs::symlink_metadata(&target).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "the instance file must not be a symlink"
        );
        assert!(metadata.file_type().is_file());
        assert_eq!(
            metadata.nlink(),
            1,
            "the instance file must not be a hard link"
        );
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);

        fs::write(&target, b"edited in game again").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"launcher truth");
        fs::remove_dir_all(&root).unwrap();
    }

    /// A name that does not resolve in the mod list is reported, never read as
    /// a path, and does not stop the other files.
    #[test]
    fn a_name_that_does_not_resolve_is_reported_and_the_rest_is_placed() {
        let root = unique_test_root();
        let modlist_dir = root.join("Pack");
        let instance = root.join("instance");
        let stray = root.join("user-disk").join("legacy.json");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        fs::write(&stray, b"found by accident").unwrap();
        store(&modlist_dir, "cc-1", "present.json", b"present");
        let legacy = stray.display().to_string();
        let rules = [rule_with(vec![config(
            "cc-1",
            &[],
            "any",
            "",
            &["gone.json", &legacy, "present.json"],
        )])];

        let placed =
            materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
                .expect("a missing file must not fail the launch");

        assert_eq!(
            fs::read(instance.join("config").join("present.json")).unwrap(),
            b"present"
        );
        assert!(!instance.join("config").join("legacy.json").exists());
        let missing = placed
            .missing_files
            .iter()
            .map(|m| {
                (
                    m.mod_id.as_str(),
                    m.config_id.as_str(),
                    m.file_name.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            missing,
            vec![
                ("sodium", "cc-1", "gone.json"),
                ("sodium", "cc-1", legacy.as_str())
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
