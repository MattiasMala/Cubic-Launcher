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

/// What the cache holds for a project or a version on one target.
///
/// The distinction the `find_*` lookups cannot express: they answer `None`
/// both when a mod was never cached and when its row is intact but the jar is
/// gone, so the second case is indistinguishable from the first and the mod
/// simply leaves the game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheProbe {
    /// A row whose jar is in the cache directory.
    Ready(ModCacheRecord),
    /// A row whose jar is gone, carrying the url and the hash needed to fetch
    /// that exact version again — no API call required.
    JarMissing(ModCacheRecord),
    /// A row whose jar is gone and that nothing here can restore: a locally
    /// copied jar has no Modrinth url, and neither has a row saved without one.
    JarMissingUnrecoverable(ModCacheRecord),
    /// No row for this target.
    NotCached,
}

/// What a row whose jar is gone allows. Pure, so the two callers of the probes
/// share one definition of "restorable".
pub fn classify_missing_jar(record: ModCacheRecord) -> CacheProbe {
    let restorable = record
        .download_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty());

    if record.is_local || !restorable {
        CacheProbe::JarMissingUnrecoverable(record)
    } else {
        CacheProbe::JarMissing(record)
    }
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

/// The eight `mod_cache` columns in the order every record query selects them.
fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModCacheRecord> {
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

    /// The cached sha1 for a project on this target, **without** requiring the
    /// jar to be on disk.
    ///
    /// `find_compatible_by_project` deliberately hides a row whose artifact is
    /// gone (`ensure_record_file_available`), because its callers need a file to
    /// link. A hash names a Modrinth *version*, not a local file, so "which
    /// version is newest for the project this file came from" stays answerable
    /// with an emptied cache directory. Reusing the disk-checking lookup here
    /// would push every mod onto the per-project path exactly when the bulk
    /// path matters most.
    ///
    /// The hash is lowercased here, once: `POST /v2/version_files/update` omits
    /// an uppercase hash in silence, indistinguishably from an unknown one.
    pub fn find_cached_file_hash_by_project(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<String>> {
        let hash = self
            .connection
            .query_row(
                r#"
                SELECT file_hash
                FROM mod_cache
                WHERE modrinth_project_id = ?1
                  AND mc_version = ?2
                  AND mod_loader = ?3
                  AND is_local = 0
                  AND file_hash IS NOT NULL
                  AND trim(file_hash) <> ''
                ORDER BY rowid DESC
                LIMIT 1
                "#,
                params![
                    project_id.trim(),
                    &target.minecraft_version,
                    target.mod_loader.as_modrinth_loader(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(hash.map(|hash| hash.trim().to_ascii_lowercase()))
    }

    /// Same bridge as `find_compatible_by_project_or_alias`: user rules can name
    /// a slug, `modrinth_project_aliases` maps it to the canonical project id.
    pub fn find_cached_file_hash_by_project_or_alias(
        &self,
        project_id_or_alias: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<String>> {
        if let Some(hash) = self.find_cached_file_hash_by_project(project_id_or_alias, target)? {
            return Ok(Some(hash));
        }

        let Some(canonical_project_id) = self.find_canonical_project_id(project_id_or_alias)?
        else {
            return Ok(None);
        };

        if canonical_project_id == project_id_or_alias.trim() {
            return Ok(None);
        }

        self.find_cached_file_hash_by_project(&canonical_project_id, target)
    }

    /// The `modrinth_version_id` registered for a project on this target,
    /// **without** requiring the jar to be on disk.
    ///
    /// Same reason as `find_cached_file_hash_by_project`: the pre-check has to
    /// report which version is *registered* — that is what "0.5.8 → 0.6.0"
    /// means — and not whether its file survived.
    ///
    /// It also has to be **the same row** that lookup picks, or the reported
    /// "current" version would not be the one whose hash produced the
    /// candidate. The primary key is `(modrinth_version_id, mc_version,
    /// mod_loader)`, so one project and target can own several rows; the hash
    /// lookup takes the newest row *that has a usable hash*, which is why the
    /// hash predicate is repeated here as the first ordering key instead of as
    /// a filter. Whenever a hashed row exists both lookups land on it; with no
    /// hashed row the version id is still reported, which is the honest answer
    /// for a mod on the per-project path.
    ///
    /// `is_local = 1` rows are excluded: a locally copied jar carries a
    /// synthetic version id, and `GET /versions?ids=` rejects the whole request
    /// on one non-base62 id.
    pub fn find_cached_version_id_by_project(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<String>> {
        let version_id = self
            .connection
            .query_row(
                r#"
                SELECT modrinth_version_id
                FROM mod_cache
                WHERE modrinth_project_id = ?1
                  AND mc_version = ?2
                  AND mod_loader = ?3
                  AND is_local = 0
                  AND modrinth_version_id IS NOT NULL
                  AND trim(modrinth_version_id) <> ''
                ORDER BY
                    (file_hash IS NOT NULL AND trim(file_hash) <> '') DESC,
                    rowid DESC
                LIMIT 1
                "#,
                params![
                    project_id.trim(),
                    &target.minecraft_version,
                    target.mod_loader.as_modrinth_loader(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(version_id.map(|version_id| version_id.trim().to_string()))
    }

    /// Same slug → canonical id bridge as
    /// `find_cached_file_hash_by_project_or_alias`.
    pub fn find_cached_version_id_by_project_or_alias(
        &self,
        project_id_or_alias: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<String>> {
        if let Some(version_id) = self.find_cached_version_id_by_project(project_id_or_alias, target)?
        {
            return Ok(Some(version_id));
        }

        let Some(canonical_project_id) = self.find_canonical_project_id(project_id_or_alias)?
        else {
            return Ok(None);
        };

        if canonical_project_id == project_id_or_alias.trim() {
            return Ok(None);
        }

        self.find_cached_version_id_by_project(&canonical_project_id, target)
    }

    /// The row a launch would use for a project on this target, **without**
    /// requiring the jar to be on disk.
    ///
    /// Ordering matches `find_cached_version_id_by_project` — hashed rows
    /// first, then newest — so "the registered version" means the same thing
    /// whether the pre-check reports it or the launch restores it. Unlike that
    /// lookup it keeps `is_local = 1` rows: a local jar that vanished has to be
    /// named to be reported, even though nothing can re-download it.
    pub fn find_registered_by_project(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        self.connection
            .query_row(
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
                ORDER BY
                    (file_hash IS NOT NULL AND trim(file_hash) <> '') DESC,
                    rowid DESC
                LIMIT 1
                "#,
                params![
                    project_id.trim(),
                    &target.minecraft_version,
                    target.mod_loader.as_modrinth_loader(),
                ],
                record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Same slug → canonical id bridge as `find_compatible_by_project_or_alias`.
    pub fn find_registered_by_project_or_alias(
        &self,
        project_id_or_alias: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        if let Some(record) = self.find_registered_by_project(project_id_or_alias, target)? {
            return Ok(Some(record));
        }

        let Some(canonical_project_id) = self.find_canonical_project_id(project_id_or_alias)?
        else {
            return Ok(None);
        };

        if canonical_project_id == project_id_or_alias.trim() {
            return Ok(None);
        }

        self.find_registered_by_project(&canonical_project_id, target)
    }

    /// The row for one version id on this target, **without** requiring the jar
    /// to be on disk. `find_by_version_id` is this plus the file check.
    pub fn find_registered_by_version_id(
        &self,
        version_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        self.connection
            .query_row(
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
                params![
                    version_id,
                    &target.minecraft_version,
                    target.mod_loader.as_modrinth_loader(),
                ],
                record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Everything the cache can say about a project on this target, in one
    /// answer: the usable row, the registered row whose jar disappeared, or
    /// nothing at all.
    ///
    /// The disk-checking lookup runs first and unchanged, so a project with
    /// several rows still launches from the newest one that kept its file.
    pub fn probe_project(
        &self,
        project_id_or_alias: &str,
        target: &ResolutionTarget,
    ) -> Result<CacheProbe> {
        if let Some(record) = self.find_compatible_by_project_or_alias(project_id_or_alias, target)?
        {
            return Ok(CacheProbe::Ready(record));
        }

        match self.find_registered_by_project_or_alias(project_id_or_alias, target)? {
            Some(record) => Ok(classify_missing_jar(record)),
            None => Ok(CacheProbe::NotCached),
        }
    }

    /// Same three answers for one exact version id — the version a pre-check
    /// handed down, which must not be traded for another one.
    pub fn probe_version(
        &self,
        version_id: &str,
        target: &ResolutionTarget,
    ) -> Result<CacheProbe> {
        if let Some(record) = self.find_by_version_id(version_id, target)? {
            return Ok(CacheProbe::Ready(record));
        }

        match self.find_registered_by_version_id(version_id, target)? {
            Some(record) => Ok(classify_missing_jar(record)),
            None => Ok(CacheProbe::NotCached),
        }
    }
}

impl ModCacheLookup for SqliteModCacheRepository<'_> {
    fn find_by_version_id(
        &self,
        version_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModCacheRecord>> {
        match self.find_registered_by_version_id(version_id, target)? {
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

/// The download that puts a registered version back on disk.
///
/// Everything comes from the row itself — url, file name, sha1 — so what
/// arrives is the version the cache claims to hold and not the newest one, and
/// no API call is involved. `file_size` is unknown (`mod_cache` does not store
/// it) and only feeds the progress total, which counts files when it is zero.
pub fn pending_download_from_record(record: &ModCacheRecord) -> Result<PendingDownload> {
    let download_url = record
        .download_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .with_context(|| {
            format!(
                "cached version '{}' of project '{}' has no download url to restore '{}' from",
                record.modrinth_version_id, record.modrinth_project_id, record.jar_filename
            )
        })?;

    validate_cache_key(
        &record.mod_loader,
        &record.modrinth_version_id,
        &record.jar_filename,
    )?;

    Ok(PendingDownload {
        modrinth_project_id: record.modrinth_project_id.clone(),
        modrinth_version_id: record.modrinth_version_id.clone(),
        jar_filename: record.jar_filename.clone(),
        mc_version: record.mc_version.clone(),
        mod_loader: record.mod_loader.clone(),
        file_hash: record.file_hash.clone(),
        download_url: download_url.to_string(),
        file_size: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use rusqlite::Connection;
    use sha1::{Digest, Sha1};

    use crate::database::initialize_database;
    use crate::modrinth::ModrinthVersion;
    use crate::resolver::{ModLoader, ResolutionTarget};

    use super::{
        build_mod_acquisition_plan, cache_record_from_version, cached_artifact_path_for_record,
        legacy_cached_artifact_path, pending_download_from_record, pending_download_from_version,
        validate_cache_key, CacheProbe, ModCacheLookup, ModCacheRecord, SqliteModCacheRepository,
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
              "version_type": "release",
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
    fn hash_lookup_answers_for_an_intact_row_whose_jar_is_gone() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("canonical-sodium", "version-1", "sodium.jar"), &target())
            .expect("cache record should insert");
        repository
            .upsert_project_alias("sodium", "canonical-sodium")
            .expect("project alias should insert");

        // No jar is written: the record lookup must stay silent, the hash lookup
        // must not. An emptied cache directory would otherwise send the whole
        // modlist onto the per-request-per-mod path.
        assert!(repository
            .find_compatible_by_project_or_alias("sodium", &target())
            .expect("record lookup should succeed")
            .is_none());
        assert_eq!(
            repository
                .find_cached_file_hash_by_project("canonical-sodium", &target())
                .expect("hash lookup should succeed")
                .as_deref(),
            Some("version-1-sha1")
        );
        assert_eq!(
            repository
                .find_cached_file_hash_by_project_or_alias("sodium", &target())
                .expect("alias hash lookup should succeed")
                .as_deref(),
            Some("version-1-sha1")
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn hash_lookup_skips_a_row_without_a_usable_file_hash() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("sodium", "version-1", "sodium.jar"), &target())
            .expect("cache record should insert");
        connection
            .execute(
                "UPDATE mod_cache SET file_hash = NULL WHERE modrinth_version_id = ?1",
                ["version-1"],
            )
            .expect("file_hash should be nulled");

        assert!(repository
            .find_cached_file_hash_by_project("sodium", &target())
            .expect("hash lookup should succeed")
            .is_none());

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn hash_lookup_lowercases_the_stored_hash() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("sodium", "version-1", "sodium.jar"), &target())
            .expect("cache record should insert");
        connection
            .execute(
                "UPDATE mod_cache SET file_hash = ?1 WHERE modrinth_version_id = ?2",
                ["  ABCDEF0123456789ABCDEF0123456789ABCDEF01  ", "version-1"],
            )
            .expect("file_hash should be replaced with an uppercase value");

        assert_eq!(
            repository
                .find_cached_file_hash_by_project("sodium", &target())
                .expect("hash lookup should succeed")
                .as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn version_id_lookup_answers_for_an_intact_row_whose_jar_is_gone() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(
                &version("canonical-sodium", "version-1", "sodium.jar"),
                &target(),
            )
            .expect("cache record should insert");
        repository
            .upsert_project_alias("sodium", "canonical-sodium")
            .expect("project alias should insert");

        // No jar on disk. The pre-check has to say which version is registered,
        // and a missing file does not change that answer.
        assert!(repository
            .find_compatible_by_project_or_alias("sodium", &target())
            .expect("record lookup should succeed")
            .is_none());
        assert_eq!(
            repository
                .find_cached_version_id_by_project("canonical-sodium", &target())
                .expect("version id lookup should succeed")
                .as_deref(),
            Some("version-1")
        );
        assert_eq!(
            repository
                .find_cached_version_id_by_project_or_alias("sodium", &target())
                .expect("alias version id lookup should succeed")
                .as_deref(),
            Some("version-1")
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn version_id_lookup_ignores_a_local_row() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("sodium", "local-sodium-1", "sodium.jar"), &target())
            .expect("cache record should insert");
        connection
            .execute(
                "UPDATE mod_cache SET is_local = 1 WHERE modrinth_version_id = ?1",
                ["local-sodium-1"],
            )
            .expect("row should become local");

        // A locally copied jar carries a synthetic version id, and
        // `GET /versions?ids=` rejects the whole request on one non-base62 id.
        assert!(repository
            .find_cached_version_id_by_project("sodium", &target())
            .expect("version id lookup should succeed")
            .is_none());

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn version_id_lookup_prefers_the_same_row_as_the_hash_lookup() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("sodium", "hashed-row", "sodium-a.jar"), &target())
            .expect("hashed row should insert");
        // A newer row for the same project and target — the primary key is
        // (version id, mc version, loader), so this is legal — whose hash is
        // NULL. The hash lookup skips it; the version id lookup must skip it
        // too, or the reported "current" version would not be the one the
        // candidate hash came from.
        repository
            .upsert_modrinth_version(&version("sodium", "unhashed-row", "sodium-b.jar"), &target())
            .expect("second row should insert");
        connection
            .execute(
                "UPDATE mod_cache SET file_hash = NULL WHERE modrinth_version_id = ?1",
                ["unhashed-row"],
            )
            .expect("file_hash should be nulled");

        assert_eq!(
            repository
                .find_cached_file_hash_by_project("sodium", &target())
                .expect("hash lookup should succeed")
                .as_deref(),
            Some("hashed-row-sha1")
        );
        assert_eq!(
            repository
                .find_cached_version_id_by_project("sodium", &target())
                .expect("version id lookup should succeed")
                .as_deref(),
            Some("hashed-row")
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// Writes the jar a record points at, so the row counts as usable.
    fn write_jar_for(mods_cache_dir: &Path, record: &ModCacheRecord) {
        let artifact_path = cached_artifact_path_for_record(mods_cache_dir, record);
        fs::create_dir_all(
            artifact_path
                .parent()
                .expect("artifact parent directory should exist"),
        )
        .expect("artifact parent directory should be created");
        fs::write(&artifact_path, b"jar").expect("jar should be written");
    }

    #[test]
    fn probing_a_project_tells_a_missing_row_apart_from_a_missing_jar() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let registered = repository
            .upsert_modrinth_version(&version("sodium", "version-1", "sodium.jar"), &target())
            .expect("cache record should insert");

        // The measured difference: the disk-checking lookup answers `None` for
        // both mods, so a launch built on it cannot tell a mod it never had
        // from a mod whose jar was deleted, and drops both.
        assert!(repository
            .find_compatible_by_project_or_alias("sodium", &target())
            .expect("record lookup should succeed")
            .is_none());
        assert!(repository
            .find_compatible_by_project_or_alias("polytone", &target())
            .expect("record lookup should succeed")
            .is_none());

        assert_eq!(
            repository
                .probe_project("sodium", &target())
                .expect("probe should succeed"),
            CacheProbe::JarMissing(registered.clone())
        );
        assert_eq!(
            repository
                .probe_project("polytone", &target())
                .expect("probe should succeed"),
            CacheProbe::NotCached
        );

        write_jar_for(&mods_cache_dir, &registered);
        assert_eq!(
            repository
                .probe_project("sodium", &target())
                .expect("probe should succeed"),
            CacheProbe::Ready(registered)
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn probing_a_version_id_answers_for_that_version_only() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        let older = repository
            .upsert_modrinth_version(&version("sodium", "version-1", "sodium-1.jar"), &target())
            .expect("older record should insert");
        let newer = repository
            .upsert_modrinth_version(&version("sodium", "version-2", "sodium-2.jar"), &target())
            .expect("newer record should insert");
        write_jar_for(&mods_cache_dir, &older);

        assert_eq!(
            repository
                .probe_version("version-1", &target())
                .expect("probe should succeed"),
            CacheProbe::Ready(older)
        );
        // The row for the version that was asked for, not the one whose jar
        // happens to be there.
        assert_eq!(
            repository
                .probe_version("version-2", &target())
                .expect("probe should succeed"),
            CacheProbe::JarMissing(newer)
        );
        assert_eq!(
            repository
                .probe_version("version-never-seen", &target())
                .expect("probe should succeed"),
            CacheProbe::NotCached
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn a_lost_jar_with_nothing_to_fetch_it_from_is_not_restorable() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let mods_cache_dir = root_dir.join("cache").join("mods");

        fs::create_dir_all(&mods_cache_dir).expect("mods cache directory should be created");
        initialize_database(&database_path).expect("database should initialize");

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = SqliteModCacheRepository::new(&connection, &mods_cache_dir);
        repository
            .upsert_modrinth_version(&version("my-jar", "local-1", "my-jar.jar"), &target())
            .expect("local record should insert");
        repository
            .upsert_modrinth_version(&version("urlless", "version-1", "urlless.jar"), &target())
            .expect("url-less record should insert");
        connection
            .execute(
                "UPDATE mod_cache SET is_local = 1 WHERE modrinth_version_id = ?1",
                ["local-1"],
            )
            .expect("row should become local");
        connection
            .execute(
                "UPDATE mod_cache SET download_url = NULL WHERE modrinth_version_id = ?1",
                ["version-1"],
            )
            .expect("download url should be nulled");

        // A copied jar has no Modrinth url, and neither has a row saved without
        // one: both are lost jars, and neither can be fetched again from here.
        for project in ["my-jar", "urlless"] {
            let probe = repository
                .probe_project(project, &target())
                .expect("probe should succeed");
            assert!(
                matches!(probe, CacheProbe::JarMissingUnrecoverable(_)),
                "{project} should be unrestorable, got {probe:?}"
            );
        }

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn the_restore_download_carries_the_registered_version() {
        let record = ModCacheRecord {
            modrinth_project_id: "sodium".into(),
            modrinth_version_id: "version-1".into(),
            jar_filename: "sodium-0.5.8.jar".into(),
            mc_version: "1.21.1".into(),
            mod_loader: "fabric".into(),
            file_hash: Some("version-1-sha1".into()),
            download_url: Some(" https://cdn.modrinth.com/data/sodium/sodium-0.5.8.jar ".into()),
            is_local: false,
        };

        let pending = pending_download_from_record(&record).expect("download should build");

        assert_eq!(pending.modrinth_version_id, "version-1");
        assert_eq!(pending.jar_filename, "sodium-0.5.8.jar");
        assert_eq!(
            pending.download_url,
            "https://cdn.modrinth.com/data/sodium/sodium-0.5.8.jar"
        );
        assert_eq!(pending.file_hash.as_deref(), Some("version-1-sha1"));

        let urlless = ModCacheRecord {
            download_url: None,
            ..record
        };
        assert!(pending_download_from_record(&urlless).is_err());
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
