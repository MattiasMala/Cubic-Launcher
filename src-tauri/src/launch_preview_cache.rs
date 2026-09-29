#![allow(dead_code)]

// Cache and artifact helpers for the launch pipeline.
//
// A launch that resolves versions online and one that launches from a version
// map must end up with the same final JAR set when the artifacts are already
// here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use crate::instance_mods::CachedModJar;
use crate::launcher_paths::LauncherPaths;
use crate::mod_cache::{
    cache_record_from_version, cached_artifact_path_for_record, cached_local_artifact_path,
    pending_download_from_record, pending_download_from_version, CacheProbe, ModAcquisitionPlan,
    ModCacheLookup, ModCacheRecord, SqliteModCacheRepository,
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
        None if !destination_path.exists() => {
            download_file(http_client, &file.url, &destination_path).await?
        }
        None => {}
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

/// `load_cached_mod_record_for_target` plus the answer it cannot give: whether
/// a mod with no usable artifact was never cached, or is registered and lost
/// its jar.
pub(super) fn probe_cached_mod_for_target(
    launcher_paths: &LauncherPaths,
    project_id: &str,
    target: &ResolutionTarget,
) -> Result<CacheProbe> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    repository.probe_project(project_id, target)
}

/// Same, for the one version id a pre-check named.
pub(super) fn probe_cached_version_for_target(
    launcher_paths: &LauncherPaths,
    version_id: &str,
    target: &ResolutionTarget,
) -> Result<CacheProbe> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    repository.probe_version(version_id, target)
}

/// One cache column per selected Modrinth mod, on this target, keyed by the mod
/// id used in the rules.
///
/// One connection for the whole pass, unlike `load_cached_mod_record_for_target`
/// which opens one per mod. Mods with no row — or a row whose column is unusable
/// — are simply absent from the map.
fn load_cached_column_for_selected<Lookup>(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    lookup: Lookup,
) -> Result<HashMap<String, String>>
where
    Lookup: Fn(&SqliteModCacheRepository<'_>, &str) -> Result<Option<String>>,
{
    let mut values = HashMap::new();
    if selected_mods.is_empty() {
        return Ok(values);
    }

    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) || values.contains_key(&selected.mod_id)
        {
            continue;
        }
        if let Some(value) = lookup(&repository, &selected.mod_id)? {
            values.insert(selected.mod_id.clone(), value);
        }
    }

    Ok(values)
}

/// Cached sha1 per selected Modrinth mod. The hash comes from the row itself:
/// see `find_cached_file_hash_by_project` for why the disk-checking lookup is
/// the wrong tool here.
pub(super) fn load_cached_file_hashes_for_selected(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    target: &ResolutionTarget,
) -> Result<HashMap<String, String>> {
    load_cached_column_for_selected(launcher_paths, selected_mods, |repository, mod_id| {
        repository.find_cached_file_hash_by_project_or_alias(mod_id, target)
    })
}

/// Registered `modrinth_version_id` per selected Modrinth mod — the "current"
/// side of an update row. Absent means "never downloaded for this target",
/// which by D17 is not an update.
pub(super) fn load_cached_version_ids_for_selected(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    target: &ResolutionTarget,
) -> Result<HashMap<String, String>> {
    load_cached_column_for_selected(launcher_paths, selected_mods, |repository, mod_id| {
        repository.find_cached_version_id_by_project_or_alias(mod_id, target)
    })
}

/// The acquisition plan of every launch.
///
/// Three inputs, one per kind of artifact the selection pass produced: live
/// versions (a version the cache does not hold, whose metadata came from
/// Modrinth), records with their jar in place, and records whose jar is gone
/// and that the download stage has to restore **at the registered version**.
/// A launch that resolved everything online passes two empty lists.
pub(super) fn build_remote_acquisition_plan_from_artifacts(
    launcher_paths: &LauncherPaths,
    live_versions: &[ModrinthVersion],
    cached_records: &[ModCacheRecord],
    missing_jar_records: &[ModCacheRecord],
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

    for record in missing_jar_records {
        if seen_version_ids.insert(record.modrinth_version_id.clone()) {
            to_download.push(pending_download_from_record(record)?);
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

    crate::atomic_write::write_atomically(destination_path, bytes)
        .with_context(|| format!("failed to publish {}", destination_path.display()))?;

    Ok(())
}

pub(super) fn persist_remote_versions_and_aliases(
    launcher_paths: &LauncherPaths,
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
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
    restored_records: &[ModCacheRecord],
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

    // A restored record's jar lands at the same cache path as an intact one:
    // the download stage has already put it back by the time this runs, so the
    // two kinds link identically.
    for record in cached_records.iter().chain(restored_records) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ModLoader;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("download-tests")
            .join(format!("{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch root should be created");
        root
    }

    fn fixed_server(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("server address").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0; 2048];
                let _ = stream.read(&mut request);
                counter.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}/artifact.jar"), requests)
    }

    fn cut_short_server(prefix: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("server address").port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request should arrive");
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                prefix.len() * 2
            );
            stream.write_all(header.as_bytes()).expect("header");
            stream.write_all(prefix).expect("partial body");
            stream.flush().expect("flush");
        });
        format!("http://127.0.0.1:{port}/interrupted.jar")
    }

    fn version(url: &str, body: &[u8]) -> ModrinthVersion {
        let sha1 = format!("{:x}", Sha1::digest(body));
        serde_json::from_value(serde_json::json!({
            "id": "version-complete",
            "project_id": "project-complete",
            "version_number": "1.0.0",
            "name": "Complete",
            "game_versions": ["1.20.1"],
            "loaders": ["fabric"],
            "version_type": "release",
            "files": [{
                "hashes": { "sha1": sha1 },
                "url": url,
                "filename": "complete.jar",
                "primary": true,
                "size": body.len()
            }],
            "date_published": "2026-09-29T00:00:00Z"
        }))
        .expect("version fixture should deserialize")
    }

    fn target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.20.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    fn partial_path(path: &Path) -> PathBuf {
        let mut name = path.file_name().expect("target file name").to_os_string();
        name.push(".part");
        path.with_file_name(name)
    }

    #[tokio::test]
    async fn a_truncated_cached_mod_is_not_returned_to_the_launch() {
        let root = scratch_root("truncated-mod");
        let paths = LauncherPaths::new(&root);
        let complete = b"the complete mod jar";
        let (url, requests) = fixed_server(complete);
        let version = version(&url, complete);
        let destination = paths.remote_mod_artifact_path(
            target().mod_loader.as_modrinth_loader(),
            &version.id,
            "complete.jar",
        );
        fs::create_dir_all(destination.parent().expect("cache parent")).expect("cache parent");
        fs::write(&destination, b"the complete").expect("truncated cache fixture");

        let returned =
            ensure_remote_version_cached(&reqwest::Client::new(), &paths, &version, &target())
                .await
                .expect("the corrupt cache entry should be repaired");

        assert_eq!(returned, destination);
        assert_eq!(fs::read(&returned).expect("launch-visible jar"), complete);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "the truncated jar must not suppress its replacement"
        );
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn a_complete_download_lands_without_a_partial() {
        let root = scratch_root("complete");
        let destination = root.join("complete.jar");
        let (url, _) = fixed_server(b"whole file");

        download_file(&reqwest::Client::new(), &url, &destination)
            .await
            .expect("download should complete");

        assert_eq!(fs::read(&destination).expect("final file"), b"whole file");
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn a_partial_left_by_a_killed_download_does_not_block_the_next_one() {
        let root = scratch_root("stale-partial");
        let destination = root.join("complete.jar");
        fs::write(partial_path(&destination), b"half").expect("stale partial");
        let (url, _) = fixed_server(b"whole file");

        download_file(&reqwest::Client::new(), &url, &destination)
            .await
            .expect("retry should complete");

        assert_eq!(fs::read(&destination).expect("final file"), b"whole file");
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn a_download_cut_short_never_appears_under_the_final_name() {
        let root = scratch_root("cut-short-new");
        let destination = root.join("complete.jar");
        let url = cut_short_server(b"half");

        let result = download_file(&reqwest::Client::new(), &url, &destination).await;

        assert!(result.is_err(), "the short body must be reported");
        assert!(!destination.exists(), "the final name must stay absent");
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn a_download_cut_short_leaves_the_previous_file_untouched() {
        let root = scratch_root("cut-short-existing");
        let destination = root.join("complete.jar");
        fs::write(&destination, b"previous complete file").expect("previous file");
        let url = cut_short_server(b"half");

        let result = download_file(&reqwest::Client::new(), &url, &destination).await;

        assert!(result.is_err(), "the short body must be reported");
        assert_eq!(
            fs::read(&destination).expect("previous file remains"),
            b"previous complete file"
        );
        fs::remove_dir_all(root).expect("scratch cleanup");
    }
}
