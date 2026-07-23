
use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::dependencies::DependencyLink;
use crate::launcher_paths::LauncherPaths;
use crate::modrinth::{DependencyType, ModrinthClient, ModrinthVersion};
use crate::resolver::ResolutionTarget;

use super::fabric::{
    fabric_dependency_predicates_match, provided_ids_for_metadata,
    read_embedded_fabric_mod_metadata, FabricValidationIssue, OwnedEmbeddedFabricModMetadata,
};
use super::{emit_launcher_issue, ensure_remote_version_cached};

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

fn embedded_dependency_is_builtin(dependency_id: &str) -> bool {
    matches!(
        dependency_id.trim().to_ascii_lowercase().as_str(),
        "minecraft" | "java" | "fabricloader" | "fabric-loader" | "quilt_loader" | "quiltloader"
    )
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

/// Emit each dependency notice to the ErrorCenter as a non-blocking warning
/// (severity "warning", scope "launch") so it is visible as a decision, not
/// buried in the log stream. The launch is never blocked — the user, now
/// informed, may proceed (Fabric is the runtime safety net).
pub(super) fn emit_dependency_notices(
    app_handle: &tauri::AppHandle,
    notices: &[DependencyNotice],
) -> Result<()> {
    for notice in notices {
        let title = match notice.kind {
            DependencyNoticeKind::Missing => "Missing dependency",
            DependencyNoticeKind::VersionUnsatisfied => "Dependency version mismatch",
        };
        let message = match notice.kind {
            DependencyNoticeKind::Missing => format!(
                "'{}' declares a required dependency that is not in this mod-list.",
                notice.requiring_project_id
            ),
            DependencyNoticeKind::VersionUnsatisfied => format!(
                "'{}' declares a dependency version the mod-list does not provide for this Minecraft version.",
                notice.requiring_project_id
            ),
        };
        emit_launcher_issue(
            app_handle,
            title,
            &message,
            &notice.detail,
            "warning",
            "launch",
        )?;
    }
    Ok(())
}
