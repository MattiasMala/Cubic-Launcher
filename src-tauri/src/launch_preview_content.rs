use std::path::Path;

use anyhow::{Context, Result};

use crate::content_packs::{load_content_list, ContentEntry, ContentList};
use crate::launcher_paths::LauncherPaths;
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
        if instance_dir.exists() {
            crate::instance_mods::clear_instance_mods_directory(&instance_dir)?;
        }

        if active_entries.is_empty() {
            continue;
        }

        std::fs::create_dir_all(&instance_dir)
            .with_context(|| format!("failed to create {}", instance_dir.display()))?;

        for entry in &active_entries {
            if entry.source != "modrinth" {
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
    if instance_dir.exists() {
        crate::instance_mods::clear_instance_mods_directory(&instance_dir)?;
    }

    if active_entries.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(&instance_dir)
        .with_context(|| format!("failed to create {}", instance_dir.display()))?;

    for entry in &active_entries {
        if entry.source != "modrinth" {
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

    Ok(())
}
