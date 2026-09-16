#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Seek};
use std::path::Path;

use anyhow::{Context, Result};

use crate::instance_mods::CachedModJar;
use crate::resolver::{ModLoader, ResolutionTarget};

use super::compare_fabric_dependency_versions;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct EmbeddedFabricRequirementSet {
    pub(super) minecraft_predicates: Vec<String>,
    pub(super) java_predicates: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct EmbeddedFabricRequirements {
    pub(super) root_entry: Option<EmbeddedFabricRequirementSet>,
    pub(super) entries: Vec<EmbeddedFabricRequirementSet>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct EmbeddedFabricModMetadata {
    pub(super) mod_id: String,
    pub(super) version: String,
    pub(super) provides: Vec<String>,
    pub(super) depends: HashMap<String, Vec<String>>,
    pub(super) breaks: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OwnedEmbeddedFabricModMetadata {
    pub(super) owner_project_id: String,
    pub(super) metadata: EmbeddedFabricModMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FabricValidationIssue {
    pub(super) reason_code: &'static str,
    pub(super) owner_project_id: String,
    pub(super) mod_id: String,
    pub(super) dependency_id: Option<String>,
    pub(super) detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FabricIssueRetryState {
    pub(super) signature: String,
    pub(super) consecutive_attempts: usize,
}

pub(super) fn fabric_issue_signature(issue: &FabricValidationIssue) -> String {
    format!(
        "{}|{}|{}|{}",
        issue.reason_code,
        issue.owner_project_id,
        issue.mod_id,
        issue.dependency_id.as_deref().unwrap_or("-"),
    )
}

pub(super) fn fabric_issue_reason_priority(reason_code: &str) -> u8 {
    match reason_code {
        "exact_dependency_conflict" => 0,
        "breaks_conflict" => 1,
        "incompatible_dependency_version" => 2,
        "missing_dependency" => 3,
        "embedded_version_incompatible" => 4,
        _ => 10,
    }
}

pub(super) fn choose_primary_fabric_issue<'a>(
    issues: &'a HashMap<String, FabricValidationIssue>,
) -> Option<(&'a String, &'a FabricValidationIssue)> {
    let mut entries = issues.iter().collect::<Vec<_>>();
    entries.sort_by(
        |(left_project_id, left_issue), (right_project_id, right_issue)| {
            fabric_issue_reason_priority(left_issue.reason_code)
                .cmp(&fabric_issue_reason_priority(right_issue.reason_code))
                .then_with(|| left_project_id.cmp(right_project_id))
        },
    );
    entries.into_iter().next()
}

pub(super) fn required_java_for_cached_mod_jars(jars: &[CachedModJar]) -> Result<u32> {
    let mut required_java = 0;

    for jar in jars {
        let requirements = read_embedded_fabric_requirements(&jar.cache_path)?;
        if let Some(min_java) = embedded_min_java_requirement(&requirements) {
            required_java = required_java.max(min_java);
        }
    }

    Ok(required_java)
}

pub(super) fn jar_metadata_allows_target(
    jar_path: &Path,
    target: &ResolutionTarget,
) -> Result<bool> {
    Ok(embedded_minecraft_requirements_match(
        &read_embedded_fabric_requirements(jar_path)?,
        target,
    ))
}

pub(super) fn read_embedded_fabric_requirements(
    jar_path: &Path,
) -> Result<EmbeddedFabricRequirements> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("failed to open JAR metadata from {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read JAR metadata from {}", jar_path.display()))?;

    read_embedded_fabric_requirements_from_archive(&mut archive, &jar_path.display().to_string())
}

pub(super) fn read_embedded_fabric_mod_metadata(
    jar_path: &Path,
) -> Result<Vec<EmbeddedFabricModMetadata>> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("failed to open JAR metadata from {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read JAR metadata from {}", jar_path.display()))?;

    read_embedded_fabric_mod_metadata_from_archive(&mut archive, &jar_path.display().to_string())
}

pub(super) fn read_root_fabric_mod_metadata(
    jar_path: &Path,
) -> Result<Option<EmbeddedFabricModMetadata>> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("failed to open JAR metadata from {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read JAR metadata from {}", jar_path.display()))?;
    let metadata =
        match read_embedded_fabric_metadata(&mut archive, &jar_path.display().to_string())? {
            Some(metadata) => metadata,
            None => return Ok(None),
        };

    Ok(fabric_mod_metadata_from_json(&metadata))
}

pub(super) fn read_root_fabric_provided_ids(jar_path: &Path) -> Result<HashSet<String>> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("failed to open JAR metadata from {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read JAR metadata from {}", jar_path.display()))?;
    let metadata =
        match read_embedded_fabric_metadata(&mut archive, &jar_path.display().to_string())? {
            Some(metadata) => metadata,
            None => return Ok(HashSet::new()),
        };

    Ok(fabric_mod_metadata_from_json(&metadata)
        .map(|entry| provided_ids_for_metadata(&entry))
        .unwrap_or_default())
}

pub(super) fn read_bundled_fabric_provided_ids(jar_path: &Path) -> Result<HashSet<String>> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("failed to open JAR metadata from {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read JAR metadata from {}", jar_path.display()))?;
    let metadata =
        match read_embedded_fabric_metadata(&mut archive, &jar_path.display().to_string())? {
            Some(metadata) => metadata,
            None => return Ok(HashSet::new()),
        };

    let mut provided_ids = HashSet::new();
    for nested_path in nested_fabric_jar_paths(&metadata) {
        let mut nested_bytes = Vec::new();
        let read_nested = {
            let mut nested_file = match archive.by_name(&nested_path) {
                Ok(file) => file,
                Err(_) => continue,
            };
            nested_file.read_to_end(&mut nested_bytes).is_ok()
        };
        if !read_nested {
            continue;
        }

        let mut nested_archive = match zip::ZipArchive::new(Cursor::new(nested_bytes)) {
            Ok(archive) => archive,
            Err(_) => continue,
        };
        let nested_entries = read_embedded_fabric_mod_metadata_from_archive(
            &mut nested_archive,
            &format!("{}!{nested_path}", jar_path.display()),
        )?;
        for entry in nested_entries {
            provided_ids.extend(provided_ids_for_metadata(&entry));
        }
    }

    Ok(provided_ids)
}

fn read_embedded_fabric_requirements_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    source: &str,
) -> Result<EmbeddedFabricRequirements> {
    let metadata = match read_embedded_fabric_metadata(archive, source)? {
        Some(metadata) => metadata,
        None => return Ok(EmbeddedFabricRequirements::default()),
    };

    let mut requirements = EmbeddedFabricRequirements::default();
    let root_requirements = requirement_set_from_metadata(&metadata);
    requirements.root_entry = Some(root_requirements.clone());
    requirements.entries.push(root_requirements);

    for nested_path in nested_fabric_jar_paths(&metadata) {
        let mut nested_bytes = Vec::new();
        let read_nested = {
            let mut nested_file = match archive.by_name(&nested_path) {
                Ok(file) => file,
                Err(_) => continue,
            };
            nested_file.read_to_end(&mut nested_bytes).is_ok()
        };
        if !read_nested {
            continue;
        }

        let mut nested_archive = match zip::ZipArchive::new(Cursor::new(nested_bytes)) {
            Ok(archive) => archive,
            Err(_) => continue,
        };
        let nested_requirements = read_embedded_fabric_requirements_from_archive(
            &mut nested_archive,
            &format!("{source}!{nested_path}"),
        )?;
        requirements.entries.extend(nested_requirements.entries);
    }

    Ok(requirements)
}

fn read_embedded_fabric_mod_metadata_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    source: &str,
) -> Result<Vec<EmbeddedFabricModMetadata>> {
    let metadata = match read_embedded_fabric_metadata(archive, source)? {
        Some(metadata) => metadata,
        None => return Ok(Vec::new()),
    };

    let mut entries = fabric_mod_metadata_from_json(&metadata)
        .into_iter()
        .collect::<Vec<_>>();

    for nested_path in nested_fabric_jar_paths(&metadata) {
        let mut nested_bytes = Vec::new();
        let read_nested = {
            let mut nested_file = match archive.by_name(&nested_path) {
                Ok(file) => file,
                Err(_) => continue,
            };
            nested_file.read_to_end(&mut nested_bytes).is_ok()
        };
        if !read_nested {
            continue;
        }

        let mut nested_archive = match zip::ZipArchive::new(Cursor::new(nested_bytes)) {
            Ok(archive) => archive,
            Err(_) => continue,
        };
        let nested_entries = read_embedded_fabric_mod_metadata_from_archive(
            &mut nested_archive,
            &format!("{source}!{nested_path}"),
        )?;
        entries.extend(nested_entries);
    }

    Ok(entries)
}

/// The root `fabric.mod.json` of an already-open archive, if it has one.
///
/// `pub(crate)` for [`crate::mod_icons`]: the icon of a local mod jar comes
/// from the **root** entry, so the nested-jar recursion in
/// [`read_embedded_fabric_mod_metadata_from_archive`] would answer with a
/// bundled mod's icon. Taking the archive by reference is what lets the caller
/// read `fabric.mod.json` and `META-INF/*.mods.toml` from one open handle.
pub(crate) fn read_embedded_fabric_metadata<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    source: &str,
) -> Result<Option<serde_json::Value>> {
    let mut metadata_file = match archive.by_name("fabric.mod.json") {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };

    let mut metadata = String::new();
    metadata_file
        .read_to_string(&mut metadata)
        .with_context(|| format!("failed to read fabric.mod.json from {source}"))?;

    Ok(serde_json::from_str(&metadata).ok())
}

fn fabric_mod_metadata_from_json(
    metadata: &serde_json::Value,
) -> Option<EmbeddedFabricModMetadata> {
    let mod_id = metadata
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let version = metadata
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("*")
        .to_string();

    let mut provides = metadata
        .get("provides")
        .map(json_predicates_to_strings)
        .unwrap_or_default();
    provides.retain(|value| !value.trim().is_empty() && value != &mod_id);
    provides.sort();
    provides.dedup();

    Some(EmbeddedFabricModMetadata {
        mod_id,
        version,
        provides,
        depends: fabric_dependency_map_from_json(metadata, "depends"),
        breaks: fabric_dependency_map_from_json(metadata, "breaks"),
    })
}

pub(super) fn provided_ids_for_metadata(metadata: &EmbeddedFabricModMetadata) -> HashSet<String> {
    let mut provided_ids = HashSet::new();
    if !metadata.mod_id.trim().is_empty() {
        provided_ids.insert(metadata.mod_id.clone());
    }
    provided_ids.extend(metadata.provides.iter().cloned());
    provided_ids
}

fn requirement_set_from_metadata(metadata: &serde_json::Value) -> EmbeddedFabricRequirementSet {
    let depends = metadata
        .get("depends")
        .and_then(serde_json::Value::as_object);

    EmbeddedFabricRequirementSet {
        minecraft_predicates: depends
            .and_then(|depends| depends.get("minecraft"))
            .map(json_predicates_to_strings)
            .unwrap_or_default(),
        java_predicates: depends
            .and_then(|depends| depends.get("java"))
            .map(json_predicates_to_strings)
            .unwrap_or_default(),
    }
}

fn fabric_dependency_map_from_json(
    metadata: &serde_json::Value,
    key: &str,
) -> HashMap<String, Vec<String>> {
    let mut dependency_map = metadata
        .get(key)
        .and_then(serde_json::Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(dependency_id, predicates)| {
                    let dependency_id = dependency_id.trim();
                    if dependency_id.is_empty() {
                        return None;
                    }

                    Some((
                        dependency_id.to_string(),
                        json_predicates_to_strings(predicates),
                    ))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    dependency_map.retain(|dependency_id, _| !dependency_id.trim().is_empty());
    dependency_map
}

fn nested_fabric_jar_paths(metadata: &serde_json::Value) -> Vec<String> {
    metadata
        .get("jars")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("file"))
        .filter_map(serde_json::Value::as_str)
        .map(ToString::to_string)
        .collect()
}

pub(super) fn embedded_minecraft_requirements_match(
    requirements: &EmbeddedFabricRequirements,
    target: &ResolutionTarget,
) -> bool {
    if target.mod_loader != ModLoader::Fabric {
        return true;
    }

    let Some(entry) = requirements.root_entry.as_ref() else {
        return true;
    };

    entry.minecraft_predicates.is_empty()
        || entry.minecraft_predicates.iter().any(|predicate| {
            minecraft_version_predicate_matches(predicate, &target.minecraft_version)
        })
}

pub(super) fn embedded_min_java_requirement(
    requirements: &EmbeddedFabricRequirements,
) -> Option<u32> {
    requirements
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .java_predicates
                .iter()
                .filter_map(|predicate| minimum_java_version_for_predicate(predicate))
                .min()
        })
        .max()
}

fn json_predicates_to_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(predicate) => vec![predicate.to_string()],
        serde_json::Value::Array(predicates) => predicates
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn minecraft_version_predicate_matches(predicate: &str, concrete: &str) -> bool {
    let predicate = predicate.trim();
    if predicate.is_empty() || predicate == "*" {
        return true;
    }

    predicate
        .split("||")
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .any(|branch| {
            branch
                .replace(',', " ")
                .split_whitespace()
                .all(|token| minecraft_version_token_matches(token, concrete))
        })
}

fn minecraft_version_token_matches(token: &str, concrete: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token == "*" {
        return true;
    }

    if let Some(expected) = token.strip_prefix('~') {
        return tilde_minecraft_version_matches(expected.trim(), concrete);
    }

    for (prefix, ordering) in [
        (">=", Some(std::cmp::Ordering::Greater)),
        ("<=", Some(std::cmp::Ordering::Less)),
        (">", Some(std::cmp::Ordering::Greater)),
        ("<", Some(std::cmp::Ordering::Less)),
        ("=", None),
    ] {
        if let Some(expected) = token.strip_prefix(prefix) {
            let Some(actual_ordering) = compare_minecraft_versions(concrete, expected.trim())
            else {
                return true;
            };
            return match (prefix, ordering) {
                (">=", Some(std::cmp::Ordering::Greater)) => {
                    actual_ordering == std::cmp::Ordering::Greater
                        || actual_ordering == std::cmp::Ordering::Equal
                }
                ("<=", Some(std::cmp::Ordering::Less)) => {
                    actual_ordering == std::cmp::Ordering::Less
                        || actual_ordering == std::cmp::Ordering::Equal
                }
                (">", Some(std::cmp::Ordering::Greater)) => {
                    actual_ordering == std::cmp::Ordering::Greater
                }
                ("<", Some(std::cmp::Ordering::Less)) => {
                    actual_ordering == std::cmp::Ordering::Less
                }
                ("=", None) => actual_ordering == std::cmp::Ordering::Equal,
                _ => true,
            };
        }
    }

    crate::modrinth::mc_version_matches(token, concrete)
        || compare_minecraft_versions(concrete, token)
            .is_some_and(|ordering| ordering == std::cmp::Ordering::Equal)
}

fn tilde_minecraft_version_matches(expected: &str, concrete: &str) -> bool {
    let Some(actual_ordering) = compare_minecraft_versions(concrete, expected) else {
        return true;
    };
    if actual_ordering == std::cmp::Ordering::Less {
        return false;
    }

    let Some(mut upper_parts) = parse_minecraft_version_parts(expected) else {
        return true;
    };
    if upper_parts.len() >= 2 {
        upper_parts[1] += 1;
        if upper_parts.len() > 2 {
            upper_parts[2..].fill(0);
        } else {
            upper_parts.push(0);
        }
    } else if let Some(last) = upper_parts.last_mut() {
        *last += 1;
    } else {
        return true;
    }

    let upper_bound = upper_parts
        .into_iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(".");

    compare_minecraft_versions(concrete, &upper_bound)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Less)
}

fn compare_minecraft_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let mut left_parts = parse_minecraft_version_parts(left)?;
    let mut right_parts = parse_minecraft_version_parts(right)?;
    let max_len = left_parts.len().max(right_parts.len());
    left_parts.resize(max_len, 0);
    right_parts.resize(max_len, 0);
    Some(left_parts.cmp(&right_parts))
}

fn parse_minecraft_version_parts(value: &str) -> Option<Vec<u64>> {
    value
        .split('.')
        .map(|segment| {
            let numeric = segment
                .trim()
                .split(|ch: char| !ch.is_ascii_digit())
                .next()
                .unwrap_or("");
            if numeric.is_empty() {
                None
            } else {
                numeric.parse::<u64>().ok()
            }
        })
        .collect()
}

pub(super) fn minimum_java_version_for_predicate(predicate: &str) -> Option<u32> {
    predicate
        .split("||")
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .filter_map(minimum_java_version_for_branch)
        .min()
}

pub(super) fn fabric_dependency_predicates_match(predicates: &[String], concrete: &str) -> bool {
    predicates.is_empty()
        || predicates
            .iter()
            .any(|predicate| fabric_dependency_predicate_matches(predicate, concrete))
}

fn fabric_dependency_predicate_matches(predicate: &str, concrete: &str) -> bool {
    let predicate = predicate.trim();
    if predicate.is_empty() || predicate == "*" {
        return true;
    }

    predicate
        .split("||")
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .any(|branch| {
            branch
                .replace(',', " ")
                .split_whitespace()
                .all(|token| fabric_dependency_token_matches(token, concrete))
        })
}

fn fabric_dependency_token_matches(token: &str, concrete: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token == "*" {
        return true;
    }

    for (prefix, ordering) in [
        (">=", Some(std::cmp::Ordering::Greater)),
        ("<=", Some(std::cmp::Ordering::Less)),
        (">", Some(std::cmp::Ordering::Greater)),
        ("<", Some(std::cmp::Ordering::Less)),
        ("=", None),
    ] {
        if let Some(expected) = token.strip_prefix(prefix) {
            let expected = expected.trim();
            let Some(actual_ordering) = compare_fabric_dependency_versions(concrete, expected)
            else {
                return concrete.eq_ignore_ascii_case(expected);
            };
            return match (prefix, ordering) {
                (">=", Some(std::cmp::Ordering::Greater)) => {
                    actual_ordering == std::cmp::Ordering::Greater
                        || actual_ordering == std::cmp::Ordering::Equal
                }
                ("<=", Some(std::cmp::Ordering::Less)) => {
                    actual_ordering == std::cmp::Ordering::Less
                        || actual_ordering == std::cmp::Ordering::Equal
                }
                (">", Some(std::cmp::Ordering::Greater)) => {
                    actual_ordering == std::cmp::Ordering::Greater
                }
                ("<", Some(std::cmp::Ordering::Less)) => {
                    actual_ordering == std::cmp::Ordering::Less
                }
                ("=", None) => actual_ordering == std::cmp::Ordering::Equal,
                _ => true,
            };
        }
    }

    compare_fabric_dependency_versions(concrete, token)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Equal)
        || concrete.eq_ignore_ascii_case(token)
}

fn minimum_java_version_for_branch(branch: &str) -> Option<u32> {
    let mut minimum = 0u32;

    for token in branch.replace(',', " ").split_whitespace() {
        let token = token.trim();
        if token.is_empty() || token == "*" {
            continue;
        }

        if let Some(value) = token.strip_prefix(">=") {
            minimum = minimum.max(value.trim().parse::<u32>().ok()?);
            continue;
        }
        if let Some(value) = token.strip_prefix('>') {
            minimum = minimum.max(value.trim().parse::<u32>().ok()?.saturating_add(1));
            continue;
        }
        if let Some(value) = token.strip_prefix('=') {
            minimum = minimum.max(value.trim().parse::<u32>().ok()?);
            continue;
        }
        if token.starts_with('<') {
            continue;
        }

        minimum = minimum.max(token.parse::<u32>().ok()?);
    }

    Some(minimum)
}
