#![allow(dead_code)]

// Cache and artifact helpers for the launch pipeline.
//
// Cache-only mode depends on this module producing the same final JAR set as
// an online launch when artifacts are already present locally. Be careful with
// dependency persistence: online launches refresh dependency rows for resolved
// parents so cache-only launches do not reuse stale dependencies from another
// Minecraft version or loader.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use crate::dependencies::{DependencyLink, DependencyRequest, DependencySelector};
use crate::instance_mods::CachedModJar;
use crate::launcher_paths::LauncherPaths;
use crate::mod_cache::{
    build_mod_acquisition_plan, cache_record_from_version, cached_artifact_path_for_record,
    cached_local_artifact_path, pending_download_from_version, ModAcquisitionPlan, ModCacheLookup,
    ModCacheRecord, SqliteModCacheRepository,
};
use crate::modrinth::ModrinthVersion;
use crate::process_streaming::ProcessLogStream;
use crate::path_safety::validate_path_component;
use crate::resolver::ResolutionTarget;
use crate::rules::ModSource;

use super::{emit_log, emit_progress, jar_metadata_allows_target, DownloadArtifact, SelectedMod};

pub(super) async fn ensure_remote_version_cached(
    http_client: &reqwest::Client,
    launcher_paths: &LauncherPaths,
    version: &ModrinthVersion,
    target: &ResolutionTarget,
) -> Result<PathBuf> {
    let record = cache_record_from_version(version, target)?;
    let destination_path =
        cached_artifact_path_for_record(launcher_paths.mods_cache_dir(), &record);
    if destination_path.exists() {
        return Ok(destination_path);
    }

    let file = version.primary_file().with_context(|| {
        format!(
            "version '{}' for project '{}' does not expose a primary file",
            version.id, version.project_id
        )
    })?;
    match file.hashes.get("sha1").map(String::as_str) {
        Some(hash) => {
            crate::minecraft_downloader::download_file_verified(
                http_client,
                &file.url,
                &destination_path,
                hash,
            )
            .await?
        }
        None => download_file(http_client, &file.url, &destination_path).await?,
    }

    Ok(destination_path)
}

pub(super) fn load_cached_mod_record_for_target(
    launcher_paths: &LauncherPaths,
    project_id: &str,
    target: &ResolutionTarget,
) -> Result<Option<ModCacheRecord>> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    repository.find_compatible_by_project_or_alias(project_id, target)
}

pub(super) fn load_cached_mod_record_by_version(
    launcher_paths: &LauncherPaths,
    version_id: &str,
    target: &ResolutionTarget,
) -> Result<Option<ModCacheRecord>> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    repository.find_by_version_id(version_id, target)
}

pub(super) fn load_cached_dependency_requests(
    launcher_paths: &LauncherPaths,
    parent_mod_ids: &[String],
) -> Result<Vec<DependencyRequest>> {
    if parent_mod_ids.is_empty() {
        return Ok(Vec::new());
    }

    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;

    let mut requests = Vec::new();
    let mut statement = connection.prepare(
        r#"
        SELECT dependency_id, specific_version
        FROM dependencies
        WHERE mod_parent_id = ?1
          AND dep_type = 'required'
        ORDER BY dependency_id ASC
        "#,
    )?;

    for parent_mod_id in parent_mod_ids {
        let rows = statement.query_map([parent_mod_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;

        for row in rows {
            let (dependency_id, specific_version) = row?;
            let selector = match specific_version {
                Some(version_id) if !version_id.trim().is_empty() => {
                    DependencySelector::VersionId { version_id }
                }
                _ => DependencySelector::ProjectId {
                    project_id: dependency_id.clone(),
                },
            };

            requests.push(DependencyRequest {
                parent_mod_id: parent_mod_id.clone(),
                selector,
            });
        }
    }

    Ok(requests)
}

pub(super) fn build_remote_acquisition_plan_from_artifacts(
    launcher_paths: &LauncherPaths,
    live_versions: &[ModrinthVersion],
    cached_records: &[ModCacheRecord],
    target: &ResolutionTarget,
) -> Result<ModAcquisitionPlan> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());

    let mut seen_version_ids = HashSet::new();
    let mut cached = Vec::new();
    let mut to_download = Vec::new();

    for record in cached_records {
        if seen_version_ids.insert(record.modrinth_version_id.clone()) {
            cached.push(record.clone());
        }
    }

    for version in live_versions {
        if !seen_version_ids.insert(version.id.clone()) {
            continue;
        }

        match repository.find_by_version_id(&version.id, target)? {
            Some(record) => cached.push(record),
            None => to_download.push(pending_download_from_version(version, target)?),
        }
    }

    Ok(ModAcquisitionPlan {
        cached,
        to_download,
    })
}

pub(super) fn build_remote_acquisition_plan(
    launcher_paths: &LauncherPaths,
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
) -> Result<crate::mod_cache::ModAcquisitionPlan> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());

    build_mod_acquisition_plan(versions, target, &repository)
}

pub(super) async fn download_pending_artifacts(
    app_handle: &tauri::AppHandle,
    http_client: &reqwest::Client,
    default_directory: &Path,
    artifacts: &[DownloadArtifact],
) -> Result<()> {
    let total = artifacts.len();
    if total == 0 {
        return Ok(());
    }

    let total_bytes: u64 = artifacts.iter().map(|artifact| artifact.file_size).sum();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(10));
    let mut tasks: tokio::task::JoinSet<Result<DownloadArtifact>> = tokio::task::JoinSet::new();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

    for artifact in artifacts {
        let artifact = artifact.clone();
        let http_client = http_client.clone();
        let permit_source = semaphore.clone();
        let progress_tx = progress_tx.clone();
        tasks.spawn(async move {
            // Permit is held through the await so concurrency stays bounded.
            let _permit = permit_source
                .acquire_owned()
                .await
                .map_err(|error| anyhow::anyhow!("failed to acquire download permit: {error}"))?;
            download_artifact_streaming(&http_client, &artifact, progress_tx)
                .await
                .with_context(|| {
                    format!(
                        "failed to download '{}' to {}",
                        artifact.url,
                        artifact.destination_path.display()
                    )
                })?;
            Ok(artifact)
        });
    }
    drop(progress_tx);

    let mut completed: usize = 0;
    let mut downloaded_bytes: u64 = 0;
    let mut last_progress = 41u8;
    while completed < total {
        tokio::select! {
            Some(delta) = progress_rx.recv() => {
                downloaded_bytes = downloaded_bytes.saturating_add(delta);
                if total_bytes > 0 {
                    let progress = 42u8
                        + ((16u64 * downloaded_bytes.min(total_bytes)) / total_bytes) as u8;
                    if progress > last_progress {
                        last_progress = progress;
                        emit_progress(
                            app_handle,
                            "resolving",
                            progress,
                            "Downloading Mods",
                            &format!(
                                "Downloaded {} of {} ({completed} of {total} mods).",
                                format_bytes(downloaded_bytes.min(total_bytes)),
                                format_bytes(total_bytes),
                            ),
                        )?;
                    }
                }
            }
            Some(join_result) = tasks.join_next() => {
                let artifact =
                    join_result.map_err(|error| anyhow::anyhow!("download task panicked: {error}"))??;
                completed += 1;
                while let Ok(delta) = progress_rx.try_recv() {
                    downloaded_bytes = downloaded_bytes.saturating_add(delta);
                }

                if total_bytes == 0 {
                    let progress = 42u8 + ((16usize * completed) / total) as u8;
                    emit_progress(
                        app_handle,
                        "resolving",
                        progress,
                        "Downloading Mods",
                        &format!("Downloaded {completed} of {total} mods."),
                    )?;
                } else {
                    emit_progress(
                        app_handle,
                        "resolving",
                        last_progress.max(42),
                        "Downloading Mods",
                        &format!(
                            "Downloaded {} of {} ({completed} of {total} mods).",
                            format_bytes(downloaded_bytes.min(total_bytes)),
                            format_bytes(total_bytes),
                        ),
                    )?;
                }

                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Download] Saved {} to {}",
                        artifact.filename,
                        artifact
                            .destination_path
                            .strip_prefix(default_directory)
                            .unwrap_or(&artifact.destination_path)
                            .display()
                    ),
                )?;
            }
            else => break,
        }
    }

    Ok(())
}

async fn download_artifact_streaming(
    http_client: &reqwest::Client,
    artifact: &DownloadArtifact,
    progress_tx: tokio::sync::mpsc::UnboundedSender<u64>,
) -> Result<()> {
    if let Some(parent) = artifact.destination_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let response = http_client
        .get(&artifact.url)
        .send()
        .await
        .with_context(|| format!("failed to request {}", artifact.url))?
        .error_for_status()
        .with_context(|| format!("download request failed for {}", artifact.url))?;

    let temp_path = artifact.destination_path.with_file_name(format!(
        "{}.part",
        artifact
            .destination_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download")
    ));
    let mut file = tokio::fs::File::create(&temp_path)
        .await
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    let mut hasher = artifact.file_hash.as_ref().map(|_| Sha1::new());

    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read downloaded bytes from {}", artifact.url))?
    {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        let _ = progress_tx.send(chunk.len() as u64);
    }
    file.flush()
        .await
        .with_context(|| format!("failed to flush {}", temp_path.display()))?;
    drop(file);

    if let (Some(expected), Some(hasher)) = (&artifact.file_hash, hasher) {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&temp_path).await;
            anyhow::bail!(
                "SHA1 mismatch for {}: expected {}, got {}",
                artifact.filename,
                expected,
                actual
            );
        }
    }

    tokio::fs::rename(&temp_path, &artifact.destination_path)
        .await
        .with_context(|| {
            format!(
                "failed to move {} to {}",
                temp_path.display(),
                artifact.destination_path.display()
            )
        })?;

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

pub(super) async fn download_file(
    http_client: &reqwest::Client,
    url: &str,
    destination_path: &Path,
) -> Result<()> {
    let response = http_client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to request {url}"))?
        .error_for_status()
        .with_context(|| format!("download request failed for {url}"))?;
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read downloaded bytes from {url}"))?;

    if let Some(parent) = destination_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    tokio::fs::write(destination_path, bytes)
        .await
        .with_context(|| format!("failed to write {}", destination_path.display()))?;

    Ok(())
}

pub(super) fn persist_remote_versions_and_dependencies(
    launcher_paths: &LauncherPaths,
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
    dependency_links: &[DependencyLink],
    project_aliases: &[(String, String)],
) -> Result<()> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());

    for version in versions {
        repository.upsert_modrinth_version(version, target)?;
    }

    for (alias, canonical_project_id) in project_aliases {
        repository.upsert_project_alias(alias, canonical_project_id)?;
    }

    let refreshed_parent_ids = versions
        .iter()
        .map(|version| version.project_id.clone())
        .collect::<HashSet<_>>();
    persist_dependency_links(&connection, dependency_links, &refreshed_parent_ids)
}

pub(super) fn persist_dependency_links(
    connection: &Connection,
    links: &[DependencyLink],
    refreshed_parent_ids: &HashSet<String>,
) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;

    for parent_id in refreshed_parent_ids {
        transaction.execute(
            "DELETE FROM dependencies WHERE mod_parent_id = ?1",
            [parent_id.as_str()],
        )?;
    }

    for link in links {
        transaction.execute(
            r#"
            INSERT INTO dependencies (
                mod_parent_id,
                dependency_id,
                dep_type,
                specific_version,
                jar_filename
            ) VALUES (?1, ?2, 'required', ?3, ?4)
            ON CONFLICT(mod_parent_id, dependency_id) DO UPDATE SET
                dep_type = excluded.dep_type,
                specific_version = excluded.specific_version,
                jar_filename = excluded.jar_filename
            "#,
            params![
                &link.parent_mod_id,
                &link.dependency_id,
                &link.specific_version,
                &link.jar_filename,
            ],
        )?;
    }

    transaction.commit()?;
    Ok(())
}

pub(super) fn local_mod_jar_path(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    file_name: &str,
) -> Result<PathBuf> {
    validate_path_component(modlist_name)?;
    validate_path_component(file_name)?;
    Ok(launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join("local-jars")
        .join(file_name))
}

pub(super) fn build_cached_mod_jars(
    app_handle: &tauri::AppHandle,
    selected_mods: &[SelectedMod],
    versions: &[ModrinthVersion],
    cached_records: &[ModCacheRecord],
    target: &ResolutionTarget,
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
) -> Result<Vec<CachedModJar>> {
    let mut jars = Vec::new();
    let mut seen = HashSet::new();
    let mod_loader = target.mod_loader.as_modrinth_loader();

    // Local mods: JAR lives at local-jars/{mod_id}.jar; copy to cache/mods/.
    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Local) {
            continue;
        }

        let file_name = format!("{}.jar", selected.mod_id);
        if seen.insert(file_name.clone()) {
            let source = local_mod_jar_path(launcher_paths, modlist_name, &file_name)?;
            let dest =
                cached_local_artifact_path(launcher_paths.mods_cache_dir(), mod_loader, &file_name);
            if source.exists() && !jar_metadata_allows_target(&source, target)? {
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Launch] skipping local mod '{}': embedded metadata is incompatible with {} / {}",
                        selected.mod_id,
                        target.minecraft_version,
                        target.mod_loader.as_modrinth_loader()
                    ),
                )?;
                continue;
            }
            if source.exists() && !dest.exists() {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::copy(&source, &dest).with_context(|| {
                    format!("failed to copy local JAR '{}' to mod cache", file_name)
                })?;
            }
            jars.push(CachedModJar {
                jar_filename: file_name,
                cache_path: dest,
            });
        }
    }

    for record in cached_records {
        if seen.insert(record.jar_filename.clone()) {
            jars.push(CachedModJar {
                jar_filename: record.jar_filename.clone(),
                cache_path: cached_artifact_path_for_record(
                    launcher_paths.mods_cache_dir(),
                    record,
                ),
            });
        }
    }

    for version in versions {
        let record = cache_record_from_version(version, target)?;
        let jar_filename = record.jar_filename.clone();
        if seen.insert(jar_filename.clone()) {
            jars.push(CachedModJar {
                jar_filename,
                cache_path: cached_artifact_path_for_record(
                    launcher_paths.mods_cache_dir(),
                    &record,
                ),
            });
        }
    }

    Ok(jars)
}
