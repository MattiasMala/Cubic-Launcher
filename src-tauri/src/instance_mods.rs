use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::path_safety::validate_path_component;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModJar {
    pub jar_filename: String,
    pub cache_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInstanceMods {
    pub linked_files: Vec<PathBuf>,
}

pub fn prepare_instance_mods_directory(
    _mods_cache_dir: &Path,
    instance_mods_dir: &Path,
    jars: &[CachedModJar],
) -> Result<PreparedInstanceMods> {
    for jar in jars {
        validate_path_component(&jar.jar_filename)?;
    }

    fs::create_dir_all(instance_mods_dir).with_context(|| {
        format!(
            "failed to create instance mods directory at {}",
            instance_mods_dir.display()
        )
    })?;

    clear_instance_mods_directory(instance_mods_dir)?;

    let mut seen_filenames = HashSet::new();
    let mut linked_files = Vec::new();

    for jar in jars {
        if !seen_filenames.insert(jar.jar_filename.clone()) {
            continue;
        }

        let source_path = &jar.cache_path;
        let target_path = instance_mods_dir.join(&jar.jar_filename);

        if !source_path.exists() {
            anyhow::bail!(
                "required cached mod JAR '{}' is missing from {}",
                jar.jar_filename,
                source_path.display()
            );
        }

        create_file_link(source_path, &target_path)?;
        linked_files.push(target_path);
    }

    Ok(PreparedInstanceMods { linked_files })
}

pub fn clear_instance_mods_directory(instance_mods_dir: &Path) -> Result<()> {
    fs::create_dir_all(instance_mods_dir).with_context(|| {
        format!(
            "failed to create instance mods directory at {}",
            instance_mods_dir.display()
        )
    })?;

    for entry in fs::read_dir(instance_mods_dir).with_context(|| {
        format!(
            "failed to read instance mods directory at {}",
            instance_mods_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect entry inside {}",
                instance_mods_dir.display()
            )
        })?;

        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;

        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove directory {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove file {}", path.display()))?;
        }
    }

    Ok(())
}

pub fn create_file_link(source_path: &Path, target_path: &Path) -> Result<()> {
    if target_path.exists() {
        fs::remove_file(target_path).with_context(|| {
            format!("failed to remove existing target {}", target_path.display())
        })?;
    }

    create_symlink_file(source_path, target_path).or_else(|symlink_error| {
        fs::hard_link(source_path, target_path).with_context(|| {
            format!(
                "failed to create link from {} to {} after symlink failure: {symlink_error}",
                source_path.display(),
                target_path.display()
            )
        })
    })
}

#[cfg(target_family = "unix")]
fn create_symlink_file(source_path: &Path, target_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source_path, target_path).with_context(|| {
        format!(
            "failed to create symlink from {} to {}",
            target_path.display(),
            source_path.display()
        )
    })
}

#[cfg(target_family = "windows")]
fn create_symlink_file(source_path: &Path, target_path: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(source_path, target_path).with_context(|| {
        format!(
            "failed to create symlink from {} to {}",
            target_path.display(),
            source_path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{clear_instance_mods_directory, prepare_instance_mods_directory, CachedModJar};

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-instance-mods-test-{timestamp}"))
    }

    #[test]
    fn clear_instance_mods_directory_removes_existing_entries() {
        let root_dir = unique_test_root();
        let instance_mods_dir = root_dir.join("instance").join("mods");

        fs::create_dir_all(&instance_mods_dir).expect("instance mods dir should be created");
        fs::write(instance_mods_dir.join("old-mod.jar"), b"old").expect("old file should exist");
        fs::create_dir_all(instance_mods_dir.join("nested-dir"))
            .expect("nested directory should exist");

        clear_instance_mods_directory(&instance_mods_dir).expect("cleanup should succeed");

        let entries = fs::read_dir(&instance_mods_dir)
            .expect("instance mods dir should be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("entries should collect");

        assert!(
            entries.is_empty(),
            "instance mods dir should be empty after cleanup"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn prepare_instance_mods_directory_recreates_links_from_cache() {
        let root_dir = unique_test_root();
        let mods_cache_dir = root_dir.join("cache").join("mods");
        let instance_mods_dir = root_dir
            .join("mod-lists")
            .join("Pack")
            .join("instances")
            .join("1.21.1-Fabric")
            .join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache dir should be created");
        fs::create_dir_all(&instance_mods_dir).expect("instance mods dir should be created");
        fs::write(mods_cache_dir.join("sodium.jar"), b"sodium")
            .expect("cached sodium jar should exist");
        fs::write(mods_cache_dir.join("fabric-api.jar"), b"fabric-api")
            .expect("cached fabric-api jar should exist");
        fs::write(instance_mods_dir.join("stale-mod.jar"), b"stale")
            .expect("stale file should exist before cleanup");

        let prepared = prepare_instance_mods_directory(
            &mods_cache_dir,
            &instance_mods_dir,
            &[
                CachedModJar {
                    jar_filename: "sodium.jar".into(),
                    cache_path: mods_cache_dir.join("sodium.jar"),
                },
                CachedModJar {
                    jar_filename: "fabric-api.jar".into(),
                    cache_path: mods_cache_dir.join("fabric-api.jar"),
                },
                CachedModJar {
                    jar_filename: "fabric-api.jar".into(),
                    cache_path: mods_cache_dir.join("fabric-api.jar"),
                },
            ],
        )
        .expect("instance mod preparation should succeed");

        assert_eq!(prepared.linked_files.len(), 2);
        assert!(!instance_mods_dir.join("stale-mod.jar").exists());
        assert!(instance_mods_dir.join("sodium.jar").exists());
        assert!(instance_mods_dir.join("fabric-api.jar").exists());
        assert_eq!(
            fs::read(instance_mods_dir.join("sodium.jar")).expect("linked sodium file should read"),
            b"sodium"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn prepare_instance_mods_directory_fails_when_cached_jar_is_missing() {
        let root_dir = unique_test_root();
        let mods_cache_dir = root_dir.join("cache").join("mods");
        let instance_mods_dir = root_dir.join("instance").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache dir should be created");

        let error = prepare_instance_mods_directory(
            &mods_cache_dir,
            &instance_mods_dir,
            &[CachedModJar {
                jar_filename: "missing.jar".into(),
                cache_path: mods_cache_dir.join("missing.jar"),
            }],
        )
        .expect_err("preparation should fail for missing cache file");

        assert!(error
            .to_string()
            .contains("required cached mod JAR 'missing.jar' is missing"));

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn prepare_rejects_traversal_jar_filename() {
        let root_dir = unique_test_root();
        let mods_cache_dir = root_dir.join("cache").join("mods");
        let instance_mods_dir = root_dir.join("instance").join("mods");
        let escaped_path = root_dir.join("instance").join("evil.jar");
        let source_path = mods_cache_dir.join("evil-source.jar");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache dir should be created");
        fs::write(&source_path, b"evil").expect("malicious source fixture should exist");

        let result = prepare_instance_mods_directory(
            &mods_cache_dir,
            &instance_mods_dir,
            &[CachedModJar {
                jar_filename: "../evil.jar".into(),
                cache_path: source_path,
            }],
        );

        assert!(result.is_err(), "traversal filename should be rejected");
        assert!(
            !escaped_path.exists(),
            "rejected filename must not create a link outside the mods directory"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

}
