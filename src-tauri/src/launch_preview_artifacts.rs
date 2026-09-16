#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;
use crate::mod_cache::{CacheProbe, ModCacheRecord};
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{
    resolve_modlist, version_rules_conflict, FailureReason, ModLoader, ResolutionResult,
    ResolutionTarget, RuleOutcome,
};
use crate::rules::{ModList, ModSource, Rule, RULES_FILENAME};

use super::{
    embedded_minecraft_requirements_match, emit_launcher_issue, emit_log,
    ensure_remote_version_cached, load_cached_file_hashes_for_selected,
    probe_cached_mod_for_target, probe_cached_version_for_target,
    read_embedded_fabric_requirements, SelectedMod,
};

pub(super) struct TopLevelVersionCandidates {
    selected_mod_id: String,
    project_id: String,
    candidates: Vec<ModrinthVersion>,
}

#[derive(Debug, Clone)]
pub(super) enum RemoteArtifact {
    /// A version the cache does not hold, with the metadata the download stage
    /// needs. In cache-only mode this only happens for a version a pre-check
    /// named: an update just accepted is not in the cache by definition.
    Live(ModrinthVersion),
    /// A cached version whose jar is on disk.
    Cached(ModCacheRecord),
    /// A registered version whose jar is gone from the cache directory. The
    /// row carries the url and the hash, so the launch restores exactly that
    /// version without asking Modrinth anything.
    MissingJar(ModCacheRecord),
}
/// Pick the preferred candidate by channel first, then by `date_published`
/// (RFC3339 UTC, so lexicographic comparison is chronological). Candidates are
/// already filtered to the exact/wildcard-compatible set by
/// `fetch_project_versions`.
fn select_preferred_version(versions: Vec<ModrinthVersion>) -> Option<ModrinthVersion> {
    versions.into_iter().max_by(|left, right| {
        left.channel_rank()
            .cmp(&right.channel_rank())
            .then_with(|| left.date_published.cmp(&right.date_published))
    })
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
                if let Some(version) = select_preferred_version(candidate_versions) {
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

/// Selected Modrinth mods split by whether `mod_cache` knows a sha1 for them on
/// this target. One entry per mod id, in resolution order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CachedHashSplit {
    /// `(mod_id, lowercased sha1)` — the bulk path.
    pub(super) hashed: Vec<(String, String)>,
    /// No row for this target, or a row whose `file_hash` is NULL — the
    /// per-project path.
    pub(super) unhashed: Vec<String>,
}

/// What the bulk stage answered, and what it left to the per-project stage.
pub(super) struct HashStageOutcome {
    pub(super) resolved: HashMap<String, ModrinthVersion>,
    pub(super) per_project_mod_ids: Vec<String>,
    /// Mod ids that had a cached hash the bulk response omitted, and are
    /// therefore retried per project (D22). Subset of `per_project_mod_ids`.
    pub(super) omitted_mod_ids: Vec<String>,
    /// Set when the bulk call itself failed and its whole set fell back.
    pub(super) bulk_error: Option<String>,
}

pub(super) fn split_selected_by_cached_hash(
    selected_mods: &[SelectedMod],
    hash_by_mod_id: &HashMap<String, String>,
) -> CachedHashSplit {
    let mut split = CachedHashSplit::default();
    let mut seen = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) {
            continue;
        }
        if !seen.insert(selected.mod_id.as_str()) {
            continue;
        }

        match hash_by_mod_id.get(&selected.mod_id) {
            Some(hash) => split.hashed.push((selected.mod_id.clone(), hash.clone())),
            None => split.unhashed.push(selected.mod_id.clone()),
        }
    }

    split
}

/// Re-key a keyed Modrinth response by mod id, dropping the mod ids the
/// response left out.
///
/// Used with two kinds of key. A **sha1** from the bulk endpoint: an omitted
/// hash yields no entry and the caller retries that mod per project instead of
/// declaring it unavailable, because the response cannot distinguish "no
/// version for this target" from "this hash is no longer known". A **version
/// id** handed down by the pre-check: an omission there means Modrinth dropped
/// the exact version the popup showed. Two mod ids sharing one key both
/// resolve either way.
pub(super) fn versions_by_mod_id(
    keyed: &[(String, String)],
    versions_by_key: &HashMap<String, ModrinthVersion>,
) -> HashMap<String, ModrinthVersion> {
    keyed
        .iter()
        .filter_map(|(mod_id, key)| {
            versions_by_key
                .get(key)
                .map(|version| (mod_id.clone(), version.clone()))
        })
        .collect()
}

/// Bulk results win; the per-project stage only fills mod ids the bulk stage
/// left out. The two key sets are disjoint in the normal path and overlap only
/// after a failed bulk call sent its whole set to the fallback.
pub(super) fn merge_resolved_versions(
    mut bulk: HashMap<String, ModrinthVersion>,
    per_project: HashMap<String, ModrinthVersion>,
) -> HashMap<String, ModrinthVersion> {
    for (mod_id, version) in per_project {
        bulk.entry(mod_id).or_insert(version);
    }
    bulk
}

/// The bulk stage: one cascade for every mod that has a cached hash.
///
/// Generic over the request so both fallbacks are testable without network.
///
/// Two things go back to the per-project path. An **omitted hash** (D22): the
/// endpoint answers `200` with the hash simply absent, and that covers both "no
/// version exists for this target" — where the per-project path finds nothing
/// either, so the retry costs one wasted request — and "Modrinth no longer
/// knows this hash", where the per-project path still finds the version and the
/// mod keeps working exactly as it does today. A **failed call**: a per-project
/// error costs one mod today (`prefetch_compatible_versions_for_selected`),
/// while a bulk error would cost the whole modlist, so the set falls back
/// instead of shrinking the launch. Both are slow paths, and both are rare.
pub(super) async fn resolve_versions_for_cached_hashes<Fetch, Fut>(
    split: &CachedHashSplit,
    fetch_bulk: Fetch,
) -> HashStageOutcome
where
    Fetch: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<HashMap<String, ModrinthVersion>>>,
{
    if split.hashed.is_empty() {
        return HashStageOutcome {
            resolved: HashMap::new(),
            per_project_mod_ids: split.unhashed.clone(),
            omitted_mod_ids: Vec::new(),
            bulk_error: None,
        };
    }

    let hashes = split
        .hashed
        .iter()
        .map(|(_, hash)| hash.clone())
        .collect::<Vec<_>>();

    match fetch_bulk(hashes).await {
        Ok(versions_by_hash) => {
            let resolved = versions_by_mod_id(&split.hashed, &versions_by_hash);
            let omitted_mod_ids = split
                .hashed
                .iter()
                .map(|(mod_id, _)| mod_id)
                .filter(|mod_id| !resolved.contains_key(mod_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();

            let mut per_project_mod_ids = omitted_mod_ids.clone();
            per_project_mod_ids.extend(split.unhashed.iter().cloned());

            HashStageOutcome {
                resolved,
                per_project_mod_ids,
                omitted_mod_ids,
                bulk_error: None,
            }
        }
        Err(error) => {
            let mut per_project_mod_ids = split
                .hashed
                .iter()
                .map(|(mod_id, _)| mod_id.clone())
                .collect::<Vec<_>>();
            per_project_mod_ids.extend(split.unhashed.iter().cloned());

            HashStageOutcome {
                resolved: HashMap::new(),
                per_project_mod_ids,
                omitted_mod_ids: Vec::new(),
                bulk_error: Some(format!("{error:#}")),
            }
        }
    }
}

/// Same contract as `prefetch_compatible_versions_for_selected` — mod id →
/// preferred `ModrinthVersion` — reached through `mod_cache` + the bulk
/// endpoint, falling back to one request per mod only for the mods no cached
/// hash covers.
pub(super) async fn resolve_compatible_versions_hybrid(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    selected_mods: &[SelectedMod],
    client: &ModrinthClient,
    target: &ResolutionTarget,
) -> Result<HashMap<String, ModrinthVersion>> {
    let hash_by_mod_id =
        load_cached_file_hashes_for_selected(launcher_paths, selected_mods, target)?;
    let split = split_selected_by_cached_hash(selected_mods, &hash_by_mod_id);
    let outcome = resolve_versions_for_cached_hashes(&split, |hashes| async move {
        client.fetch_latest_versions_by_hash(&hashes, target).await
    })
    .await;

    if let Some(error) = &outcome.bulk_error {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Launch] bulk version lookup failed for {} cached hashes; falling back to one request per mod ({error})",
                split.hashed.len()
            ),
        );
    } else if !outcome.omitted_mod_ids.is_empty() {
        // The endpoint reports neither "unknown hash" nor "no version for this
        // target": both are omissions. Naming them is the only diagnostic
        // available for a case that should not happen (D23).
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Launch] bulk version lookup omitted {}; retrying one request per mod",
                outcome.omitted_mod_ids.join(", ")
            ),
        );
    }

    let per_project_mod_ids = outcome
        .per_project_mod_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let per_project_selection = selected_mods
        .iter()
        .filter(|selected| per_project_mod_ids.contains(selected.mod_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let per_project = prefetch_compatible_versions_for_selected(
        app_handle,
        launcher_paths,
        http_client,
        &per_project_selection,
        client,
        target,
    )
    .await?;

    Ok(merge_resolved_versions(outcome.resolved, per_project))
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
        RemoteArtifact::Cached(record) | RemoteArtifact::MissingJar(record) => {
            &record.modrinth_project_id
        }
    }
}

/// The three artifact kinds as the three lists the download stage and the
/// instance-linking stage consume, deduplicated by version id.
#[derive(Debug, Default)]
pub(super) struct RemoteArtifactSplit {
    pub(super) live_versions: Vec<ModrinthVersion>,
    pub(super) cached_records: Vec<ModCacheRecord>,
    pub(super) missing_jar_records: Vec<ModCacheRecord>,
}

pub(super) fn split_remote_artifacts(artifacts: &[RemoteArtifact]) -> RemoteArtifactSplit {
    let mut split = RemoteArtifactSplit::default();
    let mut seen_version_ids = HashSet::new();

    for artifact in artifacts {
        match artifact {
            RemoteArtifact::Live(version) => {
                if seen_version_ids.insert(version.id.clone()) {
                    split.live_versions.push(version.clone());
                }
            }
            RemoteArtifact::Cached(record) => {
                if seen_version_ids.insert(record.modrinth_version_id.clone()) {
                    split.cached_records.push(record.clone());
                }
            }
            RemoteArtifact::MissingJar(record) => {
                if seen_version_ids.insert(record.modrinth_version_id.clone()) {
                    split.missing_jar_records.push(record.clone());
                }
            }
        }
    }

    split
}

/// How the launch acquires a version a pre-check named, given what the cache
/// holds for that exact version id.
///
/// Pure, because it is the decision the whole of A3a is about: with a map, the
/// cache stops being the thing that chooses and becomes one of two places the
/// chosen version can come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NamedVersionPlan {
    /// In the cache with its jar: install it from there.
    UseCached(ModCacheRecord),
    /// Registered but its jar is gone: restore that version from the row.
    RestoreRegistered(ModCacheRecord),
    /// Not in the cache: ask Modrinth for its metadata. This is the accepted
    /// update — by definition it cannot be cached yet.
    FetchMetadata,
    /// The row for this id is a locally copied jar, whose version id is
    /// synthetic: sending it to `GET /versions?ids=` would `400` the whole
    /// request, so the mod falls back to the project lookup, i.e. to today's
    /// behaviour.
    ProjectLookup,
}

pub(super) fn plan_for_named_version(probe: CacheProbe) -> NamedVersionPlan {
    match probe {
        CacheProbe::Ready(record) => NamedVersionPlan::UseCached(record),
        CacheProbe::JarMissing(record) => NamedVersionPlan::RestoreRegistered(record),
        CacheProbe::JarMissingUnrecoverable(record) if record.is_local => {
            NamedVersionPlan::ProjectLookup
        }
        // A remote row without a url is exactly the case Modrinth can answer:
        // the id is real, only the row is incomplete.
        CacheProbe::JarMissingUnrecoverable(_) | CacheProbe::NotCached => {
            NamedVersionPlan::FetchMetadata
        }
    }
}

/// One cache probe per distinct selected Modrinth mod, in resolution order.
///
/// No network and no logging: both cache-only passes over the same mod list
/// want the same answer, and only the second one — the one that runs on the
/// mods the re-resolution actually kept — reports it.
pub(super) fn probe_selected_mods(
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    target: &ResolutionTarget,
) -> Result<Vec<(String, CacheProbe)>> {
    let mut probes = Vec::new();
    let mut seen = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) || !seen.insert(&selected.mod_id) {
            continue;
        }

        probes.push((
            selected.mod_id.clone(),
            probe_cached_mod_for_target(launcher_paths, &selected.mod_id, target)?,
        ));
    }

    Ok(probes)
}

/// What "the cache can serve this mod" is allowed to mean, per path.
///
/// The two paths differ for a measured reason, not for taste. With the jar
/// gone from the cache directory, restoring it means downloading from
/// `cdn.modrinth.com`, and a failed download **aborts the whole launch**
/// (`download_pending_artifacts` propagates, `run_launch_pipeline` returns).
/// On the resolving path the only situation where the restore matters is one
/// where Modrinth already failed to answer, so the restore is likely to fail
/// too: measured, that turned "one mod missing" into "the game does not
/// start". The path that has a version map is the one that may restore — there
/// the version is named, the restore is the point (A3b), and the launch has
/// something to promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheBacking {
    /// The jar is on disk: nothing has to be fetched for this mod.
    JarOnDisk,
    /// A registered row is enough; the launch restores the jar from it.
    RestorableFromRow,
}

/// The selected mods this machine can install with no help from Modrinth.
///
/// D29: a row for **this exact target** — the probe keys on
/// `(project, mc_version, mod_loader)`, never on the project alone — makes the
/// mod available whatever Modrinth says. A project cached for another game
/// version is not cached for this one.
pub(super) fn cache_backed_mods(
    probes: &[(String, CacheProbe)],
    backing: CacheBacking,
) -> HashSet<String> {
    probes
        .iter()
        .filter(|(_, probe)| match probe {
            CacheProbe::Ready(_) => true,
            CacheProbe::JarMissing(_) => backing == CacheBacking::RestorableFromRow,
            CacheProbe::JarMissingUnrecoverable(_) | CacheProbe::NotCached => false,
        })
        .map(|(mod_id, _)| mod_id.clone())
        .collect()
}

/// The cache's answer for the selected mods the resolving path did not resolve.
///
/// The availability pass keeps a mod whose jar the cache holds even when no
/// version came back for it. Such a mod is absent from the version map, so
/// without this nothing downstream installs it: it would stay in the
/// resolution and out of the game, which is the failure this repository keeps
/// paying for.
///
/// Only jars that are **on disk** become artifacts here, matching
/// `CacheBacking::JarOnDisk`: this path installs what it already has and
/// fetches nothing, so it cannot make the launch depend on a host that just
/// went silent.
pub(super) fn cached_artifacts_for_unresolved(
    probes: &[(String, CacheProbe)],
    resolved: &HashMap<String, ModrinthVersion>,
) -> Vec<(String, RemoteArtifact)> {
    probes
        .iter()
        .filter(|(mod_id, _)| !resolved.contains_key(mod_id))
        .filter_map(|(mod_id, probe)| match probe {
            CacheProbe::Ready(record) => {
                Some((mod_id.clone(), RemoteArtifact::Cached(record.clone())))
            }
            CacheProbe::JarMissing(_)
            | CacheProbe::JarMissingUnrecoverable(_)
            | CacheProbe::NotCached => None,
        })
        .collect()
}

/// The cache fallback of a resolving launch, named in the log mod by mod.
///
/// No network: the probe reads the database and the disk. The log matters as
/// much as the lists — a mod installed from the cache because Modrinth had
/// nothing to say about it is exactly what the launch has to admit out loud.
pub(super) fn resolve_cache_fallback_artifacts(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    resolved: &HashMap<String, ModrinthVersion>,
    target: &ResolutionTarget,
) -> Result<RemoteArtifactSplit> {
    let probes = probe_selected_mods(launcher_paths, selected_mods, target)?;
    let fallback = cached_artifacts_for_unresolved(&probes, resolved);

    for (mod_id, artifact) in &fallback {
        // "No version came back" covers both a Modrinth that has none for this
        // target and a lookup that failed: the resolving pass reports the two
        // the same way, so the log does not claim to tell them apart.
        if let RemoteArtifact::Cached(record) = artifact {
            let _ = emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Cache] no version came back from Modrinth for '{}' on this target; installing the cached jar {} (version {})",
                    mod_id, record.jar_filename, record.modrinth_version_id
                ),
            );
        }
    }

    // One notice for the whole fallback, not one per mod: with Modrinth
    // unreachable this is the entire mod-list, and twenty-five banners say
    // less than one.
    if !fallback.is_empty() {
        let _ = emit_launcher_issue(
            app_handle,
            "Launching from your cached versions",
            &format!(
                "{} mod(s) got no version from Modrinth for this target and are installed from the jars you already have.",
                fallback.len()
            ),
            "Modrinth either has no compatible version for them or did not answer. The launch continues with the cached versions.",
            "warning",
            "launch",
        );
    }

    for (mod_id, probe) in &probes {
        if resolved.contains_key(mod_id) {
            continue;
        }
        match probe {
            // Nothing to restore it from here: the row's url points at the
            // host that just failed to answer, and one failed download ends
            // the launch. The mod is skipped, and said.
            CacheProbe::JarMissing(record) => {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stderr,
                    format!(
                        "[Cache] the cached jar of '{}' is gone ({}) and no version came back from Modrinth to fetch it with; the mod is skipped",
                        mod_id, record.jar_filename
                    ),
                );
                let _ = emit_launcher_issue(
                    app_handle,
                    "Mod left out of the launch",
                    &format!(
                        "The jar of '{mod_id}' is gone from the cache and Modrinth gave no version to fetch it with."
                    ),
                    &format!(
                        "{} (version {}) would have to be downloaded again, and this launch got no answer for it.",
                        record.jar_filename, record.modrinth_version_id
                    ),
                    "warning",
                    "launch",
                );
            }
            _ => report_unusable_probe(app_handle, mod_id, probe),
        }
    }

    let artifacts = fallback
        .into_iter()
        .map(|(_, artifact)| artifact)
        .collect::<Vec<_>>();

    Ok(split_remote_artifacts(&artifacts))
}

/// A registered version whose jar left the cache directory, said twice: in the
/// launch log and on the banner (D28). The launch keeps working — the row
/// carries the url and the hash — but the user is the only one who knows
/// whether a jar disappearing is normal on their machine.
fn report_restored_jar(app_handle: &tauri::AppHandle, mod_id: &str, record: &ModCacheRecord) {
    let _ = emit_log(
        app_handle,
        ProcessLogStream::Stderr,
        format!(
            "[Cache] the cached jar of '{}' is gone ({}); re-downloading the registered version {} instead of the newest one",
            mod_id, record.jar_filename, record.modrinth_version_id
        ),
    );
    let _ = emit_launcher_issue(
        app_handle,
        "Cached jar restored",
        &format!(
            "The jar of '{mod_id}' had left the cache directory; the launch re-downloaded the version you had."
        ),
        &format!(
            "{} (version {}) was missing from the cache directory and was restored from its cache row, not replaced by the newest version.",
            record.jar_filename, record.modrinth_version_id
        ),
        "warning",
        "launch",
    );
}

/// A mod the cache cannot serve and nothing can restore, with the reason.
///
/// Both selection passes treat it as unavailable — today's behaviour — and
/// this is the one case where the mod is **not** in the game: the log names it
/// and the banner says so, because a mod missing in silence is the worst
/// failure this pipeline has.
fn report_unusable_probe(app_handle: &tauri::AppHandle, mod_id: &str, probe: &CacheProbe) {
    let (message, detail) = match probe {
        CacheProbe::JarMissingUnrecoverable(record) if record.is_local => (
            format!(
                "[Cache] the cached jar of local mod '{}' is gone ({}); a locally copied jar has no Modrinth download url, so it cannot be restored and the mod is skipped",
                mod_id, record.jar_filename
            ),
            format!(
                "{} was copied in by hand, so there is no download url to restore it from. Add the jar again to get the mod back.",
                record.jar_filename
            ),
        ),
        CacheProbe::JarMissingUnrecoverable(record) => (
            format!(
                "[Cache] the cached jar of '{}' is gone ({}, version {}) and its cache row has no download url, so it cannot be restored and the mod is skipped",
                mod_id, record.jar_filename, record.modrinth_version_id
            ),
            format!(
                "{} (version {}) is missing and its cache row has no download url.",
                record.jar_filename, record.modrinth_version_id
            ),
        ),
        CacheProbe::NotCached => (
            format!(
                "[Cache] '{mod_id}' has never been cached for this target and this launch has no chosen version to fetch it with; the mod is skipped"
            ),
            "The mod has never been downloaded for this Minecraft version and loader, and this launch had no chosen version to fetch it with.".to_string(),
        ),
        CacheProbe::Ready(_) | CacheProbe::JarMissing(_) => return,
    };

    let _ = emit_log(app_handle, ProcessLogStream::Stderr, message);
    let _ = emit_launcher_issue(
        app_handle,
        "Mod left out of the launch",
        &format!("'{mod_id}' is not in this launch: its jar is not in the cache and cannot be fetched."),
        &detail,
        "warning",
        "launch",
    );
}

/// What the launch installs for the selected mods when it may not choose
/// versions itself.
///
/// With a version map the cache stops deciding: every covered mod is acquired
/// at **its** version id, from the cache when the row matches and the jar is
/// there, from `GET /versions?ids=` plus the normal download stage when it is
/// not (D16, D27). Without a map, or for a mod the map does not cover, the
/// project lookup answers exactly as it always did.
///
/// A registered row whose jar disappeared is its own case in both paths: it is
/// restored at the version the row names, from the row's own url and hash, so
/// "launch from your cached versions" cannot drift into "launch from the
/// newest".
pub(super) async fn resolve_selected_remote_artifacts(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    selected_mods: &[SelectedMod],
    client: &ModrinthClient,
    target: &ResolutionTarget,
    preresolved: Option<&HashMap<String, String>>,
) -> Result<HashMap<String, RemoteArtifact>> {
    // An absent map behaves as a map that covers nothing, which is what makes
    // "no map means exactly today's behaviour" structural instead of asserted.
    let no_map = HashMap::new();
    let (covered, uncovered) =
        split_selected_by_preresolved(selected_mods, preresolved.unwrap_or(&no_map));

    let mut artifacts = HashMap::new();
    let mut to_fetch: Vec<(String, String)> = Vec::new();
    let mut by_project: Vec<String> = uncovered;

    for (mod_id, version_id) in &covered {
        match plan_for_named_version(probe_cached_version_for_target(
            launcher_paths,
            version_id,
            target,
        )?) {
            NamedVersionPlan::UseCached(record) => {
                artifacts.insert(mod_id.clone(), RemoteArtifact::Cached(record));
            }
            NamedVersionPlan::RestoreRegistered(record) => {
                report_restored_jar(app_handle, mod_id, &record);
                artifacts.insert(mod_id.clone(), RemoteArtifact::MissingJar(record));
            }
            NamedVersionPlan::FetchMetadata => {
                to_fetch.push((mod_id.clone(), version_id.clone()));
            }
            NamedVersionPlan::ProjectLookup => {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stderr,
                    format!(
                        "[Cache] the version chosen for '{mod_id}' ({version_id}) belongs to a locally copied jar; falling back to the cached artifact for that project"
                    ),
                );
                by_project.push(mod_id.clone());
            }
        }
    }

    if !to_fetch.is_empty() {
        let version_ids = to_fetch
            .iter()
            .map(|(_, version_id)| version_id.clone())
            .collect::<Vec<_>>();
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Cache] {} chosen version(s) are not in the cache ({}); fetching their metadata to download them",
                to_fetch.len(),
                to_fetch
                    .iter()
                    .map(|(mod_id, _)| mod_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );

        // A failed call must not cost the launch. This branch exists so a
        // cache-only launch keeps working without network (D19): the mods whose
        // chosen version could not be looked up fall back to the cached
        // artifact, which is what a launch with no map does anyway.
        match client.fetch_versions_by_ids(&version_ids).await {
            Ok(by_version_id) => {
                let fetched = versions_by_mod_id(&to_fetch, &by_version_id);

                let dropped = to_fetch
                    .iter()
                    .map(|(mod_id, _)| mod_id)
                    .filter(|mod_id| !fetched.contains_key(mod_id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !dropped.is_empty() {
                    let _ = emit_log(
                        app_handle,
                        ProcessLogStream::Stderr,
                        format!(
                            "[Cache] Modrinth no longer returns the chosen version of {}; falling back to the cached artifact for those projects",
                            dropped.join(", ")
                        ),
                    );
                    by_project.extend(dropped);
                }

                for (mod_id, version) in fetched {
                    artifacts.insert(mod_id, RemoteArtifact::Live(version));
                }
            }
            Err(error) => {
                let _ = emit_log(
                    app_handle,
                    ProcessLogStream::Stderr,
                    format!(
                        "[Cache] the metadata of the chosen versions could not be fetched ({error:#}); launching from the cached versions instead"
                    ),
                );
                by_project.extend(to_fetch.iter().map(|(mod_id, _)| mod_id.clone()));
            }
        }
    }

    for mod_id in &by_project {
        if artifacts.contains_key(mod_id) {
            continue;
        }

        let probe = probe_cached_mod_for_target(launcher_paths, mod_id, target)?;
        match probe {
            CacheProbe::Ready(record) => {
                artifacts.insert(mod_id.clone(), RemoteArtifact::Cached(record));
            }
            CacheProbe::JarMissing(record) => {
                report_restored_jar(app_handle, mod_id, &record);
                artifacts.insert(mod_id.clone(), RemoteArtifact::MissingJar(record));
            }
            other => report_unusable_probe(app_handle, mod_id, &other),
        }
    }

    Ok(artifacts)
}

/// What a target actually loads, once the availability pass has had its say —
/// and what it stopped loading with nothing in its place, which no later stage
/// can still see (A5).
#[derive(Debug, Clone)]
pub(super) struct TargetSelection {
    pub(super) resolution: ResolutionResult,
    pub(super) selected_mods: Vec<SelectedMod>,
    pub(super) dropped: Vec<DroppedMod>,
}

/// The Modrinth mods resolution selected and for which no artifact is
/// available. `is_available` is the only difference between the online and the
/// cache-only reading of "available", so the rest is shared.
pub(super) fn unavailable_selected_mods<Available>(
    selected_mods: &[SelectedMod],
    is_available: Available,
) -> Vec<String>
where
    Available: Fn(&str) -> bool,
{
    selected_mods
        .iter()
        .filter(|selected| matches!(selected.source, ModSource::Modrinth))
        .filter(|selected| !is_available(&selected.mod_id))
        .map(|selected| selected.mod_id.clone())
        .collect()
}

/// Temporarily disable the mods with no available artifact and re-resolve, so
/// alternatives get a chance. Nothing unavailable means the first resolution
/// stands and no second resolution happens — the normal case.
pub(super) fn reresolve_without_unavailable(
    modlist: &ModList,
    resolution: ResolutionResult,
    unavailable: &[String],
    target: &ResolutionTarget,
) -> Result<ResolutionResult> {
    if unavailable.is_empty() {
        return Ok(resolution);
    }

    let mut patched = modlist.clone();
    for mod_id in unavailable {
        if let Some(rule) = patched.find_rule_mut(mod_id) {
            rule.enabled = false;
        }
    }

    resolve_modlist(&patched, target)
}

/// Why a mod is not in the launch, in the only three shapes the availability
/// pass can produce.
///
/// The first two are the pass itself, and they are two values and not one
/// because "available" means a different thing on each path
/// (`CacheBacking`): a notice that names the wrong reason is worse than a
/// reason that is generic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DropReason {
    /// The resolving path: no version came back from Modrinth for this target
    /// and no jar for it is on disk.
    NoVersionAndNoCachedJar,
    /// The pre-checked path: no version was chosen for it before the launch
    /// and the cache cannot produce a jar either.
    NoChosenVersionAndNothingCached,
    /// Collateral: the mod itself was installable, but a mod it `requires`
    /// was not, so the re-resolution left it out too
    /// (`FailureReason::RequiredModMissing`). Leaving this one silent would
    /// reopen the hole one rule further down.
    RequiredModLeftOut,
}

/// One mod the launch lost, with the reason to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DroppedMod {
    pub(super) mod_id: String,
    pub(super) reason: DropReason,
}

/// The mods the availability pass took out of the launch with nothing in
/// their place. Empty in the normal case.
///
/// Reporting the `unavailable` list as it comes would be wrong in both
/// directions, and a banner that cries wolf teaches people to ignore banners —
/// the failure the dependency notices already cost this repository. So the
/// decision is a **diff of the two resolutions, slot by slot**: a top-level
/// rule that resolved before the pass and no longer resolves after it is a
/// mod that left the game, whatever the reason the resolver files it under.
///
/// What that buys, case by case:
///
/// - **an alternative took over**: the slot is still `Resolved`, to another
///   id. The group did its job, and saying "X was left out" while Y is loaded
///   is false. No notice.
/// - **the rules excluded it** (`exclude_if`, `requires`, `version_rules`):
///   that is `resolve_modlist`'s decision, taken before this pass runs, so the
///   slot is already `Unresolved` in `before` and the diff never sees it.
///   Nothing to filter.
/// - **a real loss**: `Resolved` before, not `Resolved` after, and the id is
///   nowhere in the final active set. Said out loud.
/// - **a mod a dropped dependency took with it**: `requires` points at a mod
///   that just went unavailable, so this one ends `Unresolved` too
///   (`resolver.rs:231-233`) — and it is *not* in `unavailable`, which is why
///   iterating that list would let a genuine silent loss through. The diff
///   catches it and `RequiredModLeftOut` says the truth about it.
///
/// The same diff is also the only honest source for the reason: an
/// availability-disabled rule takes the `!rule.enabled` branch and comes back
/// as `ExcludedByActiveMod` (`resolver.rs:210-218`), indistinguishable from a
/// real `exclude_if`, so `FailureReason` cannot be trusted to carry the why.
///
/// Comparing by slot index is sound because both resolutions ran over the same
/// top-level rule vector: `reresolve_without_unavailable` clones the mod-list
/// and only flips `enabled`, and `resolve_modlist` pushes one `resolved_rules`
/// entry per top-level rule, in order.
pub(super) fn mods_dropped_without_replacement(
    before: &ResolutionResult,
    after: &ResolutionResult,
    unavailable: &[String],
    unavailable_reason: DropReason,
) -> Vec<DroppedMod> {
    let mut dropped: Vec<DroppedMod> = Vec::new();

    for (index, rule) in before.resolved_rules.iter().enumerate() {
        let RuleOutcome::Resolved { resolved_id } = &rule.outcome else {
            continue;
        };
        if matches!(
            after.resolved_rules.get(index).map(|rule| &rule.outcome),
            Some(RuleOutcome::Resolved { .. })
        ) {
            continue;
        }
        // Still loaded because another rule of the tree resolved to it: there
        // is nothing missing to report.
        if after.active_mods.contains(resolved_id.as_str()) {
            continue;
        }
        if dropped.iter().any(|lost| &lost.mod_id == resolved_id) {
            continue;
        }

        dropped.push(DroppedMod {
            mod_id: resolved_id.clone(),
            reason: if unavailable.iter().any(|mod_id| mod_id == resolved_id) {
                unavailable_reason
            } else {
                DropReason::RequiredModLeftOut
            },
        });
    }

    dropped
}

/// The reason in the words of someone reading it, singular or plural.
///
/// "no version for this Minecraft version" is useful; "unresolved" is the
/// resolver talking to itself.
fn dropped_reason_clause(reason: DropReason, target: &ResolutionTarget, many: bool) -> String {
    let pronoun = if many { "them" } else { "it" };

    match reason {
        DropReason::NoVersionAndNoCachedJar => format!(
            "there is no version of {pronoun} for Minecraft {} / {} and no jar for {pronoun} in your cache",
            target.minecraft_version,
            target.mod_loader.as_modrinth_loader()
        ),
        DropReason::NoChosenVersionAndNothingCached => format!(
            "this launch had no version chosen for {pronoun} and your cache has no jar for Minecraft {} / {}",
            target.minecraft_version,
            target.mod_loader.as_modrinth_loader()
        ),
        DropReason::RequiredModLeftOut => format!(
            "a mod {pronoun} need{} is not in this launch either",
            if many { "" } else { "s" }
        ),
    }
}

/// The long half of the notice: the cause, then what it costs.
fn dropped_reason_detail(reason: DropReason, many: bool) -> String {
    let (it, its) = if many { ("them", "their") } else { ("it", "its") };

    let cause = match reason {
        DropReason::NoVersionAndNoCachedJar => {
            "Modrinth returned no compatible version for this Minecraft version and loader — there may be none, or it may not have answered — and the cache has no jar to install instead.".to_string()
        }
        DropReason::NoChosenVersionAndNothingCached => format!(
            "The versions chosen before this launch do not cover {it}, and the cache has no jar for this Minecraft version and loader."
        ),
        DropReason::RequiredModLeftOut => format!(
            "The mod-list makes {it} require a mod this launch could not install, so the resolver left {it} out too."
        ),
    };

    format!("{cause} Nothing took {its} place, so the game starts without {it}.")
}

/// At most six names on the banner, all of them in the log.
fn dropped_mod_names(mod_ids: &[&str]) -> String {
    const SHOWN: usize = 6;

    let shown = mod_ids
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");

    match mod_ids.len().saturating_sub(SHOWN) {
        0 => shown,
        hidden => format!("{shown} and {hidden} more"),
    }
}

/// One notice per reason, in the order the reasons first appear; empty when
/// nothing was lost.
///
/// A notice per mod is what `report_unusable_probe` does, and it is the wrong
/// shape here: this pass decides about the whole selection at once, so a
/// Modrinth that does not answer drops every mod with no jar on disk — 24 of
/// them on this repository's own mod-list — and the banner shows three at a
/// time (`NoticeBanner.tsx:19`). Aggregating is what A3 already chose for the
/// cache fallback. Grouping by reason rather than into one line is the price
/// of not lying: the mods a dropped dependency took with it are out for a
/// different reason than the mods nothing could install, and one sentence
/// cannot be true of both. At most three notices, one per `DropReason`.
///
/// The title is `report_unusable_probe`'s, so that two stages reporting the
/// same kind of loss do not look like two different problems.
fn dropped_mods_notices(
    dropped: &[DroppedMod],
    target: &ResolutionTarget,
) -> Vec<(&'static str, String, String)> {
    let mut notices = Vec::new();

    for reason in [
        DropReason::NoVersionAndNoCachedJar,
        DropReason::NoChosenVersionAndNothingCached,
        DropReason::RequiredModLeftOut,
    ] {
        let mod_ids = dropped
            .iter()
            .filter(|lost| lost.reason == reason)
            .map(|lost| lost.mod_id.as_str())
            .collect::<Vec<_>>();
        let many = mod_ids.len() > 1;
        let clause = dropped_reason_clause(reason, target, many);

        let (title, message) = match mod_ids.as_slice() {
            [] => continue,
            [mod_id] => (
                "Mod left out of the launch",
                format!("'{mod_id}' is not in this launch: {clause}."),
            ),
            mod_ids => (
                "Mods left out of the launch",
                format!(
                    "{} mods are not in this launch ({}): {clause}.",
                    mod_ids.len(),
                    dropped_mod_names(mod_ids)
                ),
            ),
        };

        notices.push((title, message, dropped_reason_detail(reason, many)));
    }

    notices
}

/// The mods lost to the availability pass, said in both channels: one log line
/// each, one banner notice per reason (D28).
pub(super) fn report_dropped_mods(
    app_handle: &tauri::AppHandle,
    dropped: &[DroppedMod],
    target: &ResolutionTarget,
) {
    for lost in dropped {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Resolver] '{}' is not in this launch: {}, and no alternative took its place",
                lost.mod_id,
                dropped_reason_clause(lost.reason, target, false)
            ),
        );
    }

    for (title, message, detail) in dropped_mods_notices(dropped, target) {
        let _ = emit_launcher_issue(app_handle, title, &message, &detail, "warning", "launch");
    }
}

/// Selected Modrinth mods split by whether the pre-check already chose a
/// version for them.
///
/// `covered` is `(mod_id, version_id)`, one entry per mod id, in resolution
/// order — the same shape `versions_by_mod_id` consumes. `uncovered` is
/// everything else: mods the pre-check never saw, which means the mod-list
/// changed between the pre-check and the launch. That is a real case, not an
/// error, and those mods are resolved the usual way.
pub(super) fn split_selected_by_preresolved(
    selected_mods: &[SelectedMod],
    preresolved: &HashMap<String, String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut covered = Vec::new();
    let mut uncovered = Vec::new();
    let mut seen = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) {
            continue;
        }
        if !seen.insert(selected.mod_id.as_str()) {
            continue;
        }

        match preresolved.get(&selected.mod_id) {
            Some(version_id) => covered.push((selected.mod_id.clone(), version_id.clone())),
            None => uncovered.push(selected.mod_id.clone()),
        }
    }

    (covered, uncovered)
}

fn selection_subset(selected_mods: &[SelectedMod], mod_ids: &[String]) -> Vec<SelectedMod> {
    let wanted = mod_ids.iter().map(String::as_str).collect::<HashSet<_>>();

    selected_mods
        .iter()
        .filter(|selected| wanted.contains(selected.mod_id.as_str()))
        .cloned()
        .collect()
}

/// The mod ids the normal resolver still has to answer for, in the order they
/// will be logged: first the pre-checked versions Modrinth no longer returns,
/// then the mods the pre-check never covered.
///
/// The first group is the one that matters and the one nobody would think of:
/// the popup showed a version, the user agreed to it, and it is gone. Falling
/// back keeps the mod in the launch instead of dropping it silently.
pub(super) fn mod_ids_needing_resolution(
    covered: &[(String, String)],
    resolved: &HashMap<String, ModrinthVersion>,
    uncovered: &[String],
) -> (Vec<String>, Vec<String>) {
    let dropped = covered
        .iter()
        .map(|(mod_id, _)| mod_id)
        .filter(|mod_id| !resolved.contains_key(mod_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    let mut all = dropped.clone();
    all.extend(uncovered.iter().cloned());

    (all, dropped)
}

/// The versions the launch will install, taken from the ids the pre-check
/// already chose (D16) instead of choosing again.
///
/// `GET /versions?ids=` fetches those exact versions in one request — it is a
/// lookup, not a selection, so nothing here can pick a different version than
/// the popup showed. Modrinth still has to be asked because the metadata the
/// download stage needs (file name, url, sha1, size) lives in the version and
/// not in `mod_cache`.
///
/// Two sets fall back to the normal resolver: mod ids the map does not cover,
/// and version ids the response omitted — Modrinth dropped the exact version
/// the popup showed. Both are logged, because both mean the launch is not
/// installing what the user was shown.
pub(super) async fn resolve_preresolved_versions(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    selected_mods: &[SelectedMod],
    client: &ModrinthClient,
    target: &ResolutionTarget,
    preresolved: &HashMap<String, String>,
) -> Result<HashMap<String, ModrinthVersion>> {
    let (covered, uncovered) = split_selected_by_preresolved(selected_mods, preresolved);

    if !uncovered.is_empty() {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Launch] the pre-check did not cover {}; resolving {} the usual way (the mod-list changed between the pre-check and the launch)",
                uncovered.join(", "),
                if uncovered.len() == 1 { "it" } else { "them" }
            ),
        );
    }

    let version_ids = covered
        .iter()
        .map(|(_, version_id)| version_id.clone())
        .collect::<Vec<_>>();
    let by_version_id = client.fetch_versions_by_ids(&version_ids).await?;
    let resolved = versions_by_mod_id(&covered, &by_version_id);

    let (fallback_mod_ids, dropped) = mod_ids_needing_resolution(&covered, &resolved, &uncovered);
    if !dropped.is_empty() {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Launch] Modrinth no longer returns the pre-checked version of {}; resolving the usual way",
                dropped.join(", ")
            ),
        );
    }

    let fallback = resolve_compatible_versions_hybrid(
        app_handle,
        launcher_paths,
        http_client,
        &selection_subset(selected_mods, &fallback_mod_ids),
        client,
        target,
    )
    .await?;

    Ok(merge_resolved_versions(resolved, fallback))
}

/// Resolve, drop what Modrinth has no compatible version for, re-resolve, and
/// collect what the target loads.
///
/// Shared between `run_launch_pipeline` and the update pre-check on purpose:
/// the pre-check exists to show exactly what the launch will install, and two
/// copies of "which mods does this target load" are the easiest thing in this
/// feature to let drift apart. The version map of this pass is thrown away here
/// as it always was — only `unavailable` is used — because the authoritative
/// set is the one after the re-resolution.
///
/// `preresolved` short-circuits the availability question for the mods it
/// covers: the pre-check already established they have a version, so asking
/// again would both cost requests and let the two answers disagree. Mods it
/// does not cover still go through the resolver, so a mod-list that grew after
/// the pre-check is not silently disabled.
pub(super) async fn resolve_online_selection(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    modlist: &ModList,
    client: &ModrinthClient,
    target: &ResolutionTarget,
    preresolved: Option<&HashMap<String, String>>,
) -> Result<TargetSelection> {
    let resolution = resolve_modlist(modlist, target)?;
    let selected = collect_selected_mods(modlist, &resolution, target);
    let to_query = match preresolved {
        Some(preresolved) => {
            let (_, uncovered) = split_selected_by_preresolved(&selected, preresolved);
            selection_subset(&selected, &uncovered)
        }
        None => selected.clone(),
    };
    let versions = resolve_compatible_versions_hybrid(
        app_handle,
        launcher_paths,
        http_client,
        &to_query,
        client,
        target,
    )
    .await?;
    // D29 on this path too: a jar this machine already has makes the mod
    // available whatever Modrinth says, or fails to say. Without it a version
    // Modrinth has dropped, or a Modrinth that does not answer, empties the
    // mod-list — measured on this repository: 25 selected mods down to 1.
    //
    // `JarOnDisk` and not `RestorableFromRow`: see `CacheBacking`. Restoring
    // here would make the launch depend on the host that just went silent, and
    // a failed download ends the launch instead of costing one mod — measured.
    let cache_backed = cache_backed_mods(
        &probe_selected_mods(launcher_paths, &selected, target)?,
        CacheBacking::JarOnDisk,
    );
    let unavailable = unavailable_selected_mods(&selected, |mod_id| {
        versions.contains_key(mod_id)
            || cache_backed.contains(mod_id)
            || preresolved.is_some_and(|preresolved| preresolved.contains_key(mod_id))
    });
    // Taken before the pass disables anything: telling a lost mod from a
    // replaced one needs both resolutions. The clone only happens when
    // something is actually unavailable, the same path that then re-runs the
    // whole resolver.
    let before = (!unavailable.is_empty()).then(|| resolution.clone());
    let resolution = reresolve_without_unavailable(modlist, resolution, &unavailable, target)?;
    let selected_mods = collect_selected_mods(modlist, &resolution, target);

    Ok(TargetSelection {
        dropped: before
            .map(|before| {
                mods_dropped_without_replacement(
                    &before,
                    &resolution,
                    &unavailable,
                    DropReason::NoVersionAndNoCachedJar,
                )
            })
            .unwrap_or_default(),
        resolution,
        selected_mods,
    })
}

/// Same as `resolve_online_selection`, with no network at all.
///
/// "Available" is the cache's answer alone, and it covers a registered row
/// whose jar disappeared because the launch restores it: a mod is disabled
/// only when nothing can produce a jar for it. `preresolved` short-circuits
/// the question exactly as it does online — a version the pre-check chose
/// exists whether or not this machine has it, and an update accepted a second
/// ago never does.
///
/// Named for what it guarantees, zero requests, and not for the setting that
/// used to select it: `cache_only_mode` no longer exists, and a name that
/// describes a deleted setting is how this repository has already lost time
/// twice.
pub(super) fn resolve_offline_selection(
    launcher_paths: &LauncherPaths,
    modlist: &ModList,
    target: &ResolutionTarget,
    preresolved: Option<&HashMap<String, String>>,
) -> Result<TargetSelection> {
    let resolution = resolve_modlist(modlist, target)?;
    let selected = collect_selected_mods(modlist, &resolution, target);
    let cache_backed = cache_backed_mods(
        &probe_selected_mods(launcher_paths, &selected, target)?,
        CacheBacking::RestorableFromRow,
    );
    let unavailable = unavailable_selected_mods(&selected, |mod_id| {
        cache_backed.contains(mod_id)
            || preresolved.is_some_and(|preresolved| preresolved.contains_key(mod_id))
    });
    let before = (!unavailable.is_empty()).then(|| resolution.clone());
    let resolution = reresolve_without_unavailable(modlist, resolution, &unavailable, target)?;
    let selected_mods = collect_selected_mods(modlist, &resolution, target);

    Ok(TargetSelection {
        dropped: before
            .map(|before| {
                mods_dropped_without_replacement(
                    &before,
                    &resolution,
                    &unavailable,
                    DropReason::NoChosenVersionAndNothingCached,
                )
            })
            .unwrap_or_default(),
        resolution,
        selected_mods,
    })
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
    use crate::rules::{VersionRule, VersionRuleKind};

    fn test_version(id: &str, date_published: &str) -> ModrinthVersion {
        test_version_with_channel(id, date_published, "release")
    }

    fn test_version_with_channel(
        id: &str,
        date_published: &str,
        version_type: &str,
    ) -> ModrinthVersion {
        ModrinthVersion {
            id: id.into(),
            project_id: "example-project".into(),
            version_number: id.into(),
            name: id.into(),
            game_versions: vec!["26.1".into()],
            loaders: vec!["fabric".into()],
            version_type: version_type.into(),
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: date_published.into(),
        }
    }

    #[test]
    fn select_preferred_version_uses_release_before_newer_beta() {
        let release = test_version_with_channel(
            "iris-release",
            "2025-02-20T00:00:00Z",
            "release",
        );
        let beta =
            test_version_with_channel("iris-beta", "2026-06-13T00:00:00Z", "beta");

        assert_eq!(
            select_preferred_version(vec![beta, release.clone()]),
            Some(release)
        );
    }

    #[test]
    fn select_preferred_version_uses_newest_alpha_when_only_alphas_exist() {
        let older = test_version_with_channel("c2me-older", "2025-01-01T00:00:00Z", "alpha");
        let newest =
            test_version_with_channel("c2me-newest", "2026-06-13T00:00:00Z", "alpha");

        assert_eq!(
            select_preferred_version(vec![newest.clone(), older]),
            Some(newest)
        );
    }

    #[test]
    fn select_preferred_version_uses_beta_before_alpha_without_release() {
        let beta = test_version_with_channel("beta", "2025-01-01T00:00:00Z", "beta");
        let alpha = test_version_with_channel("alpha", "2026-06-13T00:00:00Z", "alpha");

        assert_eq!(
            select_preferred_version(vec![alpha, beta.clone()]),
            Some(beta)
        );
    }

    #[test]
    fn select_preferred_version_is_date_order_independent_within_channel() {
        let january = test_version("january", "2024-01-01T00:00:00Z");
        let june = test_version("june", "2024-06-01T00:00:00Z");
        let march = test_version("march", "2024-03-01T00:00:00Z");

        let first =
            select_preferred_version(vec![january.clone(), june.clone(), march.clone()]);
        let second = select_preferred_version(vec![march, january, june.clone()]);

        assert_eq!(first, Some(june.clone()));
        assert_eq!(second, Some(june));
    }

    fn modrinth_mod(mod_id: &str) -> SelectedMod {
        SelectedMod {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
        }
    }

    fn local_mod(mod_id: &str) -> SelectedMod {
        SelectedMod {
            mod_id: mod_id.into(),
            source: ModSource::Local,
        }
    }

    fn cached_hashes(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(mod_id, hash)| ((*mod_id).to_string(), (*hash).to_string()))
            .collect()
    }

    #[test]
    fn split_sends_every_mod_with_a_cached_hash_to_the_bulk_path() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];
        let split = split_selected_by_cached_hash(
            &selected,
            &cached_hashes(&[("sodium", "aaa1"), ("lithium", "bbb2")]),
        );

        assert_eq!(
            split.hashed,
            vec![
                ("sodium".to_string(), "aaa1".to_string()),
                ("lithium".to_string(), "bbb2".to_string()),
            ]
        );
        assert!(split.unhashed.is_empty());
    }

    #[test]
    fn split_sends_every_mod_to_the_per_project_path_without_cached_hashes() {
        let selected = vec![modrinth_mod("polytone"), modrinth_mod("connector-extras")];
        let split = split_selected_by_cached_hash(&selected, &HashMap::new());

        assert!(split.hashed.is_empty());
        assert_eq!(split.unhashed, vec!["polytone", "connector-extras"]);
    }

    #[test]
    fn split_separates_mixed_coverage_and_ignores_local_and_repeated_mods() {
        let selected = vec![
            modrinth_mod("sodium"),
            local_mod("my-own.jar"),
            modrinth_mod("polytone"),
            modrinth_mod("sodium"),
        ];
        let split =
            split_selected_by_cached_hash(&selected, &cached_hashes(&[("sodium", "aaa1")]));

        assert_eq!(split.hashed, vec![("sodium".to_string(), "aaa1".to_string())]);
        assert_eq!(split.unhashed, vec!["polytone"]);
    }

    #[tokio::test]
    async fn a_hash_omitted_by_the_bulk_response_is_retried_per_project() {
        let selected = vec![
            modrinth_mod("sodium"),
            modrinth_mod("lithium"),
            modrinth_mod("polytone"),
        ];
        let split = split_selected_by_cached_hash(
            &selected,
            &cached_hashes(&[("sodium", "aaa1"), ("lithium", "bbb2")]),
        );

        let outcome = resolve_versions_for_cached_hashes(&split, |_hashes| async {
            Ok(HashMap::from([(
                "aaa1".to_string(),
                test_version("sodium-version", "2026-01-01T00:00:00Z"),
            )]))
        })
        .await;

        assert_eq!(outcome.resolved.len(), 1);
        assert!(outcome.resolved.contains_key("sodium"));
        // An omission cannot be told apart from "no version for this target",
        // so the mod goes back on the per-project path instead of being
        // declared unavailable — ahead of the mods that never had a hash.
        assert_eq!(outcome.omitted_mod_ids, vec!["lithium"]);
        assert_eq!(outcome.per_project_mod_ids, vec!["lithium", "polytone"]);
        assert!(outcome.bulk_error.is_none());

        // ...and it resolves when the per-project stage finds the version, the
        // case where an unknown hash would otherwise silently drop a mod that
        // works today.
        let from_per_project = test_version("lithium-version", "2026-01-01T00:00:00Z");
        let merged = merge_resolved_versions(
            outcome.resolved,
            HashMap::from([("lithium".to_string(), from_per_project.clone())]),
        );
        assert_eq!(merged.get("lithium"), Some(&from_per_project));
        assert_eq!(merged.len(), 2);
    }

    #[tokio::test]
    async fn two_mod_ids_sharing_a_cached_hash_both_resolve() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("AANobbMI")];
        let split = split_selected_by_cached_hash(
            &selected,
            &cached_hashes(&[("sodium", "aaa1"), ("AANobbMI", "aaa1")]),
        );

        let outcome = resolve_versions_for_cached_hashes(&split, |hashes| async move {
            assert_eq!(hashes, vec!["aaa1".to_string(), "aaa1".to_string()]);
            Ok(HashMap::from([(
                "aaa1".to_string(),
                test_version("sodium-version", "2026-01-01T00:00:00Z"),
            )]))
        })
        .await;

        assert_eq!(outcome.resolved.len(), 2);
        assert_eq!(
            outcome.resolved.get("sodium"),
            outcome.resolved.get("AANobbMI")
        );
    }

    #[tokio::test]
    async fn a_failed_bulk_call_sends_its_whole_set_to_the_per_project_path() {
        let selected = vec![
            modrinth_mod("sodium"),
            modrinth_mod("lithium"),
            modrinth_mod("polytone"),
        ];
        let split = split_selected_by_cached_hash(
            &selected,
            &cached_hashes(&[("sodium", "aaa1"), ("lithium", "bbb2")]),
        );

        let outcome = resolve_versions_for_cached_hashes(&split, |_hashes| async {
            Err(anyhow::anyhow!("connection reset"))
        })
        .await;

        assert!(outcome.resolved.is_empty());
        assert_eq!(
            outcome.per_project_mod_ids,
            vec!["sodium", "lithium", "polytone"]
        );
        assert_eq!(outcome.bulk_error.as_deref(), Some("connection reset"));
    }

    #[tokio::test]
    async fn an_empty_bulk_set_costs_no_request() {
        let selected = vec![modrinth_mod("polytone")];
        let split = split_selected_by_cached_hash(&selected, &HashMap::new());

        let outcome = resolve_versions_for_cached_hashes(&split, |_hashes| async {
            panic!("the bulk endpoint must not be called without hashes");
        })
        .await;

        assert!(outcome.resolved.is_empty());
        assert_eq!(outcome.per_project_mod_ids, vec!["polytone"]);
    }

    #[test]
    fn per_project_results_only_fill_mod_ids_the_bulk_left_out() {
        let from_bulk = test_version("from-bulk", "2026-01-01T00:00:00Z");
        let from_per_project = test_version("from-per-project", "2026-01-01T00:00:00Z");

        let merged = merge_resolved_versions(
            HashMap::from([("sodium".to_string(), from_bulk.clone())]),
            HashMap::from([
                ("sodium".to_string(), from_per_project.clone()),
                ("polytone".to_string(), from_per_project.clone()),
            ]),
        );

        assert_eq!(merged.get("sodium"), Some(&from_bulk));
        assert_eq!(merged.get("polytone"), Some(&from_per_project));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn a_full_version_map_sends_no_mod_to_the_resolver() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];
        let preresolved = HashMap::from([
            ("sodium".to_string(), "sodium-v2".to_string()),
            ("lithium".to_string(), "lithium-v7".to_string()),
        ]);

        let (covered, uncovered) = split_selected_by_preresolved(&selected, &preresolved);

        assert_eq!(
            covered,
            vec![
                ("sodium".to_string(), "sodium-v2".to_string()),
                ("lithium".to_string(), "lithium-v7".to_string()),
            ]
        );
        assert!(uncovered.is_empty());
    }

    #[test]
    fn no_version_map_sends_every_mod_to_the_project_path() {
        // `resolve_selected_remote_artifacts` turns an absent map into an empty
        // one, so this is the whole of "without a map, exactly today's
        // behaviour": nothing is covered, every Modrinth mod is looked up by
        // project, and local mods stay out of it either way.
        let selected = vec![
            modrinth_mod("sodium"),
            local_mod("my-own.jar"),
            modrinth_mod("lithium"),
            modrinth_mod("sodium"),
        ];

        let (covered, uncovered) = split_selected_by_preresolved(&selected, &HashMap::new());

        assert!(covered.is_empty());
        assert_eq!(uncovered, vec!["sodium".to_string(), "lithium".to_string()]);
    }

    #[test]
    fn a_mod_added_after_the_precheck_is_resolved_as_usual() {
        let selected = vec![
            modrinth_mod("sodium"),
            modrinth_mod("polytone"),
            local_mod("optifine"),
            modrinth_mod("sodium"),
        ];
        let preresolved = HashMap::from([("sodium".to_string(), "sodium-v2".to_string())]);

        let (covered, uncovered) = split_selected_by_preresolved(&selected, &preresolved);

        // Local mods never take part, and a repeated mod id yields one entry.
        assert_eq!(
            covered,
            vec![("sodium".to_string(), "sodium-v2".to_string())]
        );
        assert_eq!(uncovered, vec!["polytone".to_string()]);
    }

    #[test]
    fn an_empty_version_map_sends_every_mod_to_the_resolver() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];

        let (covered, uncovered) = split_selected_by_preresolved(&selected, &HashMap::new());

        assert!(covered.is_empty());
        assert_eq!(
            uncovered,
            vec!["sodium".to_string(), "lithium".to_string()]
        );
    }

    #[test]
    fn a_prechecked_version_modrinth_dropped_falls_back_to_the_resolver() {
        let covered = vec![
            ("sodium".to_string(), "sodium-v2".to_string()),
            ("lithium".to_string(), "lithium-v7".to_string()),
        ];
        let resolved = HashMap::from([(
            "sodium".to_string(),
            test_version("sodium-v2", "2026-01-01T00:00:00Z"),
        )]);

        let (all, dropped) = mod_ids_needing_resolution(
            &covered,
            &resolved,
            &["polytone".to_string()],
        );

        assert_eq!(dropped, vec!["lithium".to_string()]);
        assert_eq!(all, vec!["lithium".to_string(), "polytone".to_string()]);
    }

    #[test]
    fn nothing_falls_back_when_every_prechecked_version_still_exists() {
        let covered = vec![("sodium".to_string(), "sodium-v2".to_string())];
        let resolved = HashMap::from([(
            "sodium".to_string(),
            test_version("sodium-v2", "2026-01-01T00:00:00Z"),
        )]);

        let (all, dropped) = mod_ids_needing_resolution(&covered, &resolved, &[]);

        assert!(all.is_empty());
        assert!(dropped.is_empty());
    }

    fn cached_record(version_id: &str, jar: &str) -> ModCacheRecord {
        ModCacheRecord {
            modrinth_project_id: "sodium".into(),
            modrinth_version_id: version_id.into(),
            jar_filename: jar.into(),
            mc_version: "1.20.1".into(),
            mod_loader: "forge".into(),
            file_hash: Some(format!("{version_id}-sha1")),
            download_url: Some(format!("https://cdn.modrinth.com/data/sodium/{jar}")),
            is_local: false,
        }
    }

    #[test]
    fn a_chosen_version_already_cached_is_installed_from_the_cache() {
        let record = cached_record("sodium-v2", "sodium-0.6.0.jar");

        assert_eq!(
            plan_for_named_version(CacheProbe::Ready(record.clone())),
            NamedVersionPlan::UseCached(record)
        );
    }

    #[test]
    fn a_chosen_version_the_cache_does_not_hold_is_fetched() {
        // The case the whole task exists for: an update accepted a moment ago
        // has no cache row, and the old branch inserted nothing at all for it.
        assert_eq!(
            plan_for_named_version(CacheProbe::NotCached),
            NamedVersionPlan::FetchMetadata
        );
    }

    #[test]
    fn a_chosen_version_whose_jar_is_gone_is_restored_at_that_version() {
        let record = cached_record("sodium-v1", "sodium-0.5.8.jar");

        assert_eq!(
            plan_for_named_version(CacheProbe::JarMissing(record.clone())),
            NamedVersionPlan::RestoreRegistered(record)
        );
    }

    #[test]
    fn a_chosen_version_with_an_incomplete_row_is_fetched_but_a_local_one_is_not() {
        let urlless = ModCacheRecord {
            download_url: None,
            ..cached_record("sodium-v1", "sodium-0.5.8.jar")
        };
        // Modrinth can answer for a real version id whose row lost its url.
        assert_eq!(
            plan_for_named_version(CacheProbe::JarMissingUnrecoverable(urlless)),
            NamedVersionPlan::FetchMetadata
        );

        let local = ModCacheRecord {
            is_local: true,
            download_url: None,
            ..cached_record("local-sodium", "my-sodium.jar")
        };
        // A synthetic id must never reach `GET /versions?ids=`: one non-base62
        // id fails the whole request, so this mod keeps today's behaviour.
        assert_eq!(
            plan_for_named_version(CacheProbe::JarMissingUnrecoverable(local)),
            NamedVersionPlan::ProjectLookup
        );
    }

    #[test]
    fn splitting_artifacts_keeps_the_restorable_ones_apart_from_the_usable_ones() {
        let cached = cached_record("sodium-v1", "sodium-0.5.8.jar");
        let missing = cached_record("lithium-v1", "lithium-0.11.jar");
        let live = test_version("iris-v3", "2026-01-01T00:00:00Z");

        let split = split_remote_artifacts(&[
            RemoteArtifact::Cached(cached.clone()),
            RemoteArtifact::MissingJar(missing.clone()),
            RemoteArtifact::Live(live.clone()),
            // Two mods can resolve to the same version id; the download stage
            // must see it once.
            RemoteArtifact::MissingJar(missing.clone()),
            RemoteArtifact::Live(live.clone()),
        ]);

        assert_eq!(split.cached_records, vec![cached]);
        assert_eq!(split.missing_jar_records, vec![missing]);
        assert_eq!(split.live_versions.len(), 1);
        assert_eq!(split.live_versions[0].id, live.id);
    }

    #[test]
    fn a_cached_jar_makes_a_mod_available_and_an_unrestorable_row_does_not() {
        let urlless = ModCacheRecord {
            download_url: None,
            ..cached_record("lithium-v1", "lithium-0.11.jar")
        };
        let probes = vec![
            (
                "sodium".to_string(),
                CacheProbe::Ready(cached_record("sodium-v1", "sodium-0.5.8.jar")),
            ),
            (
                "iris".to_string(),
                CacheProbe::JarMissing(cached_record("iris-v1", "iris-1.7.jar")),
            ),
            (
                "lithium".to_string(),
                CacheProbe::JarMissingUnrecoverable(urlless),
            ),
            ("polytone".to_string(), CacheProbe::NotCached),
        ];

        // The path with a version map may restore a jar that left the cache:
        // the version is named and the restore is the point.
        let restorable = cache_backed_mods(&probes, CacheBacking::RestorableFromRow);
        assert!(restorable.contains("sodium"));
        assert!(restorable.contains("iris"));
        // Nothing can produce a jar for these two, so availability must not
        // keep them: a mod kept without a jar is a mod missing in silence.
        assert!(!restorable.contains("lithium"));
        assert!(!restorable.contains("polytone"));
        assert_eq!(restorable.len(), 2);

        // The resolving path may not: restoring means downloading from the
        // host that just gave no answer, and one failed download ends the
        // launch. Measured: with the cdn blocked that turned a missing mod
        // into a launch that never started.
        let on_disk = cache_backed_mods(&probes, CacheBacking::JarOnDisk);
        assert_eq!(on_disk, HashSet::from(["sodium".to_string()]));
    }

    #[test]
    fn only_the_mods_no_version_came_back_for_fall_back_to_the_cache() {
        let sodium = cached_record("sodium-v1", "sodium-0.5.8.jar");
        let modernfix = cached_record("modernfix-v1", "modernfix-5.27.72.jar");
        let iris_cached = cached_record("iris-v1", "iris-1.7.jar");
        let resolved = HashMap::from([(
            "iris".to_string(),
            test_version("iris-v3", "2026-01-01T00:00:00Z"),
        )]);
        let probes = vec![
            ("sodium".to_string(), CacheProbe::Ready(sodium.clone())),
            ("iris".to_string(), CacheProbe::Ready(iris_cached)),
            (
                "modernfix".to_string(),
                CacheProbe::JarMissing(modernfix.clone()),
            ),
            ("polytone".to_string(), CacheProbe::NotCached),
        ];

        let fallback = cached_artifacts_for_unresolved(&probes, &resolved);

        // `sodium` only. `iris` resolved: adding its cached row would put two
        // version ids in the plan for one mod and let the launch install the
        // older one, the drift this feature exists to stop. `modernfix` has
        // lost its jar: fetching it back is exactly the request that fails
        // when Modrinth is the thing that went quiet, and a failed download
        // ends the launch.
        assert_eq!(
            fallback
                .iter()
                .map(|(mod_id, _)| mod_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sodium"]
        );
        assert!(matches!(&fallback[0].1, RemoteArtifact::Cached(record) if record == &sodium));
    }

    // ── A5: which disabled mods deserve a notice ─────────────────────────
    //
    // The availability pass disables three different things and only one of
    // them is a loss, so these tests run the real pair — `resolve_modlist`
    // then `reresolve_without_unavailable` — and check the decision against
    // it. No network: the availability verdict is the `unavailable` list,
    // which is what the network would have produced.

    fn a5_target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.20.1".into(),
            mod_loader: ModLoader::Forge,
        }
    }

    fn a5_rule(mod_id: &str, alternatives: Vec<Rule>) -> Rule {
        Rule {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives,
        }
    }

    fn a5_modlist(rules: Vec<Rule>) -> ModList {
        ModList {
            modlist_name: "A5 Pack".into(),
            author: "Author".into(),
            description: "Test".into(),
            rules,
        }
    }

    /// The pass as the launch runs it: resolve, disable what is unavailable,
    /// re-resolve, decide who deserves a notice. Returns `(lost, loaded)`.
    fn a5_pass(modlist: &ModList, unavailable: &[&str]) -> (Vec<DroppedMod>, Vec<String>) {
        let target = a5_target();
        let unavailable = unavailable
            .iter()
            .map(|mod_id| (*mod_id).to_string())
            .collect::<Vec<_>>();
        let before = resolve_modlist(modlist, &target).expect("first resolution");
        let after = reresolve_without_unavailable(modlist, before.clone(), &unavailable, &target)
            .expect("re-resolution");
        let dropped = mods_dropped_without_replacement(
            &before,
            &after,
            &unavailable,
            DropReason::NoVersionAndNoCachedJar,
        );
        let loaded = collect_selected_mods(modlist, &after, &target)
            .into_iter()
            .map(|selected| selected.mod_id)
            .collect::<Vec<_>>();

        (dropped, loaded)
    }

    fn a5_lost(dropped: &[DroppedMod]) -> Vec<(&str, DropReason)> {
        dropped
            .iter()
            .map(|lost| (lost.mod_id.as_str(), lost.reason))
            .collect()
    }

    #[test]
    fn a_mod_nothing_can_install_and_nothing_replaces_is_named() {
        let modlist = a5_modlist(vec![a5_rule("modernfix", vec![]), a5_rule("sodium", vec![])]);

        let (dropped, loaded) = a5_pass(&modlist, &["modernfix"]);

        assert_eq!(
            a5_lost(&dropped),
            vec![("modernfix", DropReason::NoVersionAndNoCachedJar)]
        );
        // And it really is out of the game, which is what makes it a loss.
        assert_eq!(loaded, vec!["sodium".to_string()]);
    }

    #[test]
    fn a_mod_an_alternative_replaces_is_not_a_notice() {
        // The group did its job: saying "embeddium was left out" while its
        // alternative is loaded is false and worries the user for nothing.
        let modlist = a5_modlist(vec![a5_rule("embeddium", vec![a5_rule("sodium", vec![])])]);

        let (dropped, loaded) = a5_pass(&modlist, &["embeddium"]);

        assert!(dropped.is_empty());
        assert_eq!(loaded, vec!["sodium".to_string()]);
    }

    #[test]
    fn a_mod_the_rules_excluded_never_reaches_the_availability_pass() {
        // `version_rules` is the user's own decision, taken by the resolver
        // before this pass: the mod is not in the selection the pass looks at,
        // so nothing here can turn it into a notice — even with a machine that
        // can install nothing at all.
        let mut forge_only = a5_rule("embeddium", vec![]);
        forge_only.version_rules = vec![VersionRule {
            kind: VersionRuleKind::Only,
            mc_versions: vec!["1.20.1".into()],
            loader: "fabric".into(),
        }];
        let modlist = a5_modlist(vec![forge_only, a5_rule("sodium", vec![])]);

        let target = a5_target();
        let before = resolve_modlist(&modlist, &target).expect("first resolution");
        let selected = collect_selected_mods(&modlist, &before, &target);
        let unavailable = unavailable_selected_mods(&selected, |_| false);

        assert_eq!(unavailable, vec!["sodium".to_string()]);
        // And the diff agrees: the excluded rule was already `Unresolved`
        // before the pass, so only the available-then-unavailable one is lost.
        let (dropped, _) = a5_pass(&modlist, &["sodium"]);
        assert_eq!(
            a5_lost(&dropped),
            vec![("sodium", DropReason::NoVersionAndNoCachedJar)]
        );
    }

    #[test]
    fn a_mod_a_dropped_dependency_takes_with_it_is_named_too() {
        // `entity-model-features` requires `entitytexturefeatures`. When the
        // required mod goes unavailable the resolver leaves both out, but only
        // the first is in `unavailable`: iterating that list would let this
        // loss through in silence, one rule further down.
        let mut dependent = a5_rule("entity-model-features", vec![]);
        dependent.requires = vec!["entitytexturefeatures".into()];
        let modlist = a5_modlist(vec![a5_rule("entitytexturefeatures", vec![]), dependent]);

        let (dropped, loaded) = a5_pass(&modlist, &["entitytexturefeatures"]);

        assert_eq!(
            a5_lost(&dropped),
            vec![
                ("entitytexturefeatures", DropReason::NoVersionAndNoCachedJar),
                ("entity-model-features", DropReason::RequiredModLeftOut),
            ]
        );
        assert!(loaded.is_empty());
    }

    #[test]
    fn every_mod_lost_in_the_same_pass_is_named_once() {
        let modlist = a5_modlist(vec![
            a5_rule("modernfix", vec![]),
            a5_rule("embeddium", vec![a5_rule("sodium", vec![])]),
            a5_rule("iris", vec![]),
            a5_rule("cull-leaves", vec![]),
        ]);

        // Three unavailable; `embeddium` has somewhere to fall back to, the
        // other two do not.
        let (dropped, loaded) = a5_pass(&modlist, &["modernfix", "embeddium", "iris"]);

        assert_eq!(
            a5_lost(&dropped),
            vec![
                ("modernfix", DropReason::NoVersionAndNoCachedJar),
                ("iris", DropReason::NoVersionAndNoCachedJar),
            ]
        );
        assert_eq!(
            loaded,
            vec!["sodium".to_string(), "cull-leaves".to_string()]
        );
    }

    #[test]
    fn a_launch_that_loses_nothing_says_nothing() {
        let modlist = a5_modlist(vec![a5_rule("sodium", vec![]), a5_rule("iris", vec![])]);

        let (dropped, loaded) = a5_pass(&modlist, &[]);

        assert!(dropped.is_empty());
        assert_eq!(loaded, vec!["sodium".to_string(), "iris".to_string()]);
        assert!(dropped_mods_notices(&dropped, &a5_target()).is_empty());
    }

    #[test]
    fn the_notice_names_the_mod_and_the_reason_in_the_words_of_whoever_reads_it() {
        let notices = dropped_mods_notices(
            &[DroppedMod {
                mod_id: "modernfix".into(),
                reason: DropReason::NoVersionAndNoCachedJar,
            }],
            &a5_target(),
        );

        // Same title as the artifact stage: two channels saying the same thing
        // in two ways is a way of confusing people.
        assert_eq!(
            notices,
            vec![(
                "Mod left out of the launch",
                "'modernfix' is not in this launch: there is no version of it for Minecraft 1.20.1 / forge and no jar for it in your cache.".to_string(),
                "Modrinth returned no compatible version for this Minecraft version and loader — there may be none, or it may not have answered — and the cache has no jar to install instead. Nothing took its place, so the game starts without it.".to_string(),
            )]
        );
    }

    #[test]
    fn many_lost_mods_produce_one_notice_that_caps_the_names_it_lists() {
        let dropped = (1..=8)
            .map(|index| DroppedMod {
                mod_id: format!("mod-{index}"),
                reason: DropReason::NoChosenVersionAndNothingCached,
            })
            .collect::<Vec<_>>();

        let notices = dropped_mods_notices(&dropped, &a5_target());

        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "Mods left out of the launch");
        assert_eq!(
            notices[0].1,
            "8 mods are not in this launch (mod-1, mod-2, mod-3, mod-4, mod-5, mod-6 and 2 more): this launch had no version chosen for them and your cache has no jar for Minecraft 1.20.1 / forge."
        );
        // Plural all the way through, detail included: the singular case is
        // the headline of this feature and reads on its own.
        assert!(notices[0]
            .2
            .ends_with("Nothing took their place, so the game starts without them."));
    }

    #[test]
    fn two_reasons_in_one_pass_produce_one_notice_each() {
        // One sentence cannot be true of both groups, so they do not share a
        // notice — and they stay at two notices, not one per mod.
        let notices = dropped_mods_notices(
            &[
                DroppedMod {
                    mod_id: "entitytexturefeatures".into(),
                    reason: DropReason::NoVersionAndNoCachedJar,
                },
                DroppedMod {
                    mod_id: "entity-model-features".into(),
                    reason: DropReason::RequiredModLeftOut,
                },
            ],
            &a5_target(),
        );

        assert_eq!(
            notices
                .iter()
                .map(|(_, message, _)| message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "'entitytexturefeatures' is not in this launch: there is no version of it for Minecraft 1.20.1 / forge and no jar for it in your cache.",
                "'entity-model-features' is not in this launch: a mod it needs is not in this launch either.",
            ]
        );
    }
}
