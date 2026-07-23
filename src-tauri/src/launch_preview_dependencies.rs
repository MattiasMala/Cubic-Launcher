#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use crate::dependencies::{DependencyLink, DependencyResolution, ResolvedDependency};
use crate::launcher_paths::LauncherPaths;
use crate::modrinth::{DependencyType, ModrinthClient, ModrinthVersion};
use crate::process_streaming::ProcessLogStream;
use crate::resolver::ResolutionTarget;

use super::fabric::{
    fabric_dependency_predicates_match, provided_ids_for_metadata,
    read_bundled_fabric_provided_ids, read_embedded_fabric_mod_metadata,
    read_root_fabric_mod_metadata, read_root_fabric_provided_ids, EmbeddedFabricModMetadata,
    FabricValidationIssue, OwnedEmbeddedFabricModMetadata,
};
use super::{
    deduplicate_versions, emit_log, ensure_remote_version_cached,
    select_latest_launch_compatible_version,
};

pub(super) fn collect_selected_project_ids(parent_versions: &[ModrinthVersion]) -> HashSet<String> {
    parent_versions
        .iter()
        .map(|version| version.project_id.clone())
        .collect()
}

pub(super) async fn resolve_embedded_metadata_dependencies(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    client: &ModrinthClient,
    target: &ResolutionTarget,
    parent_versions: &[ModrinthVersion],
    dependency_versions: &mut Vec<ModrinthVersion>,
    dependency_resolution: &mut DependencyResolution,
) -> Result<()> {
    let mut attempted_dependency_ids = HashSet::new();

    loop {
        let all_versions =
            deduplicate_versions(parent_versions.to_vec(), dependency_versions.clone());
        let metadata_entries = load_embedded_fabric_metadata_for_versions(
            launcher_paths,
            http_client,
            &all_versions,
            target,
        )
        .await?;
        let missing_dependencies = collect_missing_embedded_dependencies(&metadata_entries);
        if missing_dependencies.is_empty() {
            return Ok(());
        }

        let mut added_any = false;

        for (logical_dependency_id, owners) in missing_dependencies {
            if !attempted_dependency_ids.insert(logical_dependency_id.clone()) {
                continue;
            }

            let existing_project_ids = all_versions
                .iter()
                .map(|version| version.project_id.as_str())
                .collect::<HashSet<_>>();

            let resolved_version = resolve_embedded_dependency_version(
                app_handle,
                launcher_paths,
                http_client,
                client,
                target,
                &logical_dependency_id,
                &existing_project_ids,
            )
            .await?;

            let Some(version) = resolved_version else {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Dependencies] embedded dependency '{}' could not be resolved for {} / {}",
                        logical_dependency_id,
                        target.minecraft_version,
                        target.mod_loader.as_modrinth_loader()
                    ),
                );
                continue;
            };

            let primary_file = version.primary_file().with_context(|| {
                format!(
                    "embedded dependency '{}' version '{}' is missing a primary file",
                    logical_dependency_id, version.id
                )
            })?;

            if dependency_versions
                .iter()
                .all(|candidate| candidate.id != version.id)
            {
                dependency_versions.push(version.clone());
            }

            if dependency_resolution
                .resolved_dependencies
                .iter()
                .all(|dependency| dependency.version_id != version.id)
            {
                dependency_resolution
                    .resolved_dependencies
                    .push(ResolvedDependency {
                        dependency_id: version.project_id.clone(),
                        version_id: version.id.clone(),
                        jar_filename: primary_file.filename.clone(),
                        download_url: primary_file.url.clone(),
                        file_hash: primary_file.hashes.get("sha1").cloned(),
                        date_published: version.date_published.clone(),
                    });
            }

            for owner in owners {
                let already_linked = dependency_resolution.links.iter().any(|link| {
                    link.parent_mod_id == owner && link.dependency_id == version.project_id
                });
                if already_linked {
                    continue;
                }

                dependency_resolution.links.push(DependencyLink {
                    parent_mod_id: owner,
                    dependency_id: version.project_id.clone(),
                    specific_version: None,
                    jar_filename: primary_file.filename.clone(),
                });
            }

            let _ = emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Dependencies] added embedded dependency '{}' as '{}'",
                    logical_dependency_id, version.project_id
                ),
            );
            added_any = true;
        }

        if !added_any {
            return Ok(());
        }
    }
}

pub(super) async fn suppress_redundant_bundled_dependencies(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    target: &ResolutionTarget,
    parent_versions: &[ModrinthVersion],
    dependency_versions: &mut Vec<ModrinthVersion>,
    dependency_resolution: &mut DependencyResolution,
) -> Result<()> {
    let all_versions = deduplicate_versions(parent_versions.to_vec(), dependency_versions.clone());
    let mut bundled_ids_by_project: HashMap<String, HashSet<String>> = HashMap::new();

    for version in &all_versions {
        let jar_path =
            ensure_remote_version_cached(http_client, launcher_paths, version, target).await?;
        let bundled_ids = read_bundled_fabric_provided_ids(&jar_path)?;
        if !bundled_ids.is_empty() {
            bundled_ids_by_project.insert(version.project_id.clone(), bundled_ids);
        }
    }

    if bundled_ids_by_project.is_empty() {
        return Ok(());
    }

    let mut dependency_root_ids: HashMap<String, HashSet<String>> = HashMap::new();
    for version in dependency_versions.iter() {
        let jar_path =
            ensure_remote_version_cached(http_client, launcher_paths, version, target).await?;
        let root_ids = read_root_fabric_provided_ids(&jar_path)?;
        if !root_ids.is_empty() {
            dependency_root_ids.insert(version.project_id.clone(), root_ids);
        }
    }

    let mut removable_dependency_projects = HashSet::new();
    for dependency_version in dependency_versions.iter() {
        let Some(root_ids) = dependency_root_ids.get(&dependency_version.project_id) else {
            continue;
        };

        let links = dependency_resolution
            .links
            .iter()
            .filter(|link| {
                link.dependency_id == dependency_version.project_id
                    && link.specific_version.is_none()
            })
            .collect::<Vec<_>>();
        if links.is_empty() {
            continue;
        }

        let covered_by_bundled_parent = links.iter().all(|link| {
            bundled_ids_by_project
                .get(&link.parent_mod_id)
                .is_some_and(|bundled_ids| root_ids.iter().any(|id| bundled_ids.contains(id)))
        });
        if !covered_by_bundled_parent {
            continue;
        }

        removable_dependency_projects.insert(dependency_version.project_id.clone());
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Dependencies] dropped standalone dependency '{}' because all requiring parents already bundle it",
                dependency_version.project_id
            ),
        );
    }

    if removable_dependency_projects.is_empty() {
        return Ok(());
    }

    dependency_versions
        .retain(|version| !removable_dependency_projects.contains(&version.project_id));
    dependency_resolution
        .resolved_dependencies
        .retain(|dependency| !removable_dependency_projects.contains(&dependency.dependency_id));
    dependency_resolution
        .links
        .retain(|link| !removable_dependency_projects.contains(&link.dependency_id));

    Ok(())
}

async fn load_embedded_fabric_metadata_for_versions(
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
) -> Result<Vec<OwnedEmbeddedFabricModMetadata>> {
    let mut entries = Vec::new();

    for version in versions {
        let jar_path =
            ensure_remote_version_cached(http_client, launcher_paths, version, target).await?;
        for metadata in read_embedded_fabric_mod_metadata(&jar_path)? {
            entries.push(OwnedEmbeddedFabricModMetadata {
                owner_project_id: version.project_id.clone(),
                metadata,
            });
        }
    }

    Ok(entries)
}

async fn load_root_fabric_metadata_for_versions(
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
) -> Result<HashMap<String, EmbeddedFabricModMetadata>> {
    let mut entries = HashMap::new();

    for version in versions {
        let jar_path =
            ensure_remote_version_cached(http_client, launcher_paths, version, target).await?;
        if let Some(metadata) = read_root_fabric_mod_metadata(&jar_path)? {
            entries.insert(version.project_id.clone(), metadata);
        }
    }

    Ok(entries)
}

fn collect_missing_embedded_dependencies(
    entries: &[OwnedEmbeddedFabricModMetadata],
) -> Vec<(String, Vec<String>)> {
    let mut provided_ids = HashSet::new();
    for entry in entries {
        if !entry.metadata.mod_id.trim().is_empty() {
            provided_ids.insert(entry.metadata.mod_id.clone());
        }
        provided_ids.extend(entry.metadata.provides.iter().cloned());
    }

    let mut missing_by_dependency: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries {
        for dependency_id in entry.metadata.depends.keys() {
            if embedded_dependency_is_builtin(dependency_id) || provided_ids.contains(dependency_id)
            {
                continue;
            }
            missing_by_dependency
                .entry(dependency_id.clone())
                .or_default()
                .insert(entry.owner_project_id.clone());
        }
    }

    let mut missing = missing_by_dependency
        .into_iter()
        .map(|(dependency_id, owners)| {
            let mut owners = owners.into_iter().collect::<Vec<_>>();
            owners.sort();
            (dependency_id, owners)
        })
        .collect::<Vec<_>>();
    missing.sort_by(|left, right| left.0.cmp(&right.0));
    missing
}

pub(super) fn build_top_level_owner_map(
    parent_versions: &[ModrinthVersion],
    dependency_links: &[DependencyLink],
) -> HashMap<String, HashSet<String>> {
    let mut owners = parent_versions
        .iter()
        .map(|version| {
            (
                version.project_id.clone(),
                HashSet::from([version.project_id.clone()]),
            )
        })
        .collect::<HashMap<_, _>>();

    loop {
        let mut changed = false;

        for link in dependency_links {
            let parent_owners = owners.get(&link.parent_mod_id).cloned().unwrap_or_default();
            if parent_owners.is_empty() {
                continue;
            }

            let dependency_owners = owners.entry(link.dependency_id.clone()).or_default();
            let previous_len = dependency_owners.len();
            dependency_owners.extend(parent_owners);
            if dependency_owners.len() != previous_len {
                changed = true;
            }
        }

        if !changed {
            return owners;
        }
    }
}

pub(super) fn collect_top_level_owner_ids(
    project_ids: &HashSet<String>,
    owner_map: &HashMap<String, HashSet<String>>,
) -> HashSet<String> {
    let mut top_level_ids = HashSet::new();

    for project_id in project_ids {
        if let Some(owners) = owner_map.get(project_id) {
            top_level_ids.extend(owners.iter().cloned());
        }
    }

    top_level_ids
}

pub(super) fn validate_final_fabric_runtime(
    metadata_entries: &[OwnedEmbeddedFabricModMetadata],
    owner_map: &HashMap<String, HashSet<String>>,
) -> HashMap<String, FabricValidationIssue> {
    let mut providers_by_id: HashMap<String, Vec<&OwnedEmbeddedFabricModMetadata>> = HashMap::new();
    for entry in metadata_entries {
        for provided_id in provided_ids_for_metadata(&entry.metadata) {
            providers_by_id.entry(provided_id).or_default().push(entry);
        }
    }

    let mut issues = HashMap::new();
    for entry in metadata_entries {
        let Some(top_level_owners) = owner_map.get(&entry.owner_project_id) else {
            continue;
        };

        for (dependency_id, predicates) in &entry.metadata.depends {
            if embedded_dependency_is_builtin(dependency_id) {
                continue;
            }

            let providers = providers_by_id.get(dependency_id);
            let satisfied = providers.is_some_and(|providers| {
                providers.iter().any(|provider| {
                    fabric_dependency_predicates_match(predicates, &provider.metadata.version)
                })
            });
            if satisfied {
                continue;
            }

            let reason_code = if providers.is_some() {
                "incompatible_dependency_version"
            } else {
                "missing_dependency"
            };
            let detail = if providers.is_some() {
                format!(
                    "embedded metadata requires '{}' with a compatible version, but only incompatible versions are present",
                    dependency_id
                )
            } else {
                format!(
                    "embedded metadata requires '{}', which is missing",
                    dependency_id
                )
            };

            for top_level_owner in top_level_owners {
                issues
                    .entry(top_level_owner.clone())
                    .or_insert_with(|| FabricValidationIssue {
                        reason_code,
                        owner_project_id: entry.owner_project_id.clone(),
                        mod_id: entry.metadata.mod_id.clone(),
                        dependency_id: Some(dependency_id.clone()),
                        detail: detail.clone(),
                    });
            }
        }

        for (dependency_id, predicates) in &entry.metadata.breaks {
            let Some(providers) = providers_by_id.get(dependency_id) else {
                continue;
            };
            let Some(conflicting_provider) = providers.iter().find(|provider| {
                fabric_dependency_predicates_match(predicates, &provider.metadata.version)
            }) else {
                continue;
            };

            let detail = format!(
                "embedded metadata breaks '{}' version {}",
                dependency_id, conflicting_provider.metadata.version
            );
            for top_level_owner in top_level_owners {
                issues
                    .entry(top_level_owner.clone())
                    .or_insert_with(|| FabricValidationIssue {
                        reason_code: "breaks_conflict",
                        owner_project_id: entry.owner_project_id.clone(),
                        mod_id: entry.metadata.mod_id.clone(),
                        dependency_id: Some(dependency_id.clone()),
                        detail: detail.clone(),
                    });
            }
        }
    }

    issues
}

pub(super) fn validate_root_parent_fabric_runtime(
    parent_metadata_by_project: &HashMap<String, EmbeddedFabricModMetadata>,
    all_metadata_entries: &[OwnedEmbeddedFabricModMetadata],
) -> HashMap<String, FabricValidationIssue> {
    let mut providers_by_id: HashMap<String, Vec<&OwnedEmbeddedFabricModMetadata>> = HashMap::new();
    for entry in all_metadata_entries {
        for provided_id in provided_ids_for_metadata(&entry.metadata) {
            providers_by_id.entry(provided_id).or_default().push(entry);
        }
    }

    let mut issues = HashMap::new();
    for (project_id, metadata) in parent_metadata_by_project {
        for (dependency_id, predicates) in &metadata.depends {
            if embedded_dependency_is_builtin(dependency_id) {
                continue;
            }

            let providers = providers_by_id.get(dependency_id);
            let satisfied = providers.is_some_and(|providers| {
                providers.iter().any(|provider| {
                    fabric_dependency_predicates_match(predicates, &provider.metadata.version)
                })
            });
            if satisfied {
                continue;
            }

            let reason_code = if providers.is_some() {
                "incompatible_dependency_version"
            } else {
                "missing_dependency"
            };
            issues.insert(
                project_id.clone(),
                FabricValidationIssue {
                    reason_code,
                    owner_project_id: project_id.clone(),
                    mod_id: metadata.mod_id.clone(),
                    dependency_id: Some(dependency_id.clone()),
                    detail: if providers.is_some() {
                        format!(
                            "embedded metadata requires '{}' with a compatible version, but only incompatible versions are present",
                            dependency_id
                        )
                    } else {
                        format!("embedded metadata requires '{}', which is missing", dependency_id)
                    },
                },
            );
            break;
        }

        if issues.contains_key(project_id) {
            continue;
        }

        for (dependency_id, predicates) in &metadata.breaks {
            let Some(providers) = providers_by_id.get(dependency_id) else {
                continue;
            };
            let Some(conflicting_provider) = providers.iter().find(|provider| {
                fabric_dependency_predicates_match(predicates, &provider.metadata.version)
            }) else {
                continue;
            };

            issues.insert(
                project_id.clone(),
                FabricValidationIssue {
                    reason_code: "breaks_conflict",
                    owner_project_id: project_id.clone(),
                    mod_id: metadata.mod_id.clone(),
                    dependency_id: Some(dependency_id.clone()),
                    detail: format!(
                        "embedded metadata breaks '{}' version {}",
                        dependency_id, conflicting_provider.metadata.version
                    ),
                },
            );
            break;
        }
    }

    issues
}

async fn resolve_embedded_dependency_version(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    client: &ModrinthClient,
    target: &ResolutionTarget,
    logical_dependency_id: &str,
    existing_project_ids: &HashSet<&str>,
) -> Result<Option<ModrinthVersion>> {
    for candidate_project_id in embedded_dependency_project_candidates(logical_dependency_id) {
        if existing_project_ids.contains(candidate_project_id.as_str()) {
            return Ok(None);
        }

        if let Some(version) = select_latest_launch_compatible_version(
            app_handle,
            launcher_paths,
            http_client,
            client,
            &candidate_project_id,
            target,
        )
        .await?
        {
            return Ok(Some(version));
        }
    }

    Ok(None)
}

fn embedded_dependency_project_candidates(logical_dependency_id: &str) -> Vec<String> {
    match logical_dependency_id {
        "fabric" | "fabric-api" => vec!["fabric-api".to_string()],
        other => vec![other.to_string()],
    }
}

fn embedded_dependency_is_builtin(dependency_id: &str) -> bool {
    matches!(
        dependency_id.trim().to_ascii_lowercase().as_str(),
        "minecraft" | "java" | "fabricloader" | "fabric-loader" | "quilt_loader" | "quiltloader"
    )
}

pub(super) fn validate_selected_parent_dependencies(
    parent_versions: &[ModrinthVersion],
    selected_parent_versions: &HashMap<String, ModrinthVersion>,
    selected_project_ids: &HashSet<String>,
) -> HashSet<String> {
    let mut excluded_parents = HashSet::new();

    for parent_version in parent_versions {
        for dependency in &parent_version.dependencies {
            if dependency.dependency_type != DependencyType::Required {
                continue;
            }

            let Some(project_id) = dependency
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };

            if !selected_project_ids.contains(project_id) {
                continue;
            }

            let Some(selected_version) = selected_parent_versions.get(project_id) else {
                continue;
            };

            let exact_version_matches = dependency
                .version_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none_or(|version_id| version_id == selected_version.id);

            if !exact_version_matches {
                excluded_parents.insert(parent_version.project_id.clone());
                break;
            }
        }
    }

    excluded_parents
}

// ── Informational dependency detection (no auto-management) ──────────────────
//
// The launcher does NOT manage dependencies. It DETECTS what each selected mod
// DECLARES it needs — from Modrinth metadata (incl. a version_id pin) and from
// the embedded fabric.mod.json version predicates — evaluates it against what is
// actually in the modlist for the EXACT target, and reports mismatches as
// informational notices. It never downloads a declared dependency, never pins,
// never excludes a parent. The user decides (manual jar / removal); Fabric is
// the runtime safety net.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DependencyNoticeKind {
    /// A declared required dependency's project is not in the modlist at all.
    Missing,
    /// The dependency project IS in the modlist, but the requirement declares a
    /// version (Modrinth version_id pin, or an embedded predicate) that the
    /// selected exact-tagged version does not satisfy.
    VersionUnsatisfied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DependencyNotice {
    pub(super) requiring_project_id: String,
    pub(super) dependency_id: String,
    pub(super) kind: DependencyNoticeKind,
    pub(super) detail: String,
}

/// Pure core: notices from Modrinth-declared REQUIRED dependencies of the
/// selected top-level versions, evaluated against the selected set.
///
/// - dependency project not selected                 → Missing
/// - dependency selected but pinned version_id differs from the selected
///   version's id                                    → VersionUnsatisfied
///
/// `selected_versions` maps project_id → the selected ModrinthVersion.
/// `pin_labels` maps a pinned version_id → a human label (version_number) when
/// known (resolved only to REPORT, never to download); missing labels fall back
/// to the raw id.
pub(super) fn detect_modrinth_declared_notices(
    parent_versions: &[ModrinthVersion],
    selected_versions: &HashMap<String, ModrinthVersion>,
    pin_labels: &HashMap<String, String>,
) -> Vec<DependencyNotice> {
    let mut notices = Vec::new();

    for parent in parent_versions {
        for dependency in &parent.dependencies {
            if dependency.dependency_type != DependencyType::Required {
                continue;
            }
            let Some(project_id) = dependency
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };

            let pin = dependency
                .version_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());

            match selected_versions.get(project_id) {
                None => {
                    let want = pin
                        .map(|id| {
                            format!(
                                " {}",
                                pin_labels.get(id).cloned().unwrap_or_else(|| id.to_string())
                            )
                        })
                        .unwrap_or_default();
                    notices.push(DependencyNotice {
                        requiring_project_id: parent.project_id.clone(),
                        dependency_id: project_id.to_string(),
                        kind: DependencyNoticeKind::Missing,
                        detail: format!(
                            "'{}' declares it requires '{}'{} — not present in this mod-list for this Minecraft version.",
                            parent.project_id, project_id, want
                        ),
                    });
                }
                Some(selected) => {
                    if let Some(pin) = pin {
                        if pin != selected.id {
                            let want = pin_labels
                                .get(pin)
                                .cloned()
                                .unwrap_or_else(|| pin.to_string());
                            notices.push(DependencyNotice {
                                requiring_project_id: parent.project_id.clone(),
                                dependency_id: project_id.to_string(),
                                kind: DependencyNoticeKind::VersionUnsatisfied,
                                detail: format!(
                                    "'{}' declares it requires '{}' version {} — the mod-list has version {} for this Minecraft version.",
                                    parent.project_id, project_id, want, selected.version_number
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    notices
}

/// Pure core: convert an embedded-predicate FabricValidationIssue into a notice.
/// `incompatible_dependency_version`/`breaks_conflict` → VersionUnsatisfied;
/// `missing_dependency` → Missing.
pub(super) fn fabric_issue_to_notice(
    requiring_project_id: &str,
    issue: &FabricValidationIssue,
) -> DependencyNotice {
    let kind = match issue.reason_code {
        "missing_dependency" => DependencyNoticeKind::Missing,
        _ => DependencyNoticeKind::VersionUnsatisfied,
    };
    DependencyNotice {
        requiring_project_id: requiring_project_id.to_string(),
        dependency_id: issue.dependency_id.clone().unwrap_or_default(),
        kind,
        detail: format!(
            "'{}' (embedded '{}'): {}",
            requiring_project_id, issue.mod_id, issue.detail
        ),
    }
}

/// Async wrapper: gather Modrinth-declared + embedded-predicate notices for the
/// selected top-level versions. Network is used ONLY to read a pin's label and
/// to read embedded metadata from the already-cached jars — never to download a
/// declared dependency. Best-effort: network failures degrade to raw ids /
/// skipped embedded checks, never block the launch.
pub(super) async fn detect_dependency_notices(
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    client: &ModrinthClient,
    target: &ResolutionTarget,
    parent_versions: &[ModrinthVersion],
) -> Vec<DependencyNotice> {
    let selected_versions: HashMap<String, ModrinthVersion> = parent_versions
        .iter()
        .map(|version| (version.project_id.clone(), version.clone()))
        .collect();

    // Resolve pin labels for reporting only (best-effort).
    let mut pin_labels = HashMap::new();
    for parent in parent_versions {
        for dependency in &parent.dependencies {
            if dependency.dependency_type != DependencyType::Required {
                continue;
            }
            if let Some(pin) = dependency
                .version_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                if !pin_labels.contains_key(pin) {
                    if let Ok(Some(version)) = client.fetch_version(pin).await {
                        pin_labels.insert(pin.to_string(), version.version_number);
                    }
                }
            }
        }
    }

    let mut notices =
        detect_modrinth_declared_notices(parent_versions, &selected_versions, &pin_labels);

    // Embedded fabric.mod.json predicates over the selected jars (already cached).
    if let Ok(metadata_entries) =
        load_embedded_fabric_metadata_for_versions(launcher_paths, http_client, parent_versions, target)
            .await
    {
        let owner_map = build_top_level_owner_map(parent_versions, &[]);
        let issues = validate_final_fabric_runtime(&metadata_entries, &owner_map);
        for (requiring_project_id, issue) in issues {
            let notice = fabric_issue_to_notice(&requiring_project_id, &issue);
            // De-dup against a Modrinth-declared notice for the same pair.
            if !notices.iter().any(|existing| {
                existing.requiring_project_id == notice.requiring_project_id
                    && existing.dependency_id == notice.dependency_id
            }) {
                notices.push(notice);
            }
        }
    }

    notices.sort_by(|left, right| {
        left.requiring_project_id
            .cmp(&right.requiring_project_id)
            .then_with(|| left.dependency_id.cmp(&right.dependency_id))
    });
    notices
}
