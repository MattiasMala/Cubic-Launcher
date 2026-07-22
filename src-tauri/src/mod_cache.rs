use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha1::{Digest, Sha1};

// Mod artifact cache and lookup helpers.
//
// Cache records are keyed by canonical Modrinth project/version ids plus the
// target Minecraft version and loader. The target is part of the key because
// the same Modrinth version id can be reused across loaders while requiring a
// different on-disk cache path. User rules can still be stored as Modrinth
// slugs; the project alias table bridges those identifiers for cache-only
// launch.
use crate::modrinth::ModrinthVersion;
use crate::path_safety::validate_path_component;
use crate::resolver::ResolutionTarget;

pub fn validate_cache_key(
    mod_loader: &str,
    version_id: &str,
    jar_filename: &str,
) -> Result<()> {
    validate_path_component(mod_loader)?;
    validate_path_component(version_id)?;
    validate_path_component(jar_filename)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModCacheRecord {
    pub modrinth_project_id: String,
    pub modrinth_version_id: String,
    pub jar_filename: String,
    pub mc_version: String,
    pub mod_loader: String,
    pub file_hash: Option<String>,
    pub download_url: Option<String>,
    pub is_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDownload {
    pub modrinth_project_id: String,
    pub modrinth_version_id: String,
    pub jar_filename: String,
    pub mc_version: String,
    pub mod_loader: String,
    pub file_hash: Option<String>,
    pub download_url: String,
    pub file_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModAcquisitionPlan {
    pub cached: Vec<ModCacheRecord>,
    pub to_download: Vec<PendingDownload>,
}

pub fn cached_remote_artifact_path(
    mods_cache_dir: &Path,
    mod_loader: &str,
    version_id: &str,
    jar_filename: &str,
) -> PathBuf {
    mods_cache_dir
        .join(mod_loader)
        .join(version_id)
        .join(jar_filename)
}

pub fn cached_local_artifact_path(
    mods_cache_dir: &Path,
    mod_loader: &str,
    jar_filename: &str,
) -> PathBuf {
    mods_cache_dir
        .join(mod_loader)
        .join("local")
        .join(jar_filename)
}

pub fn legacy_cached_artifact_path(mods_cache_dir: &Path, jar_filename: &str) -> PathBuf {
    mods_cache_dir.join(jar_filename)
}

pub fn cached_artifact_path_for_record(mods_cache_dir: &Path, record: &ModCacheRecord) -> PathBuf {
    if record.is_local {
        cached_local_artifact_path(mods_cache_dir, &record.mod_loader, &record.jar_filename)
    } else {
        cached_remote_artifact_path(
            mods_cache_dir,
            &record.mod_loader,
            &record.modrinth_version_id,
            &record.jar_filename,
        )
    }
}

pub fn cached_artifact_path_for_pending_download(
    mods_cache_dir: &Path,
    pending: &PendingDownload,
) -> PathBuf {
    cached_remote_artifact_path(
        mods_cache_dir,
        &pending.mod_loader,
        &pending.modrinth_version_id,
        &pending.jar_filename,
    )
}

pub trait ModCacheLookup {
    fn find_by_version_id(
        &self,
        version_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>>;
}

pub struct SqliteModCacheRepository<'connection> {
    connection: &'connection Connection,
    mods_cache_dir: PathBuf,
}

impl<'connection> SqliteModCacheRepository<'connection> {
    pub fn new(connection: &'connection Connection, mods_cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            connection,
            mods_cache_dir: mods_cache_dir.into(),
        }
    }

    pub fn upsert_modrinth_version(
        &self,
        version: &ModrinthVersion,
        target: &ResolutionTarget,
    ) -> Result<ModCacheRecord> {
        let record = cache_record_from_version(version, target)?;

        self.connection.execute(
            r#"
            INSERT INTO mod_cache (
                modrinth_project_id,
                modrinth_version_id,
                jar_filename,
                mc_version,
                mod_loader,
                file_hash,
                download_url,
                is_local
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(modrinth_version_id, mc_version, mod_loader) DO UPDATE SET
                modrinth_project_id = excluded.modrinth_project_id,
                jar_filename = excluded.jar_filename,
                file_hash = excluded.file_hash,
                download_url = excluded.download_url,
                is_local = excluded.is_local
            "#,
            params![
                &record.modrinth_project_id,
                &record.modrinth_version_id,
                &record.jar_filename,
                &record.mc_version,
                &record.mod_loader,
                &record.file_hash,
                &record.download_url,
                record.is_local,
            ],
        )?;

        Ok(record)
    }

    pub fn find_compatible_by_project(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT
                modrinth_project_id,
                modrinth_version_id,
                jar_filename,
                mc_version,
                mod_loader,
                file_hash,
                download_url,
                is_local
            FROM mod_cache
            WHERE modrinth_project_id = ?1
              AND mc_version = ?2
              AND mod_loader = ?3
            ORDER BY rowid DESC
            "#,
        )?;

        let rows = statement.query_map(
            params![
                project_id,
                &target.minecraft_version,
                target.mod_loader.as_modrinth_loader(),
            ],
            |row| {
                Ok(ModCacheRecord {
                    modrinth_project_id: row.get(0)?,
                    modrinth_version_id: row.get(1)?,
                    jar_filename: row.get(2)?,
                    mc_version: row.get(3)?,
                    mod_loader: row.get(4)?,
                    file_hash: row.get(5)?,
                    download_url: row.get(6)?,
                    is_local: row.get(7)?,
                })
            },
        )?;

        for row in rows {
            let record = row?;
            if let Some(record) = self.ensure_record_file_available(record)? {
                return Ok(Some(record));
            }
        }

        Ok(None)
    }

    pub fn upsert_project_alias(&self, alias: &str, canonical_project_id: &str) -> Result<()> {
        let alias = alias.trim();
        let canonical_project_id = canonical_project_id.trim();
        if alias.is_empty() || canonical_project_id.is_empty() || alias == canonical_project_id {
            return Ok(());
        }

        self.connection.execute(
            r#"
            INSERT INTO modrinth_project_aliases (alias, canonical_project_id)
            VALUES (?1, ?2)
            ON CONFLICT(alias) DO UPDATE SET
                canonical_project_id = excluded.canonical_project_id
            "#,
            params![alias, canonical_project_id],
        )?;

        Ok(())
    }

    pub fn find_canonical_project_id(&self, alias: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                r#"
                SELECT canonical_project_id
                FROM modrinth_project_aliases
                WHERE alias = ?1
                "#,
                [alias.trim()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn find_compatible_by_project_or_alias(
        &self,
        project_id_or_alias: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        if let Some(record) = self.find_compatible_by_project(project_id_or_alias, target)? {
            return Ok(Some(record));
        }

        let Some(canonical_project_id) = self.find_canonical_project_id(project_id_or_alias)?
        else {
            return Ok(None);
        };

        if canonical_project_id == project_id_or_alias.trim() {
            return Ok(None);
        }

        self.find_compatible_by_project(&canonical_project_id, target)
    }
}

impl ModCacheLookup for SqliteModCacheRepository<'_> {
    fn find_by_version_id(
        &self,
        version_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT
                modrinth_project_id,
                modrinth_version_id,
                jar_filename,
                mc_version,
                mod_loader,
                file_hash,
                download_url,
                is_local
            FROM mod_cache
            WHERE modrinth_version_id = ?1
              AND mc_version = ?2
              AND mod_loader = ?3
            "#,
        )?;

        let record = statement
            .query_row(
                params![
                    version_id,
                    &target.minecraft_version,
                    target.mod_loader.as_modrinth_loader(),
                ],
                |row| {
                    Ok(ModCacheRecord {
                        modrinth_project_id: row.get(0)?,
                        modrinth_version_id: row.get(1)?,
                        jar_filename: row.get(2)?,
                        mc_version: row.get(3)?,
                        mod_loader: row.get(4)?,
                        file_hash: row.get(5)?,
                        download_url: row.get(6)?,
                        is_local: row.get(7)?,
                    })
                },
            )
            .optional()?;

        match record {
            Some(record) => self.ensure_record_file_available(record),
            None => Ok(None),
        }
    }
}

impl SqliteModCacheRepository<'_> {
    fn ensure_record_file_available(
        &self,
        record: ModCacheRecord,
    ) -> Result<Option<ModCacheRecord>> {
        validate_cache_key(
            &record.mod_loader,
            &record.modrinth_version_id,
            &record.jar_filename,
        )?;

        let artifact_path = cached_artifact_path_for_record(&self.mods_cache_dir, &record);
        if artifact_path.exists() {
            return Ok(Some(record));
        }

        let legacy_path = legacy_cached_artifact_path(&self.mods_cache_dir, &record.jar_filename);
        if !legacy_path.exists() {
            return Ok(None);
        }

        if !legacy_file_matches_record(&legacy_path, &record)? {
            return Ok(None);
        }

        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create artifact cache directory {}",
                    parent.display()
                )
            })?;
        }

        move_or_copy_file(&legacy_path, &artifact_path)?;
        Ok(Some(record))
    }
}

fn legacy_file_matches_record(path: &Path, record: &ModCacheRecord) -> Result<bool> {
    if record.is_local {
        return Ok(true);
    }

    let Some(expected_sha1) = record.file_hash.as_deref() else {
        return Ok(false);
    };

    Ok(sha1_of_file(path)?.eq_ignore_ascii_case(expected_sha1))
}

fn move_or_copy_file(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            fs::copy(source, destination).with_context(|| {
                format!(
                    "failed to copy {} to {} after rename failure: {rename_error}",
                    source.display(),
                    destination.display()
                )
            })?;
            fs::remove_file(source).with_context(|| {
                format!(
                    "failed to remove legacy cached artifact {} after copying to {}",
                    source.display(),
                    destination.display()
                )
            })?;
            Ok(())
        }
    }
}

fn sha1_of_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

pub fn build_mod_acquisition_plan(
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
    cache_lookup: &impl ModCacheLookup,
) -> Result<ModAcquisitionPlan> {
    let mut seen_version_ids = HashSet::new();
    let mut cached = Vec::new();
    let mut to_download = Vec::new();

    for version in versions {
        if !seen_version_ids.insert(version.id.clone()) {
            continue;
        }

        match cache_lookup.find_by_version_id(&version.id, target)? {
            Some(record) => cached.push(record),
            None => to_download.push(pending_download_from_version(version, target)?),
        }
    }

    Ok(ModAcquisitionPlan {
        cached,
        to_download,
    })
}

pub fn cache_record_from_version(
    version: &ModrinthVersion,
    target: &ResolutionTarget,
) -> Result<ModCacheRecord> {
    let primary_file = version.primary_file().with_context(|| {
        format!(
            "version '{}' for project '{}' does not expose any downloadable file",
            version.id, version.project_id
        )
    })?;

    let mod_loader = target.mod_loader.as_modrinth_loader();
    validate_cache_key(mod_loader, &version.id, &primary_file.filename)?;

    Ok(ModCacheRecord {
        modrinth_project_id: version.project_id.clone(),
        modrinth_version_id: version.id.clone(),
        jar_filename: primary_file.filename.clone(),
        mc_version: target.minecraft_version.clone(),
        mod_loader: mod_loader.to_string(),
        file_hash: primary_file.hashes.get("sha1").cloned(),
        download_url: Some(primary_file.url.clone()),
        is_local: false,
    })
}

pub fn pending_download_from_version(
    version: &ModrinthVersion,
    target: &ResolutionTarget,
) -> Result<PendingDownload> {
    let primary_file = version.primary_file().with_context(|| {
        format!(
            "version '{}' for project '{}' does not expose any downloadable file",
            version.id, version.project_id
        )
    })?;

    let mod_loader = target.mod_loader.as_modrinth_loader();
    validate_cache_key(mod_loader, &version.id, &primary_file.filename)?;

    Ok(PendingDownload {
        modrinth_project_id: version.project_id.clone(),
        modrinth_version_id: version.id.clone(),
        jar_filename: primary_file.filename.clone(),
        mc_version: target.minecraft_version.clone(),
        mod_loader: mod_loader.to_string(),
        file_hash: primary_file.hashes.get("sha1").cloned(),
        download_url: primary_file.url.clone(),
        file_size: primary_file.size,
    })
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use rusqlite::Connection;
    use sha1::{Digest, Sha1};

    use crate::database::initialize_database;
    use crate::modrinth::ModrinthVersion;
    use crate::resolver::{ModLoader, ResolutionTarget};

    use super::{
        build_mod_acquisition_plan, cache_record_from_version, cached_artifact_path_for_record,
        legacy_cached_artifact_path, pending_download_from_version, validate_cache_key,
        ModCacheLookup, ModCacheRecord, SqliteModCacheRepository,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-mod-cache-test-{timestamp}"))
    }

    fn target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    fn version(project_id: &str, version_id: &str, filename: &str) -> ModrinthVersion {
        serde_json::from_str(&format!(
            r#"{{
              "id": "{version_id}",
              "project_id": "{project_id}",
              "version_number": "1.0.0",
              "name": "{project_id}",
              "game_versions": ["1.21.1"],
              "loaders": ["fabric"],
              "date_published": "2024-08-01T10:00:00.000Z",
              "dependencies": [],
              "files": [
                {{
                  "hashes": {{ "sha1": "{version_id}-sha1" }},
                  "url": "https://cdn.modrinth.com/data/{project_id}/{filename}",
                  "filename": "{filename}",
                  "primary": true,
                  "size": 100
                }}
              ]
            }}"#
        ))
        .expect("version json should deserialize")
    }

    struct InMemoryLookup {
        records: Vec<super::ModCacheRecord>,
    }

    impl ModCacheLookup for InMemoryLookup {
        fn find_by_version_id(
            &self,
            version_id: &str,
            _target: &ResolutionTarget,
        ) -> Result<Option<super::ModCacheRecord>> {
            Ok(self
                .records
                .iter()
                .find(|record| record.modrinth_version_id == version_id)
                .cloned())
        }
    }

    #[test]
    fn cache_key_rejects_traversal_filename() {
        assert!(validate_cache_key("fabric", "version-1", "../../x.jar").is_err());
        assert!(validate_cache_key("fabric", "..", "safe.jar").is_err());
        assert!(validate_cache_key("../fabric", "version-1", "safe.jar").is_err());
        assert!(validate_cache_key("fabric", "version-1", "safe.jar").is_ok());
    }

    #[test]
    fn cache_record_and_pending_download_use_primary_file_metadata() {
        let version = version("sodium", "version-1", "sodium.jar");

        let record = cache_record_from_version(&version, &target()).expect("record should build");
        let pending =
            pending_download_from_version(&version, &target()).expect("download should build");

        assert_eq!(record.jar_filename, "sodium.jar");
        assert_eq!(record.file_hash.as_deref(), Some("version-1-sha1"));
        assert_eq!(
            pending.download_url,
            "https://cdn.modrinth.com/data/sodium/sodium.jar"
        );
    }

    #[test]
    fn repository_returns_cache_hit_only_when_file_exists() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let version = version("sodium", "version-1", "sodium.jar");

        repository
            .upsert_modrinth_version(&version, &target())
            .expect("cache record should insert");

        assert!(repository
            .find_by_version_id("version-1", &target())
            .expect("lookup should succeed")
            .is_none());

        let record = cache_record_from_version(&version, &target()).expect("record should build");
        let artifact_path = cached_artifact_path_for_record(&mods_cache_dir, &record);
        fs::create_dir_all(
            artifact_path
                .parent()
                .expect("artifact parent directory should exist"),
        )
        .expect("artifact parent directory should be created");
        fs::write(&artifact_path, b"jar").expect("jar should be written");

        let record = repository
            .find_by_version_id("version-1", &target())
            .expect("lookup should succeed")
            .expect("record should exist once file exists");

        assert_eq!(record.modrinth_project_id, "sodium");
        assert_eq!(record.jar_filename, "sodium.jar");

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn repository_finds_compatible_project_record_for_target() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let old_version = version("sodium", "version-older", "sodium-old.jar");
        let new_version = version("sodium", "version-newer", "sodium-new.jar");

        repository
            .upsert_modrinth_version(&old_version, &target())
            .expect("older cache record should insert");
        repository
            .upsert_modrinth_version(&new_version, &target())
            .expect("newer cache record should insert");

        let old_record =
            cache_record_from_version(&old_version, &target()).expect("old record should build");
        let new_record =
            cache_record_from_version(&new_version, &target()).expect("new record should build");
        let old_path = cached_artifact_path_for_record(&mods_cache_dir, &old_record);
        let new_path = cached_artifact_path_for_record(&mods_cache_dir, &new_record);
        fs::create_dir_all(old_path.parent().expect("old parent should exist"))
            .expect("old artifact parent should be created");
        fs::create_dir_all(new_path.parent().expect("new parent should exist"))
            .expect("new artifact parent should be created");
        fs::write(old_path, b"old").expect("old jar should exist");
        fs::write(new_path, b"new").expect("new jar should exist");

        let record = repository
            .find_compatible_by_project("sodium", &target())
            .expect("compatible lookup should succeed")
            .expect("compatible record should exist");

        assert_eq!(record.modrinth_project_id, "sodium");
        assert_eq!(record.modrinth_version_id, "version-newer");

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn repository_finds_compatible_project_record_through_alias() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let canonical_version = version("canonical-sodium", "version-1", "sodium.jar");

        repository
            .upsert_modrinth_version(&canonical_version, &target())
            .expect("cache record should insert");
        repository
            .upsert_project_alias("sodium", "canonical-sodium")
            .expect("project alias should insert");

        let record = cache_record_from_version(&canonical_version, &target())
            .expect("canonical record should build");
        let artifact_path = cached_artifact_path_for_record(&mods_cache_dir, &record);
        fs::create_dir_all(
            artifact_path
                .parent()
                .expect("artifact parent should exist"),
        )
        .expect("artifact parent should be created");
        fs::write(artifact_path, b"jar").expect("jar should exist");

        let record = repository
            .find_compatible_by_project_or_alias("sodium", &target())
            .expect("compatible alias lookup should succeed")
            .expect("compatible record should exist");

        assert_eq!(record.modrinth_project_id, "canonical-sodium");
        assert_eq!(record.modrinth_version_id, "version-1");

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn repository_migrates_matching_legacy_flat_cache_file() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let version = version("sodium", "version-1", "sodium.jar");
        let record = repository
            .upsert_modrinth_version(&version, &target())
            .expect("cache record should insert");
        let legacy_bytes = b"legacy-jar";
        let legacy_sha1 = format!("{:x}", Sha1::digest(legacy_bytes));
        connection
            .execute(
                "UPDATE mod_cache SET file_hash = ?1 WHERE modrinth_version_id = ?2",
                [legacy_sha1.as_str(), "version-1"],
            )
            .expect("test record hash should be updated");

        let legacy_path = legacy_cached_artifact_path(&mods_cache_dir, &record.jar_filename);
        fs::write(&legacy_path, legacy_bytes).expect("legacy file should be written for migration");

        let migrated = repository
            .find_by_version_id("version-1", &target())
            .expect("lookup should succeed")
            .expect("record should be returned after migration");

        let artifact_path = cached_artifact_path_for_record(&mods_cache_dir, &migrated);
        assert!(
            artifact_path.exists(),
            "artifact should be moved to new cache path"
        );
        assert!(
            !legacy_path.exists(),
            "legacy flat cache file should be removed after migration"
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn artifact_paths_differ_for_same_filename_across_loaders() {
        let mods_cache_dir = unique_test_root().join("cache").join("mods");

        let fabric_record = ModCacheRecord {
            modrinth_project_id: "cloth-config".into(),
            modrinth_version_id: "fabric-version".into(),
            jar_filename: "cloth-config-26.1.154.jar".into(),
            mc_version: "26.1.2".into(),
            mod_loader: "fabric".into(),
            file_hash: Some("fabric-sha1".into()),
            download_url: Some("https://example.invalid/fabric".into()),
            is_local: false,
        };
        let neoforge_record = ModCacheRecord {
            modrinth_project_id: "cloth-config".into(),
            modrinth_version_id: "neoforge-version".into(),
            jar_filename: "cloth-config-26.1.154.jar".into(),
            mc_version: "26.1.2".into(),
            mod_loader: "neoforge".into(),
            file_hash: Some("neoforge-sha1".into()),
            download_url: Some("https://example.invalid/neoforge".into()),
            is_local: false,
        };

        let fabric_path = cached_artifact_path_for_record(&mods_cache_dir, &fabric_record);
        let neoforge_path = cached_artifact_path_for_record(&mods_cache_dir, &neoforge_record);

        assert_ne!(fabric_path, neoforge_path);
        assert!(
            fabric_path.to_string_lossy().contains("fabric-version"),
            "fabric path should be keyed by version id"
        );
        assert!(
            neoforge_path.to_string_lossy().contains("neoforge-version"),
            "neoforge path should be keyed by version id"
        );
    }

    #[test]
    fn acquisition_plan_splits_cached_and_missing_versions_and_deduplicates() {
        let cached_version = version("sodium", "version-cached", "sodium.jar");
        let missing_version = version("fabric-api", "version-missing", "fabric-api.jar");

        let lookup = InMemoryLookup {
            records: vec![cache_record_from_version(&cached_version, &target())
                .expect("cache record should build")],
        };

        let plan = build_mod_acquisition_plan(
            &[
                cached_version.clone(),
                missing_version.clone(),
                missing_version.clone(),
            ],
            &target(),
            &lookup,
        )
        .expect("plan should build");

        assert_eq!(plan.cached.len(), 1);
        assert_eq!(plan.cached[0].modrinth_version_id, "version-cached");
        assert_eq!(plan.to_download.len(), 1);
        assert_eq!(plan.to_download[0].modrinth_version_id, "version-missing");
    }
}
