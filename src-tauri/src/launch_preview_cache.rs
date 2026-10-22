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
    cache_record_from_version, cached_artifact_path_for_record, pending_download_from_record,
    pending_download_from_version, CacheProbe, InstanceJars, ModAcquisitionPlan, ModCacheLookup,
    ModCacheRecord, SqliteModCacheRepository,
};
use crate::modrinth::ModrinthVersion;
use crate::path_safety::validate_path_component;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::ResolutionTarget;
use crate::rules::ModSource;

use super::{
    build_instance_root, emit_log, emit_progress, jar_metadata_allows_target, DownloadArtifact,
    SelectedMod,
};

/// The cached record of a project on this target, plus whether a mod with no
/// usable artifact was never cached or is registered and lost its jar. The
/// version `instance` holds comes first (D147); an empty one leaves the
/// project's answer, which is what the availability passes want.
pub(super) fn probe_cached_mod_for_target(
    launcher_paths: &LauncherPaths,
    project_id: &str,
    target: &ResolutionTarget,
    instance: &InstanceJars,
) -> Result<CacheProbe> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    repository.probe_project_in_instance(project_id, target, instance)
}

/// The jars the instance of this mod list on this target holds now, read
/// before the launch rebuilds its `mods/` (D147).
pub(super) fn read_instance_jars(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    target: &ResolutionTarget,
) -> Result<InstanceJars> {
    InstanceJars::read(&build_instance_root(launcher_paths, modlist_name, target)?.join("mods"))
}

/// Stamp the versions this launch linked as the last used on the target
/// (audit 101, risoluzione-8).
pub(super) fn record_versions_used(
    launcher_paths: &LauncherPaths,
    version_ids: &[String],
    target: &ResolutionTarget,
) -> Result<()> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir())
        .mark_versions_used(version_ids, target)
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
/// One connection for the whole pass, unlike `probe_cached_mod_for_target`
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

/// The `modrinth_version_id` this instance holds per selected Modrinth mod —
/// the "current" side of an update row. Per instance, not per target (D147,
/// audit 101 precheck-2): two mod lists on one target each keep their own
/// version, and an update accepted in one never reaches the other unasked.
/// Absent means the instance has no jar of that mod, which by D17 is a first
/// install and not an update.
pub(super) fn load_cached_version_ids_for_selected(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    target: &ResolutionTarget,
    instance: &InstanceJars,
) -> Result<HashMap<String, String>> {
    load_cached_column_for_selected(launcher_paths, selected_mods, |repository, mod_id| {
        Ok(repository
            .find_registered_in_instance(mod_id, target, instance)?
            .map(|record| record.modrinth_version_id))
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
                if let Some(sixteenths) =
                    (16u64 * downloaded_bytes.min(total_bytes)).checked_div(total_bytes)
                {
                    let progress = 42u8 + sixteenths as u8;
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

/// Where the instance link of a local mod points, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LocalJarSource {
    Link(CachedModJar),
    /// No jar in `local-jars/` under this name.
    Missing(PathBuf),
    /// A jar that does not open: the launch skips it instead of stopping on
    /// "failed to read JAR metadata" (audit 101, risoluzione-9).
    Unreadable(PathBuf, String),
    /// The jar's own metadata excludes this target.
    Incompatible,
}

/// The jar of a local mod is linked straight from `mod-lists/<name>/local-jars/`
/// (audit 101, dati-1).
///
/// It used to be copied to `cache/mods/<loader>/local/<file>` only when that
/// copy was missing: the path named neither the mod list nor the bytes, so a
/// jar replaced in `local-jars/`, or a jar with the same name in another mod
/// list, never reached the game — the first copy stayed forever. Linking the
/// source leaves nothing that can go stale; the instance's `mods/` is rebuilt
/// at every launch, so it always points at the bytes the mod list holds now.
pub(super) fn local_jar_source(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    mod_id: &str,
    target: &ResolutionTarget,
) -> Result<LocalJarSource> {
    let file_name = format!("{mod_id}.jar");
    let source = local_mod_jar_path(launcher_paths, modlist_name, &file_name)?;
    if !source.is_file() {
        return Ok(LocalJarSource::Missing(source));
    }
    match jar_metadata_allows_target(&source, target) {
        Ok(true) => Ok(LocalJarSource::Link(CachedModJar {
            jar_filename: file_name,
            cache_path: source,
        })),
        Ok(false) => Ok(LocalJarSource::Incompatible),
        Err(error) => Ok(LocalJarSource::Unreadable(source, format!("{error:#}"))),
    }
}

// Otto input distinti di un passo del lancio. Una struct di parametri
// cambierebbe i chiamanti del percorso di lancio, fuori da una modifica di
// solo stile (gate fmt/clippy, D120): il lint è sospeso solo qui.
#[allow(clippy::too_many_arguments)]
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

    // Local mods: the instance links the jar in `local-jars/` itself.
    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Local) {
            continue;
        }

        let file_name = format!("{}.jar", selected.mod_id);
        if !seen.insert(file_name.clone()) {
            continue;
        }
        match local_jar_source(launcher_paths, modlist_name, &selected.mod_id, target)? {
            LocalJarSource::Link(jar) => jars.push(jar),
            LocalJarSource::Missing(path) => emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Launch] skipping local mod '{}': its jar is not in the mod list ({})",
                    selected.mod_id,
                    path.display()
                ),
            )?,
            LocalJarSource::Unreadable(path, error) => emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Launch] skipping local mod '{}': its jar cannot be read ({}: {error})",
                    selected.mod_id,
                    path.display()
                ),
            )?,
            LocalJarSource::Incompatible => emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Launch] skipping local mod '{}': embedded metadata is incompatible with {} / {}",
                    selected.mod_id,
                    target.minecraft_version,
                    target.mod_loader.as_modrinth_loader()
                ),
            )?,
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

    /// D3 on the code a launch actually runs (audit 101, risoluzione-9): a
    /// registered jar truncated in the cache is not handed to the launch, the
    /// probe reports it missing, and the download stage restores the
    /// registered version over it from the row alone.
    #[tokio::test]
    async fn a_truncated_cached_mod_is_restored_by_the_launch_download_stage() {
        let root = scratch_root("truncated-mod");
        let paths = LauncherPaths::new(&root);
        crate::database::initialize_database(paths.database_path()).expect("database");
        let complete = crate::mod_cache::test_jar_bytes("the complete mod jar");
        let body: &'static [u8] = Box::leak(complete.clone().into_boxed_slice());
        let (url, requests) = fixed_server(body);
        let version = version(&url, body);
        let connection = Connection::open(paths.database_path()).expect("database opens");
        let repository = SqliteModCacheRepository::new(&connection, paths.mods_cache_dir());
        let record = repository
            .upsert_modrinth_version(&version, &target())
            .expect("the version is registered");
        let destination = cached_artifact_path_for_record(paths.mods_cache_dir(), &record);
        fs::create_dir_all(destination.parent().expect("cache parent")).expect("cache parent");
        fs::write(&destination, &complete[..complete.len() / 2]).expect("truncated fixture");

        let probe = repository
            .probe_version(&version.id, &target())
            .expect("the probe answers");
        let CacheProbe::JarMissing(registered) = probe else {
            panic!("a truncated jar must not count as cached, got {probe:?}");
        };
        let pending = pending_download_from_record(&registered).expect("restorable from the row");
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
        download_artifact_streaming(
            &reqwest::Client::new(),
            &DownloadArtifact {
                filename: pending.jar_filename.clone(),
                url: pending.download_url.clone(),
                destination_path: crate::mod_cache::cached_artifact_path_for_pending_download(
                    paths.mods_cache_dir(),
                    &pending,
                ),
                file_hash: pending.file_hash.clone(),
                file_size: pending.file_size,
            },
            progress_tx,
        )
        .await
        .expect("the registered version is restored");

        assert_eq!(
            fs::read(&destination).expect("launch-visible jar"),
            complete
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(!partial_path(&destination).exists());
        assert!(matches!(
            repository.probe_version(&version.id, &target()),
            Ok(CacheProbe::Ready(_))
        ));
        drop(connection);
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    fn local_jar(paths: &LauncherPaths, modlist: &str, mod_id: &str, bytes: &[u8]) -> PathBuf {
        let path = local_mod_jar_path(paths, modlist, &format!("{mod_id}.jar")).expect("path");
        fs::create_dir_all(path.parent().expect("local-jars")).expect("local-jars");
        fs::write(&path, bytes).expect("local jar");
        path
    }

    /// Audit 101, dati-1: the link follows the source, so a jar replaced in
    /// `local-jars/` and a same-named jar in another mod list each reach the
    /// game with their own bytes.
    #[test]
    fn a_local_jar_is_linked_from_its_own_mod_list_with_its_current_bytes() {
        let root = scratch_root("local-jars");
        let paths = LauncherPaths::new(&root);
        let first = local_jar(
            &paths,
            "Pack A",
            "mymod",
            &crate::mod_cache::test_jar_bytes("v1"),
        );
        let other = local_jar(
            &paths,
            "Pack B",
            "mymod",
            &crate::mod_cache::test_jar_bytes("b"),
        );

        let LocalJarSource::Link(jar) =
            local_jar_source(&paths, "Pack A", "mymod", &target()).expect("source")
        else {
            panic!("the jar should be linked");
        };
        assert_eq!(jar.cache_path, first);
        assert_eq!(jar.jar_filename, "mymod.jar");

        fs::write(&first, crate::mod_cache::test_jar_bytes("v2")).expect("updated jar");
        let LocalJarSource::Link(jar) =
            local_jar_source(&paths, "Pack A", "mymod", &target()).expect("source")
        else {
            panic!("the jar should be linked");
        };
        assert_eq!(
            fs::read(&jar.cache_path).expect("linked bytes"),
            crate::mod_cache::test_jar_bytes("v2")
        );

        let LocalJarSource::Link(jar) =
            local_jar_source(&paths, "Pack B", "mymod", &target()).expect("source")
        else {
            panic!("the jar should be linked");
        };
        assert_eq!(jar.cache_path, other);
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[test]
    fn a_missing_or_unreadable_local_jar_is_skipped_not_fatal() {
        let root = scratch_root("local-jars-bad");
        let paths = LauncherPaths::new(&root);
        local_jar(&paths, "Pack A", "broken", b"not a zip");

        assert!(matches!(
            local_jar_source(&paths, "Pack A", "broken", &target()).expect("source"),
            LocalJarSource::Unreadable(..)
        ));
        assert!(matches!(
            local_jar_source(&paths, "Pack A", "absent", &target()).expect("source"),
            LocalJarSource::Missing(_)
        ));
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
