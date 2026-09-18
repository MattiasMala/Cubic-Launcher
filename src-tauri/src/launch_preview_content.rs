use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::content_packs::{load_content_list, ContentEntry, ContentList};
use crate::launcher_paths::LauncherPaths;
use crate::local_content_packs::{link_local_pack, plan_local_pack_installs, PackLinkOutcome};
use crate::modrinth::ModrinthClient;
use crate::path_safety::validate_path_component;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::ResolutionTarget;
use crate::rules::VersionRuleKind;

use super::{download_file, emit_log};

/// Checks whether a content entry is active for the current MC version + loader.
fn is_content_entry_active(entry: &ContentEntry, mc_version: &str, loader: &str) -> bool {
    for rule in &entry.version_rules {
        let version_match =
            rule.mc_versions.is_empty() || rule.mc_versions.iter().any(|v| v == mc_version);
        let loader_match = rule.loader == "any" || rule.loader.eq_ignore_ascii_case(loader);
        match rule.kind {
            VersionRuleKind::Exclude => {
                if version_match && loader_match {
                    return false;
                }
            }
            VersionRuleKind::Only => {
                if !(version_match && loader_match) {
                    return false;
                }
            }
        }
    }
    true
}

pub(super) fn validate_content_filename(filename: &str) -> Result<()> {
    validate_path_component(filename)
}

/// Where the cache keeps one downloaded Modrinth content-pack file: one
/// directory per **version id**, with the file inside it under its own name
/// (D54) — the same shape the mod cache has had all along
/// (`mod_cache::cached_remote_artifact_path`).
///
/// Keyed by filename alone, as this was, three things went wrong at once and
/// all of them in silence, because the launch only logs `(cached)`. Resource
/// pack authors reuse a filename across versions, so an update was never
/// downloaded (`visual-effects-plus` ships every version as
/// `Visual Effects+.zip`); they also reuse it across game-version lines, so a
/// launch on another target linked the wrong bytes (`enhanced-boss-bars` 1.6
/// exists five times under `[1.6] Enhanced Boss Bars.zip`, byte-different per
/// line); and the cache is shared by every mod list, so two lists using two
/// projects with one filename overwrote each other.
///
/// The version id comes from the network, so it is validated like the filename
/// already was: it becomes a directory name.
pub(super) fn content_pack_cache_path(
    cache_dir: &Path,
    version_id: &str,
    filename: &str,
) -> Result<PathBuf> {
    validate_path_component(version_id)
        .with_context(|| format!("unsafe Modrinth version id '{version_id}'"))?;
    validate_content_filename(filename)?;
    Ok(cache_dir.join(version_id).join(filename))
}

/// Put the file of one Modrinth content-pack version in the cache if it is not
/// there yet, and say which of the two happened.
///
/// The pair is the whole point of the function: `was_cached` decides the
/// download *and* the `(cached)` label in the launch log, so the decision and
/// the path it was taken on come from one place instead of being restated at
/// every call site. That is also what makes the skip testable without a
/// `tauri::AppHandle`: the two callers below are inside the launch pipeline.
///
/// `before_download` runs only when the file has to be fetched, and before the
/// request, so the log line still precedes a download that can take a while.
pub(super) async fn ensure_content_pack_cached(
    http_client: &reqwest::Client,
    cache_dir: &Path,
    version_id: &str,
    filename: &str,
    url: &str,
    before_download: impl FnOnce() -> Result<()>,
) -> Result<(PathBuf, bool)> {
    let cached_path = content_pack_cache_path(cache_dir, version_id, filename)?;
    if cached_path.exists() {
        return Ok((cached_path, true));
    }

    before_download()?;
    download_file(http_client, url, &cached_path).await?;

    Ok((cached_path, false))
}

/// Link the `source: "local"` entries of one category into the instance, and
/// return the names that reached it.
///
/// Those names are the point of this function: they go into the set
/// [`crate::instance_content::sync_managed_content_dir`] compares the previous
/// manifest against. A pack that is linked but left out of that set is removed
/// on the *next* launch — the wipe C1 closed, reappearing in a form that takes
/// two launches to notice.
///
/// A missing file is an answer, not a failure: it is logged and does not freeze
/// the category's removals, because we know exactly which pack is gone. Nothing
/// here touches the network.
fn install_local_content_packs(
    app_handle: &tauri::AppHandle,
    modlist_dir: &Path,
    content_type: &str,
    instance_root: &Path,
    instance_subdir: &str,
    instance_dir: &Path,
    active_entries: &[&ContentEntry],
) -> Result<Vec<String>> {
    let mut installed = Vec::new();

    // What the last launch says it put here. It decides one thing only: whether
    // a real directory already under a pack's name may be replaced. Without it
    // the install would `remove_dir_all` a folder the user unpacked themselves.
    let previous = crate::instance_content::read_manifest_files(
        &crate::instance_content::manifest_path(instance_root, instance_subdir),
    );

    for pack in plan_local_pack_installs(modlist_dir, content_type, active_entries) {
        if !pack.present {
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Content] Local pack '{}' is missing from {}",
                    pack.file_name,
                    pack.source_path.display()
                ),
            )?;
            continue;
        }

        let target_path = instance_dir.join(&pack.file_name);
        let directory_is_ours = previous.iter().any(|name| name == &pack.file_name);
        let outcome = link_local_pack(&pack.source_path, &target_path, directory_is_ours)
            .with_context(|| {
                format!(
                    "failed to install local content pack '{}' into instance",
                    pack.file_name
                )
            })?;

        let how = match outcome {
            PackLinkOutcome::Linked => "",
            // Not silent: a copied folder is a second set of bytes on disk, and
            // whoever reads the log has to be able to tell why.
            PackLinkOutcome::Copied => " (copied: this platform refused a directory link)",
            PackLinkOutcome::SkippedForeignDirectory => {
                // Left out of `installed` on purpose: the name is not in the
                // manifest, so the sync will not remove it either, and the
                // user's folder stays exactly as it is.
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] Skipped local pack '{}': {} is a directory the launcher did not create",
                        pack.file_name,
                        target_path.display()
                    ),
                )?;
                continue;
            }
        };

        if !installed.iter().any(|name| name == &pack.file_name) {
            installed.push(pack.file_name.clone());
        }

        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!("[Content] {} -> {}{}", pack.file_name, instance_subdir, how),
        )?;
    }

    Ok(installed)
}

/// Resolve, download and install content packs into the instance.
pub(super) async fn resolve_and_install_content_packs(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    modrinth_client: &ModrinthClient,
    modlist_name: &str,
    target: &ResolutionTarget,
    instance_root: &Path,
) -> Result<()> {
    validate_path_component(modlist_name)?;
    let modlist_dir = launcher_paths.modlists_dir().join(modlist_name);
    let cache_dir = launcher_paths.content_packs_cache_dir();
    std::fs::create_dir_all(cache_dir).with_context(|| {
        format!(
            "failed to create content packs cache at {}",
            cache_dir.display()
        )
    })?;

    let mc_version = &target.minecraft_version;
    let loader_str = target.mod_loader.as_modrinth_loader();

    for (content_type, instance_subdir) in
        [("resourcepack", "resourcepacks"), ("shader", "shaderpacks")]
    {
        let list = load_content_list(&modlist_dir, content_type).unwrap_or_else(|_| ContentList {
            content_type: content_type.to_string(),
            entries: vec![],
            groups: vec![],
        });

        let active_entries: Vec<&ContentEntry> = list
            .entries
            .iter()
            .filter(|entry| is_content_entry_active(entry, mc_version, loader_str))
            .collect();

        let instance_dir = instance_root.join(instance_subdir);
        // What we link this launch; the previous launch's list comes from the
        // manifest, and only the difference is removed. Nothing else in the
        // directory is touched.
        let mut installed: Vec<String> = Vec::new();
        let mut lookups_complete = true;

        if !active_entries.is_empty() {
            std::fs::create_dir_all(&instance_dir)
                .with_context(|| format!("failed to create {}", instance_dir.display()))?;
        }

        installed.extend(install_local_content_packs(
            app_handle,
            &modlist_dir,
            content_type,
            instance_root,
            instance_subdir,
            &instance_dir,
            &active_entries,
        )?);

        for entry in &active_entries {
            if entry.source != "modrinth" {
                if entry.source != "local" {
                    // The old code skipped everything non-Modrinth without a
                    // word, so a typo in `source` looked like an empty list.
                    emit_log(
                        app_handle,
                        ProcessLogStream::Stdout,
                        format!(
                            "[Content] Skipping '{}': unknown source '{}'",
                            entry.id, entry.source
                        ),
                    )?;
                }
                continue;
            }

            match modrinth_client
                .fetch_content_pack_versions(&entry.id, mc_version)
                .await
            {
                Ok(versions) => {
                    let best = versions
                        .into_iter()
                        .max_by(|a, b| a.date_published.cmp(&b.date_published));
                    if let Some(version) = best {
                        if let Some(file) = version.primary_file() {
                            let (cached_path, was_cached) = ensure_content_pack_cached(
                                http_client,
                                cache_dir,
                                &version.id,
                                &file.filename,
                                &file.url,
                                || {
                                    emit_log(
                                        app_handle,
                                        ProcessLogStream::Stdout,
                                        format!(
                                            "[Content] Downloading {} ({})",
                                            entry.id, file.filename
                                        ),
                                    )
                                },
                            )
                            .await
                            .with_context(|| {
                                format!("failed to download content pack '{}'", entry.id)
                            })?;
                            let target_path = instance_dir.join(&file.filename);
                            crate::instance_mods::create_file_link(&cached_path, &target_path)
                                .with_context(|| {
                                    format!(
                                        "failed to link content pack '{}' into instance",
                                        entry.id
                                    )
                                })?;
                            if !installed.iter().any(|name| name == &file.filename) {
                                installed.push(file.filename.clone());
                            }
                            let cache_label = if was_cached { " (cached)" } else { "" };
                            emit_log(
                                app_handle,
                                ProcessLogStream::Stdout,
                                format!(
                                    "[Content] {} -> {}{}",
                                    entry.id, instance_subdir, cache_label
                                ),
                            )?;
                        }
                    } else {
                        emit_log(
                            app_handle,
                            ProcessLogStream::Stdout,
                            format!(
                                "[Content] No compatible version found for '{}' on {}",
                                entry.id, mc_version
                            ),
                        )?;
                    }
                }
                Err(error) => {
                    // Not an answer: we do not know which file this entry wants,
                    // so nothing may be removed on this pass.
                    lookups_complete = false;
                    emit_log(
                        app_handle,
                        ProcessLogStream::Stdout,
                        format!(
                            "[Content] Failed to fetch versions for '{}': {}",
                            entry.id, error
                        ),
                    )?;
                }
            }
        }

        let removed = crate::instance_content::sync_managed_content_dir(
            instance_root,
            instance_subdir,
            &instance_dir,
            &installed,
            lookups_complete,
        )?;
        for name in removed {
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!("[Content] Removed {} from {}", name, instance_subdir),
            )?;
        }
    }

    install_datapacks(
        app_handle,
        &modlist_dir,
        cache_dir,
        http_client,
        modrinth_client,
        mc_version,
        loader_str,
        instance_root,
    )
    .await
}

async fn install_datapacks(
    app_handle: &tauri::AppHandle,
    modlist_dir: &Path,
    cache_dir: &Path,
    http_client: &reqwest::Client,
    modrinth_client: &ModrinthClient,
    mc_version: &str,
    loader_str: &str,
    instance_root: &Path,
) -> Result<()> {
    // Data packs are world-specific, so put them in a top-level datapacks
    // folder supported by mods such as Open Loader.
    let list = load_content_list(modlist_dir, "datapack").unwrap_or_else(|_| ContentList {
        content_type: "datapack".to_string(),
        entries: vec![],
        groups: vec![],
    });
    let active_entries: Vec<&ContentEntry> = list
        .entries
        .iter()
        .filter(|entry| is_content_entry_active(entry, mc_version, loader_str))
        .collect();

    let instance_dir = instance_root.join("datapacks");
    let mut installed: Vec<String> = Vec::new();
    let mut lookups_complete = true;

    if !active_entries.is_empty() {
        std::fs::create_dir_all(&instance_dir)
            .with_context(|| format!("failed to create {}", instance_dir.display()))?;
    }

    installed.extend(install_local_content_packs(
        app_handle,
        modlist_dir,
        "datapack",
        instance_root,
        "datapacks",
        &instance_dir,
        &active_entries,
    )?);

    for entry in &active_entries {
        if entry.source != "modrinth" {
            if entry.source != "local" {
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] Skipping '{}': unknown source '{}'",
                        entry.id, entry.source
                    ),
                )?;
            }
            continue;
        }

        match modrinth_client
            .fetch_content_pack_versions(&entry.id, mc_version)
            .await
        {
            Ok(versions) => {
                let best = versions
                    .into_iter()
                    .max_by(|a, b| a.date_published.cmp(&b.date_published));
                if let Some(version) = best {
                    if let Some(file) = version.primary_file() {
                        let (cached_path, was_cached) = ensure_content_pack_cached(
                            http_client,
                            cache_dir,
                            &version.id,
                            &file.filename,
                            &file.url,
                            || {
                                emit_log(
                                    app_handle,
                                    ProcessLogStream::Stdout,
                                    format!(
                                        "[Content] Downloading {} ({})",
                                        entry.id, file.filename
                                    ),
                                )
                            },
                        )
                        .await
                        .with_context(|| format!("failed to download data pack '{}'", entry.id))?;
                        let target_path = instance_dir.join(&file.filename);
                        crate::instance_mods::create_file_link(&cached_path, &target_path)
                            .with_context(|| {
                                format!("failed to link data pack '{}' into instance", entry.id)
                            })?;
                        if !installed.iter().any(|name| name == &file.filename) {
                            installed.push(file.filename.clone());
                        }
                        let cache_label = if was_cached { " (cached)" } else { "" };
                        emit_log(
                            app_handle,
                            ProcessLogStream::Stdout,
                            format!("[Content] {} -> datapacks{}", entry.id, cache_label),
                        )?;
                    }
                }
            }
            Err(error) => {
                // Not an answer: we do not know which file this entry wants, so
                // nothing may be removed on this pass.
                lookups_complete = false;
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] Failed to fetch versions for '{}': {}",
                        entry.id, error
                    ),
                )?;
            }
        }
    }

    let removed = crate::instance_content::sync_managed_content_dir(
        instance_root,
        "datapacks",
        &instance_dir,
        &installed,
        lookups_complete,
    )?;
    for name in removed {
        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!("[Content] Removed {name} from datapacks"),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// One root per test: the async tests run on different threads, and two
    /// calls in the same nanosecond would otherwise share a directory that one
    /// of them removes at the end.
    fn unique_test_root(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-content-cache-test-{label}-{timestamp}"))
    }

    /// A server that answers one fixed body and counts what it was asked for,
    /// so a test can tell "downloaded" from "served from the cache" by
    /// observing the requests instead of the file.
    fn counting_server(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("test server has an address").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch);
                counter.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });

        (format!("http://127.0.0.1:{port}/pack.zip"), requests)
    }

    /// The difference between the two layouts, measured on the behaviour and
    /// not on the path: a file sitting in the cache **root** under the wanted
    /// name must not make a version count as downloaded. That file is what the
    /// filename-keyed layout left behind, and it is also what every launch
    /// before this change produced.
    #[tokio::test]
    async fn a_file_in_the_old_flat_layout_does_not_pass_for_a_version() {
        let cache_dir = unique_test_root("flat-leftover");
        let filename = "Visual Effects+.zip";
        let (url, requests) = counting_server(b"1.3.1");

        fs::create_dir_all(&cache_dir).expect("cache root should exist");
        fs::write(cache_dir.join(filename), b"1.3.0").expect("the flat leftover is on disk");
        let previous = content_pack_cache_path(&cache_dir, "rNnjlJrG", filename).expect("path");
        fs::create_dir_all(previous.parent().expect("parent")).expect("mkdir");
        fs::write(&previous, b"1.3.0").expect("the previous version is on disk too");

        let (path, was_cached) = ensure_content_pack_cached(
            &reqwest::Client::new(),
            &cache_dir,
            "MgC4Oa2v",
            filename,
            &url,
            || Ok(()),
        )
        .await
        .expect("the newer version should be fetched");

        assert!(!was_cached, "the newer version was never downloaded before");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "exactly one download must have been issued"
        );
        assert_eq!(
            fs::read(&path).expect("the fetched file is readable"),
            b"1.3.1",
            "the bytes on disk must be the newer version's"
        );
        assert_eq!(
            fs::read(&previous).expect("the previous version is readable"),
            b"1.3.0",
            "and the previous version must still be there, untouched"
        );

        let _ = fs::remove_dir_all(&cache_dir);
    }

    /// The other direction, so the fix cannot become "download every launch":
    /// the same version asked twice costs one request.
    #[tokio::test]
    async fn the_same_version_asked_twice_is_served_from_the_cache() {
        let cache_dir = unique_test_root("cache-hit");
        let (url, requests) = counting_server(b"1.3.1");
        let client = reqwest::Client::new();

        let first = ensure_content_pack_cached(
            &client,
            &cache_dir,
            "MgC4Oa2v",
            "Visual Effects+.zip",
            &url,
            || Ok(()),
        )
        .await
        .expect("first call downloads");
        let second = ensure_content_pack_cached(
            &client,
            &cache_dir,
            "MgC4Oa2v",
            "Visual Effects+.zip",
            &url,
            || panic!("a cached version must not announce a download"),
        )
        .await
        .expect("second call is a cache hit");

        assert_eq!((first.1, second.1), (false, true));
        assert_eq!(first.0, second.0);
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        let _ = fs::remove_dir_all(&cache_dir);
    }

    /// Real shape from `visual-effects-plus`: every one of its versions ships a
    /// file called `Visual Effects+.zip`, with different bytes each time
    /// (measured 2026-09-17: 1.3.0 for 1.21.1 is sha1 `134990cd3846`, 1.3.1 for
    /// the same target is `20365db0e8ee`). Keyed by filename, the second
    /// version reads as already cached and is never downloaded, so the update
    /// never lands and the launch says `(cached)`.
    #[test]
    fn two_versions_sharing_a_filename_get_separate_cache_entries() {
        let cache_dir = unique_test_root("separate-entries");
        let filename = "Visual Effects+.zip";

        let installed = content_pack_cache_path(&cache_dir, "5DrQdfaM", filename)
            .expect("the installed version has a cache path");
        let update = content_pack_cache_path(&cache_dir, "BsMkkGrN", filename)
            .expect("the newer version has a cache path");

        assert_ne!(
            installed, update,
            "two Modrinth versions must not share one cache entry"
        );

        fs::create_dir_all(installed.parent().expect("cache entry has a parent"))
            .expect("cache directory should be created");
        fs::write(&installed, b"1.3.0").expect("the installed pack should be on disk");

        assert!(
            !update.exists(),
            "a version that was never downloaded must not count as cached"
        );

        let _ = fs::remove_dir_all(&cache_dir);
    }

    /// The version id reaches the path from the network, so it is checked like
    /// the filename already was.
    #[test]
    fn refuses_a_version_id_that_would_escape_the_cache() {
        let cache_dir = unique_test_root("escaping-id");

        let error = content_pack_cache_path(&cache_dir, "../../etc", "pack.zip")
            .expect_err("a traversing version id must be refused");

        assert!(
            format!("{error:#}").contains("../../etc"),
            "the refusal names the offending id, got: {error:#}"
        );
    }



}
