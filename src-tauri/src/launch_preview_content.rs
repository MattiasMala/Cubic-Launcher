use std::path::Path;

use anyhow::{Context, Result};

use crate::content_packs::{load_content_list, ContentEntry, ContentList};
use crate::launcher_paths::LauncherPaths;
use crate::local_content_packs::{link_local_pack, plan_local_pack_installs, PackLinkKind};
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
    instance_subdir: &str,
    instance_dir: &Path,
    active_entries: &[&ContentEntry],
) -> Result<Vec<String>> {
    let mut installed = Vec::new();

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
        let kind = link_local_pack(&pack.source_path, &target_path).with_context(|| {
            format!(
                "failed to install local content pack '{}' into instance",
                pack.file_name
            )
        })?;

        if !installed.iter().any(|name| name == &pack.file_name) {
            installed.push(pack.file_name.clone());
        }

        let how = match kind {
            PackLinkKind::Linked => "",
            // Not silent: a copied folder is a second set of bytes on disk, and
            // whoever reads the log has to be able to tell why.
            PackLinkKind::Copied => " (copied: this platform refused a directory link)",
        };
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
                            validate_content_filename(&file.filename)?;
                            let cached_path = cache_dir.join(&file.filename);
                            let was_cached = cached_path.exists();
                            if !was_cached {
                                emit_log(
                                    app_handle,
                                    ProcessLogStream::Stdout,
                                    format!(
                                        "[Content] Downloading {} ({})",
                                        entry.id, file.filename
                                    ),
                                )?;
                                download_file(http_client, &file.url, &cached_path)
                                    .await
                                    .with_context(|| {
                                        format!("failed to download content pack '{}'", entry.id)
                                    })?;
                            }
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
                        validate_content_filename(&file.filename)?;
                        let cached_path = cache_dir.join(&file.filename);
                        let was_cached = cached_path.exists();
                        if !was_cached {
                            emit_log(
                                app_handle,
                                ProcessLogStream::Stdout,
                                format!("[Content] Downloading {} ({})", entry.id, file.filename),
                            )?;
                            download_file(http_client, &file.url, &cached_path)
                                .await
                                .with_context(|| {
                                    format!("failed to download data pack '{}'", entry.id)
                                })?;
                        }
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
