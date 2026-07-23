#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;
use crate::mod_cache::ModCacheRecord;
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{
    version_rules_conflict, FailureReason, ModLoader, ResolutionResult, ResolutionTarget,
    RuleOutcome,
};
use crate::rules::{ModList, ModSource, Rule, RULES_FILENAME};

use super::{
    embedded_minecraft_requirements_match, emit_log, ensure_remote_version_cached,
    load_cached_mod_record_for_target, read_embedded_fabric_requirements, SelectedMod,
};

pub(super) struct TopLevelVersionCandidates {
    selected_mod_id: String,
    project_id: String,
    candidates: Vec<ModrinthVersion>,
}

#[derive(Debug, Clone)]
pub(super) enum RemoteArtifact {
    Live(ModrinthVersion),
    Cached(ModCacheRecord),
}
/// Pick the latest candidate by `date_published` (RFC3339 UTC, so lexicographic
/// comparison is chronological). Deterministic regardless of the order the
/// Modrinth API returned the versions in. Candidates are already filtered to the
/// exact/wildcard-compatible set by `fetch_project_versions`.
fn select_latest_by_date(mut versions: Vec<ModrinthVersion>) -> Option<ModrinthVersion> {
    versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
    versions.into_iter().next()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DownloadArtifact {
    pub(super) filename: String,
    pub(super) url: String,
    pub(super) destination_path: PathBuf,
    pub(super) file_hash: Option<String>,
    pub(super) file_size: u64,
}

/// Extracts the artifact name from a jar filename by stripping the version suffix.
/// e.g. "asm-9.6.jar" → "asm", "fabric-loader-0.16.jar" → "fabric-loader"
pub(in crate::launch_preview) fn extract_artifact_name(filename: &str) -> String {
    let stem = filename.strip_suffix(".jar").unwrap_or(filename);
    // Find the last '-' followed by a digit — everything before it is the artifact name
    if let Some(pos) = stem.rfind(|c: char| c == '-').and_then(|i| {
        if stem[i + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            Some(i)
        } else {
            None
        }
    }) {
        stem[..pos].to_string()
    } else {
        stem.to_string()
    }
}

pub(super) fn parse_mod_loader(value: &str) -> Result<ModLoader> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fabric" => Ok(ModLoader::Fabric),
        "forge" => Ok(ModLoader::Forge),
        "neoforge" => Ok(ModLoader::NeoForge),
        "vanilla" => Ok(ModLoader::Vanilla),
        "quilt" => bail!("Quilt is no longer supported by Cubic Launcher"),
        other => bail!("unsupported mod loader '{other}'"),
    }
}

pub(super) fn load_modlist(launcher_paths: &LauncherPaths, modlist_name: &str) -> Result<ModList> {
    validate_path_component(modlist_name)?;
    let rules_path = launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(RULES_FILENAME);

    ModList::read_from_file(&rules_path).with_context(|| {
        format!(
            "failed to read mod-list '{}' from {}",
            modlist_name,
            rules_path.display()
        )
    })
}

/// Fetch compatible versions only for the mods that were actually selected by resolution.
/// Network/API errors for individual mods are treated as "no compatible version"
/// so that the re-resolution pass can disable them and try alternatives.
pub(super) async fn prefetch_compatible_versions_for_selected(
    app_handle: &tauri::AppHandle,
    _launcher_paths: &LauncherPaths,
    _http_client: &reqwest::Client,
    selected_mods: &[SelectedMod],
    client: &ModrinthClient,
    target: &ResolutionTarget,
) -> Result<HashMap<String, ModrinthVersion>> {
    let mut versions = HashMap::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) {
            continue;
        }
        if versions.contains_key(&selected.mod_id) {
            continue;
        }
        match client
            .fetch_project_versions(&selected.mod_id, target)
            .await
        {
            Ok(candidate_versions) => {
                if let Some(version) = select_latest_by_date(candidate_versions) {
                    versions.insert(selected.mod_id.clone(), version);
                }
            }
            Err(err) => {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stderr,
                    format!(
                        "[Launch] skipping mod '{}': failed to query Modrinth ({:#})",
                        selected.mod_id, err
                    ),
                );
            }
        }
    }

    Ok(versions)
}

pub(super) async fn prefetch_ranked_versions_for_selected(
    app_handle: &tauri::AppHandle,
    selected_mods: &[SelectedMod],
    client: &ModrinthClient,
    target: &ResolutionTarget,
) -> Result<HashMap<String, Vec<ModrinthVersion>>> {
    let mut versions = HashMap::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth)
            || versions.contains_key(&selected.mod_id)
        {
            continue;
        }

        match client
            .fetch_project_versions(&selected.mod_id, target)
            .await
        {
            Ok(mut candidate_versions) => {
                crate::modrinth::sort_versions_by_target_preference(
                    &mut candidate_versions,
                    target,
                );
                if !candidate_versions.is_empty() {
                    versions.insert(selected.mod_id.clone(), candidate_versions);
                }
            }
            Err(err) => {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stderr,
                    format!(
                        "[Launch] skipping mod '{}': failed to query Modrinth ({:#})",
                        selected.mod_id, err
                    ),
                );
            }
        }
    }

    Ok(versions)
}

pub(super) async fn select_latest_launch_compatible_version(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    client: &ModrinthClient,
    project_id_or_slug: &str,
    target: &ResolutionTarget,
) -> Result<Option<ModrinthVersion>> {
    Ok(select_launch_compatible_versions(
        app_handle,
        launcher_paths,
        http_client,
        client,
        project_id_or_slug,
        target,
    )
    .await?
    .into_iter()
    .next())
}

pub(super) async fn select_launch_compatible_versions(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    client: &ModrinthClient,
    project_id_or_slug: &str,
    target: &ResolutionTarget,
) -> Result<Vec<ModrinthVersion>> {
    let mut versions = client
        .fetch_project_versions(project_id_or_slug, target)
        .await?;
    crate::modrinth::sort_versions_by_target_preference(&mut versions, target);

    let mut compatible_versions = Vec::new();
    for version in versions {
        let jar_path = ensure_remote_version_cached(http_client, launcher_paths, &version, target)
            .await
            .with_context(|| {
                format!(
                    "failed to cache '{}' candidate version '{}'",
                    project_id_or_slug, version.id
                )
            })?;
        let requirements = read_embedded_fabric_requirements(&jar_path)?;
        if requirements.entries.is_empty()
            || embedded_minecraft_requirements_match(&requirements, target)
        {
            compatible_versions.push(version);
            continue;
        }

        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Launch] skipping remote version '{}' for '{}': embedded metadata is incompatible with {} / {}",
                version.version_number,
                project_id_or_slug,
                target.minecraft_version,
                target.mod_loader.as_modrinth_loader()
            ),
        );
    }

    Ok(compatible_versions)
}

pub(super) fn log_resolution(
    app_handle: &tauri::AppHandle,
    resolution: &ResolutionResult,
) -> Result<()> {
    for rule in &resolution.resolved_rules {
        match &rule.outcome {
            RuleOutcome::Resolved { resolved_id } => emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!("[Resolver] {} -> {}", rule.mod_id, resolved_id,),
            )?,
            RuleOutcome::Unresolved { reason } => emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Resolver] {} unresolved ({})",
                    rule.mod_id,
                    describe_failure_reason(*reason)
                ),
            )?,
        }
    }

    Ok(())
}

pub(super) fn describe_failure_reason(reason: FailureReason) -> &'static str {
    match reason {
        FailureReason::ExcludedByActiveMod => "excluded by already-selected mods",
        FailureReason::RequiredModMissing => "required mod not active",
        FailureReason::IncompatibleVersion => "incompatible version/loader",
        FailureReason::NoOptionAvailable => "no compatible option remained",
    }
}

/// Collect mods for launch.  When a rule resolves (primary or alternative),
/// include whichever mod was selected by the resolver.
pub(super) fn collect_selected_mods(
    modlist: &ModList,
    resolution: &ResolutionResult,
    _target: &ResolutionTarget,
) -> Vec<SelectedMod> {
    let mut selected = Vec::new();

    for (i, resolved) in resolution.resolved_rules.iter().enumerate() {
        let Some(top_rule) = modlist.rules.get(i) else {
            continue;
        };

        if let RuleOutcome::Resolved { resolved_id } = &resolved.outcome {
            // Find the actual rule (primary or any nested alternative) to get its source.
            let rule = modlist.find_rule(resolved_id).unwrap_or(top_rule);
            selected.push(SelectedMod {
                mod_id: resolved_id.clone(),
                source: rule.source.clone(),
            });
        }
    }

    selected
}

pub(super) fn remote_artifact_project_id(artifact: &RemoteArtifact) -> &str {
    match artifact {
        RemoteArtifact::Live(version) => &version.project_id,
        RemoteArtifact::Cached(record) => &record.modrinth_project_id,
    }
}

pub(super) fn split_remote_artifacts(
    artifacts: &[RemoteArtifact],
) -> (Vec<ModrinthVersion>, Vec<ModCacheRecord>) {
    let mut live_versions = Vec::new();
    let mut cached_records = Vec::new();
    let mut seen_version_ids = HashSet::new();

    for artifact in artifacts {
        match artifact {
            RemoteArtifact::Live(version) => {
                if seen_version_ids.insert(version.id.clone()) {
                    live_versions.push(version.clone());
                }
            }
            RemoteArtifact::Cached(record) => {
                if seen_version_ids.insert(record.modrinth_version_id.clone()) {
                    cached_records.push(record.clone());
                }
            }
        }
    }

    (live_versions, cached_records)
}

pub(super) async fn resolve_selected_remote_artifacts(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    target: &ResolutionTarget,
) -> Result<HashMap<String, RemoteArtifact>> {
    let mut artifacts = HashMap::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth)
            || artifacts.contains_key(&selected.mod_id)
        {
            continue;
        }

        if let Some(record) =
            load_cached_mod_record_for_target(launcher_paths, &selected.mod_id, target)?
        {
            artifacts.insert(selected.mod_id.clone(), RemoteArtifact::Cached(record));
        }
    }

    Ok(artifacts)
}

pub(super) fn alt_viable_for_launch(
    rule: &Rule,
    active_mods: &HashSet<String>,
    target: &ResolutionTarget,
) -> bool {
    if !rule.enabled {
        return false;
    }
    if rule.exclude_if.iter().any(|id| active_mods.contains(id)) {
        return false;
    }
    if rule.requires.iter().any(|id| !active_mods.contains(id)) {
        return false;
    }
    !version_rules_conflict(&rule.version_rules, target)
}

pub(super) fn collect_resolved_parent_versions(
    selected_mods: &[SelectedMod],
    compatible_versions: &HashMap<String, Vec<ModrinthVersion>>,
) -> Vec<ModrinthVersion> {
    let mut versions = Vec::new();
    let mut seen_projects = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth)
            || !seen_projects.insert(selected.mod_id.clone())
        {
            continue;
        }

        // Skip mods that have no compatible version on Modrinth for this
        // target — they were resolved by local rules but simply don't have
        // a release for this MC version + loader.
        if let Some(version) = compatible_versions
            .get(&selected.mod_id)
            .and_then(|versions| versions.first())
        {
            versions.push(version.clone());
        }
    }

    versions
}

pub(super) fn collect_top_level_version_candidates(
    selected_mods: &[SelectedMod],
    compatible_versions: &HashMap<String, Vec<ModrinthVersion>>,
) -> Vec<TopLevelVersionCandidates> {
    let mut top_level_candidates = Vec::new();
    let mut seen_selected_mod_ids = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth)
            || !seen_selected_mod_ids.insert(selected.mod_id.clone())
        {
            continue;
        }

        let Some(candidates) = compatible_versions.get(&selected.mod_id) else {
            continue;
        };
        let Some(first_candidate) = candidates.first() else {
            continue;
        };

        top_level_candidates.push(TopLevelVersionCandidates {
            selected_mod_id: selected.mod_id.clone(),
            project_id: first_candidate.project_id.clone(),
            candidates: candidates.clone(),
        });
    }

    top_level_candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_version(id: &str, date_published: &str) -> ModrinthVersion {
        ModrinthVersion {
            id: id.into(),
            project_id: "example-project".into(),
            version_number: id.into(),
            name: id.into(),
            game_versions: vec!["26.1".into()],
            loaders: vec!["fabric".into()],
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: date_published.into(),
        }
    }

    #[test]
    fn select_latest_by_date_is_order_independent() {
        let january = test_version("january", "2024-01-01T00:00:00Z");
        let june = test_version("june", "2024-06-01T00:00:00Z");
        let march = test_version("march", "2024-03-01T00:00:00Z");

        let first = select_latest_by_date(vec![january.clone(), june.clone(), march.clone()]);
        let second = select_latest_by_date(vec![march, january, june.clone()]);

        assert_eq!(first, Some(june.clone()));
        assert_eq!(second, Some(june));
    }
}
