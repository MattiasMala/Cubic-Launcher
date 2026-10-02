use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::path_safety::{contained_join, validate_path_component};
use crate::rules::{custom_config_files_dir, CustomConfig, Rule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedConfigPlacement {
    pub cache_subdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInstanceConfigs {
    pub materialized_files: Vec<PathBuf>,
}

pub fn prepare_instance_config_directory(
    configs_cache_dir: &Path,
    instance_config_dir: &Path,
    placements: &[CachedConfigPlacement],
) -> Result<PreparedInstanceConfigs> {
    fs::create_dir_all(instance_config_dir).with_context(|| {
        format!(
            "failed to create instance config directory at {}",
            instance_config_dir.display()
        )
    })?;

    let mut materialized_files = Vec::new();

    for placement in placements {
        let source_dir = configs_cache_dir.join(&placement.cache_subdir);

        if !source_dir.exists() {
            bail!(
                "required config cache directory '{}' is missing from {}",
                placement.cache_subdir,
                configs_cache_dir.display()
            );
        }

        if !source_dir.is_dir() {
            bail!(
                "config cache entry '{}' is not a directory in {}",
                placement.cache_subdir,
                configs_cache_dir.display()
            );
        }

        collect_and_materialize_config_files(
            &source_dir,
            &source_dir,
            instance_config_dir,
            &mut materialized_files,
        )?;
    }

    Ok(PreparedInstanceConfigs { materialized_files })
}

fn collect_and_materialize_config_files(
    source_root: &Path,
    current_dir: &Path,
    instance_config_dir: &Path,
    materialized_files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(current_dir).with_context(|| {
        format!(
            "failed to read config cache directory {}",
            current_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect config cache entry in {}",
                current_dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;

        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            collect_and_materialize_config_files(
                source_root,
                &path,
                instance_config_dir,
                materialized_files,
            )?;
            continue;
        }

        let relative_path = path.strip_prefix(source_root).with_context(|| {
            format!(
                "failed to determine relative config path for {} from {}",
                path.display(),
                source_root.display()
            )
        })?;
        let target_path = instance_config_dir.join(relative_path);

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create config target directory {}",
                    parent.display()
                )
            })?;
        }

        link_or_copy_file(&path, &target_path)?;
        materialized_files.push(target_path);
    }

    Ok(())
}

fn link_or_copy_file(source_path: &Path, target_path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(target_path) {
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(target_path).with_context(|| {
                format!(
                    "failed to remove directory target {}",
                    target_path.display()
                )
            })?;
        } else {
            fs::remove_file(target_path).with_context(|| {
                format!("failed to remove file target {}", target_path.display())
            })?;
        }
    }

    if create_symlink_file(source_path, target_path).is_ok() {
        return Ok(());
    }

    if fs::hard_link(source_path, target_path).is_ok() {
        return Ok(());
    }

    fs::copy(source_path, target_path)
        .map(|_| ())
        .with_context(|| {
            format!(
                "failed to materialize config file from {} to {}",
                source_path.display(),
                target_path.display()
            )
        })
}

#[cfg(target_family = "unix")]
fn create_symlink_file(source_path: &Path, target_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source_path, target_path).map_err(Into::into)
}

#[cfg(target_family = "windows")]
fn create_symlink_file(source_path: &Path, target_path: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(source_path, target_path).map_err(Into::into)
}

/// One name in a custom config's `files` that has no file in the mod list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCustomConfigFile {
    pub mod_id: String,
    pub config_id: String,
    pub file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaterializedCustomConfigs {
    pub materialized_files: Vec<PathBuf>,
    pub missing_files: Vec<MissingCustomConfigFile>,
}

/// Put the custom configs of a mod list that apply to this Minecraft version
/// and loader into the instance, at every launch (D105).
///
/// Rooted on the instance, not on `config/`: `target_path` is a directory
/// relative to the instance root and empty means `config` (D129), and one that
/// leaves the instance — absolute, or `..` past the root — refuses the whole
/// call before anything is written. Applies-to follows the repo's convention
/// (`launch_preview_content::is_content_entry_active`): no versions means any
/// version, `any` or a case-insensitive match for the loader.
///
/// Each file is the launcher's copy in
/// [`custom_config_files_dir`]; the instance file is removed and the copy
/// written with `fs::copy`, so an edit made in game is lost at the next launch
/// (D118). Never a symlink or a hard link, as [`link_or_copy_file`] would try:
/// with those the instance file *is* the launcher's file, a game edit would
/// rewrite it, and the next launch would "restore" the game's edit.
///
/// A name that does not resolve to a file there — gone, or a pre-D112
/// absolute path, which is never read as a path — goes into
/// `missing_files` and the rest is still placed: the caller logs it.
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

    let mut placements = Vec::with_capacity(active.len());
    for (mod_id, config) in active {
        let relative = if config.target_path.trim().is_empty() {
            "config"
        } else {
            config.target_path.as_str()
        };
        let target_dir = contained_join(instance_root, relative).with_context(|| {
            format!(
                "custom config '{}' of '{mod_id}' has target path '{}', which is outside the instance",
                config.id, config.target_path
            )
        })?;
        placements.push((mod_id, config, target_dir));
    }

    let mut result = MaterializedCustomConfigs::default();
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
            result.materialized_files.push(target);
        }
    }
    Ok(result)
}

/// Remove whatever file is at `target` and write a fresh copy of `source`.
/// Removing first matters: `fs::copy` onto a symlink or a hard link would write
/// through it, into the file it shares bytes with.
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
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{prepare_instance_config_directory, CachedConfigPlacement};

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-instance-configs-test-{timestamp}"))
    }

    #[test]
    fn prepares_nested_config_files_without_removing_unrelated_entries() {
        let root_dir = unique_test_root();
        let configs_cache_dir = root_dir.join("cache").join("configs");
        let cache_subdir = configs_cache_dir.join("hash_sodium_fabric_084");
        let instance_config_dir = root_dir.join("instance").join("config");

        fs::create_dir_all(cache_subdir.join("cloth-config"))
            .expect("cache subdirectories should be created");
        fs::create_dir_all(&instance_config_dir).expect("instance config dir should be created");

        fs::write(cache_subdir.join("sodium-options.json"), b"new sodium")
            .expect("sodium config should be written");
        fs::write(
            cache_subdir.join("cloth-config").join("client.toml"),
            b"cloth settings",
        )
        .expect("cloth config should be written");
        fs::write(instance_config_dir.join("generated.cfg"), b"keep me")
            .expect("unrelated config should be written");

        let prepared = prepare_instance_config_directory(
            &configs_cache_dir,
            &instance_config_dir,
            &[CachedConfigPlacement {
                cache_subdir: "hash_sodium_fabric_084".into(),
            }],
        )
        .expect("config preparation should succeed");

        assert_eq!(prepared.materialized_files.len(), 2);
        assert_eq!(
            fs::read(instance_config_dir.join("sodium-options.json"))
                .expect("sodium config should read"),
            b"new sodium"
        );
        assert_eq!(
            fs::read(instance_config_dir.join("cloth-config").join("client.toml"))
                .expect("nested config should read"),
            b"cloth settings"
        );
        assert_eq!(
            fs::read(instance_config_dir.join("generated.cfg"))
                .expect("unrelated config should remain"),
            b"keep me"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn later_config_placements_override_earlier_files() {
        let root_dir = unique_test_root();
        let configs_cache_dir = root_dir.join("cache").join("configs");
        let first_cache_subdir = configs_cache_dir.join("hash_first");
        let second_cache_subdir = configs_cache_dir.join("hash_second");
        let instance_config_dir = root_dir.join("instance").join("config");

        fs::create_dir_all(&first_cache_subdir).expect("first cache dir should be created");
        fs::create_dir_all(&second_cache_subdir).expect("second cache dir should be created");
        fs::write(first_cache_subdir.join("shared.toml"), b"first")
            .expect("first config should be written");
        fs::write(second_cache_subdir.join("shared.toml"), b"second")
            .expect("second config should be written");

        prepare_instance_config_directory(
            &configs_cache_dir,
            &instance_config_dir,
            &[
                CachedConfigPlacement {
                    cache_subdir: "hash_first".into(),
                },
                CachedConfigPlacement {
                    cache_subdir: "hash_second".into(),
                },
            ],
        )
        .expect("config preparation should succeed");

        assert_eq!(
            fs::read(instance_config_dir.join("shared.toml")).expect("shared config should read"),
            b"second"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn errors_when_required_config_cache_directory_is_missing() {
        let root_dir = unique_test_root();
        let configs_cache_dir = root_dir.join("cache").join("configs");
        let instance_config_dir = root_dir.join("instance").join("config");

        fs::create_dir_all(&configs_cache_dir).expect("configs cache dir should be created");

        let error = prepare_instance_config_directory(
            &configs_cache_dir,
            &instance_config_dir,
            &[CachedConfigPlacement {
                cache_subdir: "missing_config_set".into(),
            }],
        )
        .expect_err("config preparation should fail");

        assert!(error
            .to_string()
            .contains("required config cache directory 'missing_config_set' is missing"));

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
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

    #[test]
    fn a_target_path_outside_the_instance_is_refused() {
        for escaping in ["../outside", "/tmp/cubic-custom-config-absolute-target"] {
            let root = unique_test_root();
            let modlist_dir = root.join("Pack");
            let instance = root.join("instance");
            store(&modlist_dir, "cc-1", "evil.txt", b"evil");
            let rules = [rule_with(vec![config(
                "cc-1",
                &[],
                "any",
                escaping,
                &["evil.txt"],
            )])];

            let error =
                materialize_custom_configs(&modlist_dir, &instance, &rules, "1.21.1", "fabric")
                    .expect_err("an escaping target path must be refused");

            assert!(format!("{error:#}").contains(escaping), "{error:#}");
            assert!(!root.join("outside").join("evil.txt").exists());
            assert!(!Path::new(escaping).join("evil.txt").exists());
            fs::remove_dir_all(&root).unwrap();
        }
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
