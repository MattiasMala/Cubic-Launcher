use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};

use crate::modrinth::{DependencyType, ModrinthClient, ModrinthVersion};
use crate::resolver::ResolutionTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRequest {
    pub parent_mod_id: String,
    pub selector: DependencySelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySelector {
    ProjectId { project_id: String },
    VersionId { version_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDependency {
    pub dependency_id: String,
    pub version_id: String,
    pub jar_filename: String,
    pub download_url: String,
    pub file_hash: Option<String>,
    pub date_published: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyLink {
    pub parent_mod_id: String,
    pub dependency_id: String,
    pub specific_version: Option<String>,
    pub jar_filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyResolution {
    pub resolved_dependencies: Vec<ResolvedDependency>,
    pub links: Vec<DependencyLink>,
    /// Parent mod project IDs that were excluded because a required dependency
    /// had no compatible version available for the target.
    pub excluded_parents: HashSet<String>,
}

#[derive(Debug, Clone)]
struct DependencyCandidate {
    parent_mod_id: String,
    selector: DependencySelector,
    resolved_dependency: ResolvedDependency,
    version: ModrinthVersion,
}

pub fn collect_required_dependency_requests(
    parent_versions: &[ModrinthVersion],
) -> Result<Vec<DependencyRequest>> {
    let mut requests = Vec::new();

    for parent_version in parent_versions {
        for dependency in &parent_version.dependencies {
            if dependency.dependency_type != DependencyType::Required {
                continue;
            }

            let selector = if let Some(version_id) = dependency
                .version_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                DependencySelector::VersionId {
                    version_id: version_id.to_string(),
                }
            } else if let Some(project_id) = dependency
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                DependencySelector::ProjectId {
                    project_id: project_id.to_string(),
                }
            } else {
                bail!(
                    "required dependency for parent '{}' is missing both project_id and version_id",
                    parent_version.project_id
                );
            };

            requests.push(DependencyRequest {
                parent_mod_id: parent_version.project_id.clone(),
                selector,
            });
        }
    }

    Ok(requests)
}

pub fn resolve_dependency_requests(
    requests: &[DependencyRequest],
    mut fetch_latest_compatible: impl FnMut(&str) -> Result<Option<ModrinthVersion>>,
    mut fetch_exact_version: impl FnMut(&str) -> Result<Option<ModrinthVersion>>,
) -> Result<DependencyResolution> {
    let mut candidates = Vec::with_capacity(requests.len());

    for request in requests {
        let version = match &request.selector {
            DependencySelector::ProjectId { project_id } => fetch_latest_compatible(project_id)
                .with_context(|| {
                    format!(
                        "failed to resolve compatible dependency version for project '{}'",
                        project_id
                    )
                })?
                .with_context(|| {
                    format!(
                        "no compatible dependency version found for project '{}'",
                        project_id
                    )
                })?,
            DependencySelector::VersionId { version_id } => fetch_exact_version(version_id)
                .with_context(|| {
                    format!(
                        "failed to resolve exact dependency version '{}'",
                        version_id
                    )
                })?
                .with_context(|| {
                    format!("dependency version '{}' could not be found", version_id)
                })?,
        };

        candidates.push(build_dependency_candidate(request, version)?);
    }

    finalize_dependency_resolution(candidates)
}

pub async fn resolve_required_dependencies_with_client(
    parent_versions: &[ModrinthVersion],
    target: &ResolutionTarget,
    client: &ModrinthClient,
    selected_mod_ids: &HashSet<String>,
) -> Result<DependencyResolution> {
    let mut excluded_parents: HashSet<String> = HashSet::new();
    let mut candidates = Vec::new();
    let mut frontier = parent_versions.to_vec();
    let mut processed_version_ids = HashSet::new();

    while !frontier.is_empty() {
        let requests = collect_required_dependency_requests(&frontier)?;
        frontier.clear();

        for request in &requests {
            // Skip dependencies that are already explicitly selected in the mod
            // list — those are managed by the user and will be fetched (or
            // gracefully skipped) as parent mods, not as auto-resolved deps.
            if let DependencySelector::ProjectId { project_id } = &request.selector {
                if selected_mod_ids.contains(project_id) {
                    continue;
                }
            }

            // If this parent was already excluded due to a previous missing
            // dependency, skip all its remaining requests.
            if excluded_parents.contains(&request.parent_mod_id) {
                continue;
            }

            let version = match &request.selector {
                DependencySelector::ProjectId { project_id } => {
                    match client
                        .fetch_project_versions(project_id, target)
                        .await
                        .with_context(|| {
                            format!(
                                "failed to resolve compatible dependency version for project '{}'",
                                project_id
                            )
                        })?
                        .into_iter()
                        .next()
                    {
                        Some(v) => v,
                        None => {
                            excluded_parents.insert(request.parent_mod_id.clone());
                            continue;
                        }
                    }
                }
                DependencySelector::VersionId { version_id } => {
                    match client.fetch_version(version_id).await.with_context(|| {
                        format!(
                            "failed to resolve exact dependency version '{}'",
                            version_id
                        )
                    })? {
                        Some(v) => v,
                        None => {
                            excluded_parents.insert(request.parent_mod_id.clone());
                            continue;
                        }
                    }
                }
            };

            candidates.push(build_dependency_candidate(request, version)?);
        }

        let (selected_candidates, next_excluded_parents) =
            finalize_dependency_candidates(&candidates, excluded_parents.clone());
        excluded_parents = next_excluded_parents;

        frontier = selected_candidates
            .values()
            .filter(|candidate| processed_version_ids.insert(candidate.version.id.clone()))
            .map(|candidate| candidate.version.clone())
            .collect::<Vec<_>>();
    }

    let selected_candidates =
        finalize_dependency_candidates(&candidates, excluded_parents.clone()).0;
    let mut resolution =
        build_dependency_resolution(candidates, selected_candidates, excluded_parents.clone())?;
    resolution.excluded_parents.extend(excluded_parents);
    Ok(resolution)
}

fn build_dependency_candidate(
    request: &DependencyRequest,
    version: ModrinthVersion,
) -> Result<DependencyCandidate> {
    let primary_file = version.primary_file().with_context(|| {
        format!(
            "dependency version '{}' for project '{}' does not expose any downloadable file",
            version.id, version.project_id
        )
    })?;

    Ok(DependencyCandidate {
        parent_mod_id: request.parent_mod_id.clone(),
        selector: request.selector.clone(),
        resolved_dependency: ResolvedDependency {
            dependency_id: version.project_id.clone(),
            version_id: version.id.clone(),
            jar_filename: primary_file.filename.clone(),
            download_url: primary_file.url.clone(),
            file_hash: primary_file.hashes.get("sha1").cloned(),
            date_published: version.date_published.clone(),
        },
        version,
    })
}

fn finalize_dependency_resolution(
    candidates: Vec<DependencyCandidate>,
) -> Result<DependencyResolution> {
    let (selected_candidates, excluded_parents) =
        finalize_dependency_candidates(&candidates, HashSet::new());
    build_dependency_resolution(candidates, selected_candidates, excluded_parents)
}

fn finalize_dependency_candidates(
    candidates: &[DependencyCandidate],
    initial_excluded_parents: HashSet<String>,
) -> (HashMap<String, DependencyCandidate>, HashSet<String>) {
    let mut excluded_parents = initial_excluded_parents;

    loop {
        let valid_candidates = candidates
            .iter()
            .filter(|candidate| !excluded_parents.contains(&candidate.parent_mod_id))
            .cloned()
            .collect::<Vec<_>>();

        let (selected_candidates, mut newly_excluded_parents) =
            select_dependency_candidates(&valid_candidates);

        // If a dependency itself is excluded because one of its own required
        // dependencies is missing, exclude every parent that depends on it.
        for candidate in &valid_candidates {
            if excluded_parents.contains(&candidate.resolved_dependency.dependency_id) {
                newly_excluded_parents.insert(candidate.parent_mod_id.clone());
            }
        }

        if newly_excluded_parents.is_empty() {
            return (selected_candidates, excluded_parents);
        }

        excluded_parents.extend(newly_excluded_parents);
    }
}

fn select_dependency_candidates(
    candidates: &[DependencyCandidate],
) -> (HashMap<String, DependencyCandidate>, HashSet<String>) {
    let mut groups: HashMap<String, Vec<&DependencyCandidate>> = HashMap::new();
    for candidate in candidates {
        groups
            .entry(candidate.resolved_dependency.dependency_id.clone())
            .or_default()
            .push(candidate);
    }

    let mut selected_by_dependency = HashMap::new();
    let mut excluded_parents = HashSet::new();

    for (dependency_id, group) in groups {
        let exact_candidates = group
            .iter()
            .copied()
            .filter(|candidate| matches!(candidate.selector, DependencySelector::VersionId { .. }))
            .collect::<Vec<_>>();

        let selected = if exact_candidates.is_empty() {
            group.into_iter().max_by(|left, right| {
                left.resolved_dependency
                    .date_published
                    .cmp(&right.resolved_dependency.date_published)
            })
        } else {
            let distinct_exact_versions = exact_candidates
                .iter()
                .map(|candidate| candidate.resolved_dependency.version_id.as_str())
                .collect::<HashSet<_>>();

            if distinct_exact_versions.len() > 1 {
                for candidate in exact_candidates {
                    excluded_parents.insert(candidate.parent_mod_id.clone());
                }
                None
            } else {
                exact_candidates.into_iter().max_by(|left, right| {
                    left.resolved_dependency
                        .date_published
                        .cmp(&right.resolved_dependency.date_published)
                })
            }
        };

        if let Some(selected) = selected {
            selected_by_dependency.insert(dependency_id, selected.clone());
        }
    }

    for candidate in candidates {
        let Some(selected) =
            selected_by_dependency.get(&candidate.resolved_dependency.dependency_id)
        else {
            continue;
        };

        if matches!(candidate.selector, DependencySelector::VersionId { .. })
            && selected.resolved_dependency.version_id != candidate.resolved_dependency.version_id
        {
            excluded_parents.insert(candidate.parent_mod_id.clone());
        }
    }

    (selected_by_dependency, excluded_parents)
}

fn build_dependency_resolution(
    candidates: Vec<DependencyCandidate>,
    selected_by_dependency: HashMap<String, DependencyCandidate>,
    excluded_parents: HashSet<String>,
) -> Result<DependencyResolution> {
    let mut resolved_dependencies = selected_by_dependency
        .values()
        .map(|candidate| candidate.resolved_dependency.clone())
        .collect::<Vec<_>>();
    resolved_dependencies.sort_by(|left, right| left.dependency_id.cmp(&right.dependency_id));

    let mut deduplicated_links = HashMap::new();
    for candidate in candidates {
        if excluded_parents.contains(&candidate.parent_mod_id) {
            continue;
        }

        let Some(selected_candidate) =
            selected_by_dependency.get(&candidate.resolved_dependency.dependency_id)
        else {
            continue;
        };

        if matches!(candidate.selector, DependencySelector::VersionId { .. })
            && selected_candidate.resolved_dependency.version_id
                != candidate.resolved_dependency.version_id
        {
            continue;
        }

        let selected_dependency = &selected_candidate.resolved_dependency;
        let specific_version = match candidate.selector {
            DependencySelector::ProjectId { .. } => None,
            DependencySelector::VersionId { .. } => Some(selected_dependency.version_id.clone()),
        };

        deduplicated_links.insert(
            (
                candidate.parent_mod_id.clone(),
                selected_dependency.dependency_id.clone(),
            ),
            DependencyLink {
                parent_mod_id: candidate.parent_mod_id,
                dependency_id: selected_dependency.dependency_id.clone(),
                specific_version,
                jar_filename: selected_dependency.jar_filename.clone(),
            },
        );
    }

    let mut links = deduplicated_links.into_values().collect::<Vec<_>>();
    links.sort_by(|left, right| {
        left.parent_mod_id
            .cmp(&right.parent_mod_id)
            .then(left.dependency_id.cmp(&right.dependency_id))
    });

    Ok(DependencyResolution {
        resolved_dependencies,
        links,
        excluded_parents,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use crate::modrinth::ModrinthVersion;

    use super::{
        collect_required_dependency_requests, resolve_dependency_requests, DependencyLink,
        build_dependency_candidate, finalize_dependency_candidates,
        DependencyRequest, DependencyResolution, DependencySelector,
    };

    fn version_from_json(json: &str) -> ModrinthVersion {
        serde_json::from_str(json).expect("json should deserialize")
    }

    fn parent_version_with_mixed_dependencies() -> ModrinthVersion {
        version_from_json(
            r#"
            {
              "id": "parent-version",
              "project_id": "create",
              "version_number": "1.0.0",
              "name": "Create",
              "game_versions": ["1.21.1"],
              "loaders": ["fabric"],
              "date_published": "2024-07-01T00:00:00.000Z",
              "dependencies": [
                {
                  "version_id": null,
                  "project_id": "fabric-api",
                  "dependency_type": "required",
                  "file_name": null
                },
                {
                  "version_id": "exact-lib-version",
                  "project_id": "exact-lib",
                  "dependency_type": "required",
                  "file_name": null
                },
                {
                  "version_id": null,
                  "project_id": "optional-lib",
                  "dependency_type": "optional",
                  "file_name": null
                },
                {
                  "version_id": null,
                  "project_id": "conflict-lib",
                  "dependency_type": "incompatible",
                  "file_name": null
                }
              ],
              "files": [
                {
                  "hashes": { "sha1": "parent-hash" },
                  "url": "https://cdn.modrinth.com/data/create/create.jar",
                  "filename": "create.jar",
                  "primary": true,
                  "size": 123
                }
              ]
            }
            "#,
        )
    }

    fn dependency_version(
        project_id: &str,
        version_id: &str,
        published_at: &str,
        filename: &str,
    ) -> ModrinthVersion {
        version_from_json(&format!(
            r#"{{
              "id": "{version_id}",
              "project_id": "{project_id}",
              "version_number": "1.0.0",
              "name": "{project_id}",
              "game_versions": ["1.21.1"],
              "loaders": ["fabric"],
              "date_published": "{published_at}",
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
    }

    fn dependency_version_with_required_project_dependency(
        project_id: &str,
        version_id: &str,
        published_at: &str,
        filename: &str,
        dependency_project_id: &str,
    ) -> ModrinthVersion {
        version_from_json(&format!(
            r#"{{
              "id": "{version_id}",
              "project_id": "{project_id}",
              "version_number": "1.0.0",
              "name": "{project_id}",
              "game_versions": ["1.21.1"],
              "loaders": ["fabric"],
              "date_published": "{published_at}",
              "dependencies": [
                {{
                  "version_id": null,
                  "project_id": "{dependency_project_id}",
                  "dependency_type": "required",
                  "file_name": null
                }}
              ],
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
    }

    #[test]
    fn collects_only_required_dependencies() {
        let requests =
            collect_required_dependency_requests(&[parent_version_with_mixed_dependencies()])
                .expect("dependency requests should collect");

        assert_eq!(
            requests,
            vec![
                DependencyRequest {
                    parent_mod_id: "create".into(),
                    selector: DependencySelector::ProjectId {
                        project_id: "fabric-api".into(),
                    },
                },
                DependencyRequest {
                    parent_mod_id: "create".into(),
                    selector: DependencySelector::VersionId {
                        version_id: "exact-lib-version".into(),
                    },
                },
            ]
        );
    }

    #[test]
    fn resolves_exact_version_dependencies_via_version_lookup() {
        let requests = vec![DependencyRequest {
            parent_mod_id: "create".into(),
            selector: DependencySelector::VersionId {
                version_id: "geckolib-4-2".into(),
            },
        }];

        let resolution = resolve_dependency_requests(
            &requests,
            |_project_id| Ok(None),
            |version_id| {
                Ok((version_id == "geckolib-4-2").then(|| {
                    dependency_version(
                        "geckolib",
                        "geckolib-4-2",
                        "2024-08-01T10:00:00.000Z",
                        "geckolib-4.2.jar",
                    )
                }))
            },
        )
        .expect("dependency resolution should succeed");

        assert_eq!(resolution.resolved_dependencies.len(), 1);
        assert_eq!(
            resolution.resolved_dependencies[0].dependency_id,
            "geckolib"
        );
        assert_eq!(
            resolution.resolved_dependencies[0].version_id,
            "geckolib-4-2"
        );
        assert_eq!(
            resolution.links,
            vec![DependencyLink {
                parent_mod_id: "create".into(),
                dependency_id: "geckolib".into(),
                specific_version: Some("geckolib-4-2".into()),
                jar_filename: "geckolib-4.2.jar".into(),
            }]
        );
    }

    #[test]
    fn conflicting_exact_dependency_versions_exclude_their_parents() {
        let requests = vec![
            DependencyRequest {
                parent_mod_id: "mod-a".into(),
                selector: DependencySelector::VersionId {
                    version_id: "geckolib-4-0".into(),
                },
            },
            DependencyRequest {
                parent_mod_id: "mod-b".into(),
                selector: DependencySelector::VersionId {
                    version_id: "geckolib-4-2".into(),
                },
            },
        ];

        let versions = HashMap::from([
            (
                "geckolib-4-0",
                dependency_version(
                    "geckolib",
                    "geckolib-4-0",
                    "2024-05-01T10:00:00.000Z",
                    "geckolib-4.0.jar",
                ),
            ),
            (
                "geckolib-4-2",
                dependency_version(
                    "geckolib",
                    "geckolib-4-2",
                    "2024-08-01T10:00:00.000Z",
                    "geckolib-4.2.jar",
                ),
            ),
        ]);

        let resolution = resolve_dependency_requests(
            &requests,
            |_project_id| Ok(None),
            |version_id| Ok(versions.get(version_id).cloned()),
        )
        .expect("dependency resolution should succeed");

        assert!(resolution.resolved_dependencies.is_empty());
        assert!(resolution.links.is_empty());
        assert_eq!(
            resolution.excluded_parents,
            HashSet::from(["mod-a".to_string(), "mod-b".to_string()])
        );
    }

    #[test]
    fn exact_dependency_version_wins_over_newer_generic_candidate() {
        let requests = vec![
            DependencyRequest {
                parent_mod_id: "mod-a".into(),
                selector: DependencySelector::VersionId {
                    version_id: "sodium-1".into(),
                },
            },
            DependencyRequest {
                parent_mod_id: "mod-b".into(),
                selector: DependencySelector::ProjectId {
                    project_id: "sodium".into(),
                },
            },
        ];

        let exact = dependency_version(
            "sodium",
            "sodium-1",
            "2024-06-01T10:00:00.000Z",
            "sodium-1.jar",
        );
        let newer_generic = dependency_version(
            "sodium",
            "sodium-2",
            "2024-09-01T10:00:00.000Z",
            "sodium-2.jar",
        );

        let resolution = resolve_dependency_requests(
            &requests,
            |project_id| Ok((project_id == "sodium").then(|| newer_generic.clone())),
            |version_id| Ok((version_id == "sodium-1").then(|| exact.clone())),
        )
        .expect("dependency resolution should succeed");

        assert_eq!(resolution.resolved_dependencies.len(), 1);
        assert_eq!(resolution.resolved_dependencies[0].version_id, "sodium-1");
        assert_eq!(
            resolution.links,
            vec![
                DependencyLink {
                    parent_mod_id: "mod-a".into(),
                    dependency_id: "sodium".into(),
                    specific_version: Some("sodium-1".into()),
                    jar_filename: "sodium-1.jar".into(),
                },
                DependencyLink {
                    parent_mod_id: "mod-b".into(),
                    dependency_id: "sodium".into(),
                    specific_version: None,
                    jar_filename: "sodium-1.jar".into(),
                },
            ]
        );
        assert!(resolution.excluded_parents.is_empty());
    }

    #[test]
    fn project_id_dependencies_keep_null_specific_version() {
        let requests = vec![DependencyRequest {
            parent_mod_id: "create".into(),
            selector: DependencySelector::ProjectId {
                project_id: "fabric-api".into(),
            },
        }];

        let resolution = resolve_dependency_requests(
            &requests,
            |project_id| {
                Ok((project_id == "fabric-api").then(|| {
                    dependency_version(
                        "fabric-api",
                        "fabric-api-0.100.0",
                        "2024-08-15T10:00:00.000Z",
                        "fabric-api-0.100.0.jar",
                    )
                }))
            },
            |_version_id| Ok(None),
        )
        .expect("dependency resolution should succeed");

        assert_eq!(
            resolution,
            DependencyResolution {
                resolved_dependencies: vec![super::ResolvedDependency {
                    dependency_id: "fabric-api".into(),
                    version_id: "fabric-api-0.100.0".into(),
                    jar_filename: "fabric-api-0.100.0.jar".into(),
                    download_url: "https://cdn.modrinth.com/data/fabric-api/fabric-api-0.100.0.jar"
                        .into(),
                    file_hash: Some("fabric-api-0.100.0-sha1".into()),
                    date_published: "2024-08-15T10:00:00.000Z".into(),
                }],
                links: vec![DependencyLink {
                    parent_mod_id: "create".into(),
                    dependency_id: "fabric-api".into(),
                    specific_version: None,
                    jar_filename: "fabric-api-0.100.0.jar".into(),
                }],
                excluded_parents: HashSet::new(),
            }
        );
    }

    #[test]
    fn transitive_missing_dependency_excludes_top_level_parent() {
        let draggable_request = DependencyRequest {
            parent_mod_id: "draggable_lists".into(),
            selector: DependencySelector::ProjectId {
                project_id: "architectury".into(),
            },
        };
        let architectury_request = DependencyRequest {
            parent_mod_id: "architectury".into(),
            selector: DependencySelector::ProjectId {
                project_id: "fabric-api".into(),
            },
        };

        let draggable_candidate = build_dependency_candidate(
            &draggable_request,
            dependency_version_with_required_project_dependency(
                "architectury",
                "architectury-version",
                "2024-08-10T10:00:00.000Z",
                "architectury.jar",
                "fabric-api",
            ),
        )
        .expect("architectury candidate should build");
        let fabric_api_candidate = build_dependency_candidate(
            &architectury_request,
            dependency_version(
                "fabric-api",
                "fabric-api-version",
                "2024-08-11T10:00:00.000Z",
                "fabric-api.jar",
            ),
        )
        .expect("fabric-api candidate should build");

        let (selected, excluded) = finalize_dependency_candidates(
            &[draggable_candidate, fabric_api_candidate],
            HashSet::from(["architectury".to_string()]),
        );

        assert!(excluded.contains("draggable_lists"));
        assert!(!selected.contains_key("architectury"));
    }
}
