use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::content_packs::{load_content_list, ContentEntry, ContentList};
use crate::launcher_paths::LauncherPaths;
use crate::local_content_packs::{link_local_pack, plan_local_pack_installs, PackLinkOutcome};
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::path_safety::validate_path_component;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::ResolutionTarget;
use crate::rules::VersionRuleKind;

use super::{download_file, emit_log, select_preferred_version};

/// The three content categories, each with the instance subdirectory its packs
/// are linked into.
///
/// One list because the pre-check keys [`ResolvedContentVersions`] by the
/// category string and the launch looks its own entries up by that same
/// string: a renamed or added category has to move both sides at once.
pub(super) const CONTENT_CATEGORIES: [(&str, &str); 3] = [
    ("resourcepack", "resourcepacks"),
    ("shader", "shaderpacks"),
    ("datapack", "datapacks"),
];

/// The category the loop below does not install: data packs go to one
/// top-level directory, through [`install_datapacks`].
pub(super) const DATAPACK_CATEGORY: &str = "datapack";

/// `category → (entry id → Modrinth version id)`: the content-pack half of what
/// the update pre-check decided (D58).
///
/// Nested rather than one namespaced key because the two levels are two
/// different identities — the category picks the list file and the instance
/// directory, the entry id is the Modrinth slug inside it, and the same slug
/// may legitimately appear in two categories. A flat `"resourcepack:slug"` key
/// would make both sides parse a delimiter nothing forbids a slug from
/// containing.
pub type ResolvedContentVersions = HashMap<String, HashMap<String, String>>;

/// Checks whether a content entry is active for the current MC version +
/// loader. Shared with the pre-check: the two must agree on which entries the
/// launch even considers.
pub(super) fn is_content_entry_active(entry: &ContentEntry, mc_version: &str, loader: &str) -> bool {
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

/// The versions a pre-check already chose, with the metadata the install needs.
///
/// The pre-check names version ids and nothing else, so the launch still has
/// to learn each one's file name and url — but in **one** `GET /versions?ids=`
/// for all three categories together, instead of one
/// `GET /project/{id}/version` per entry. An entry the map does **not** name
/// still costs its own project request here: a pre-check that failed on it, or
/// one Modrinth has no version of at all, is resolved the usual way.
///
/// On the real mod-list that is 5 requests in the pre-check and 2 here — one
/// bulk for the four entries that resolved, one project lookup for
/// `visual-effects-plus`, which has no 1.20.1 version and is therefore in no
/// map — against 5 and 5 for two independent resolutions that can also
/// disagree with each other (D58).
pub(super) struct ContentPackPlan<'a> {
    named: Option<&'a ResolvedContentVersions>,
    metadata: HashMap<String, ModrinthVersion>,
}

impl<'a> ContentPackPlan<'a> {
    /// No answer to install: every entry is resolved the way a launch without
    /// a pre-check always has.
    pub(super) fn unplanned() -> Self {
        Self {
            named: None,
            metadata: HashMap::new(),
        }
    }

    /// Pure half, for the tests and for anyone who already has the metadata.
    pub(super) fn new(
        named: Option<&'a ResolvedContentVersions>,
        metadata: HashMap<String, ModrinthVersion>,
    ) -> Self {
        Self { named, metadata }
    }

    /// One request, or none at all when the map is absent or empty — an empty
    /// map carries no decision, exactly as it does for the mods
    /// (`LaunchResolutionPath::for_launch`).
    pub(super) async fn prepare(
        modrinth_client: &ModrinthClient,
        named: Option<&'a ResolvedContentVersions>,
    ) -> Result<Self> {
        let version_ids = named_content_version_ids(named);
        if version_ids.is_empty() {
            return Ok(Self::unplanned());
        }

        let metadata = modrinth_client.fetch_versions_by_ids(&version_ids).await?;
        Ok(Self::new(named, metadata))
    }

    /// The version the pre-check named for one entry, when Modrinth still
    /// returns it. `None` sends the entry down the normal resolution.
    pub(super) fn version_for(&self, category: &str, entry_id: &str) -> Option<&ModrinthVersion> {
        let version_id = self.named?.get(category)?.get(entry_id)?;
        self.metadata.get(version_id)
    }

    /// `category/entry id` for every entry the map named and the lookup did
    /// not answer for: Modrinth no longer has the exact version the popup
    /// showed, so the launch resolves it again and has to say so.
    pub(super) fn dropped(&self) -> Vec<String> {
        let Some(named) = self.named else {
            return Vec::new();
        };

        let mut dropped = named
            .iter()
            .flat_map(|(category, entries)| {
                entries.iter().filter_map(move |(entry_id, version_id)| {
                    (!self.metadata.contains_key(version_id))
                        .then(|| format!("{category}/{entry_id}"))
                })
            })
            .collect::<Vec<_>>();

        dropped.sort();
        dropped
    }

    /// How many entries the launch will install without resolving anything.
    pub(super) fn covered(&self) -> usize {
        self.named.map_or(0, |named| {
            named
                .values()
                .flat_map(|entries| entries.values())
                .filter(|version_id| self.metadata.contains_key(*version_id))
                .count()
        })
    }
}

/// Every version id the map names, sorted and deduplicated so the request is
/// reproducible — the same reason `version_ids_needing_a_number` sorts.
pub(super) fn named_content_version_ids(named: Option<&ResolvedContentVersions>) -> Vec<String> {
    let Some(named) = named else {
        return Vec::new();
    };

    let mut version_ids = named
        .values()
        .flat_map(|entries| entries.values())
        .filter(|version_id| !version_id.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();

    version_ids.sort();
    version_ids.dedup();
    version_ids
}

/// One active Modrinth entry after the version decision and before any
/// download.
///
/// Pulled out of the two install loops so the decision is measurable without a
/// `tauri::AppHandle` — the same extraction `ensure_content_pack_cached` got in
/// C5, for the same reason: everything inside those loops is unreachable from
/// the test suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContentEntryResolution {
    pub(super) entry_id: String,
    /// `None` with no `lookup_error` is an answer, not a failure: Modrinth has
    /// no version of this entry for the target. The two cases are kept apart
    /// because only one of them may freeze the category's removals.
    pub(super) version: Option<ModrinthVersion>,
    pub(super) lookup_error: Option<String>,
    /// The version came from the pre-check's map instead of a resolution here.
    pub(super) preresolved: bool,
}

/// Decide which version of each Modrinth entry this launch installs.
///
/// The pre-checked version wins whenever the map names one that Modrinth still
/// returns; everything else is resolved exactly as a launch without a
/// pre-check does, with the same preference the mods use (D56).
pub(super) async fn resolve_content_entry_versions(
    modrinth_client: &ModrinthClient,
    plan: &ContentPackPlan<'_>,
    category: &str,
    mc_version: &str,
    entry_ids: &[&str],
) -> Vec<ContentEntryResolution> {
    let mut resolutions = Vec::with_capacity(entry_ids.len());

    for entry_id in entry_ids {
        if let Some(version) = plan.version_for(category, entry_id) {
            resolutions.push(ContentEntryResolution {
                entry_id: (*entry_id).to_string(),
                version: Some(version.clone()),
                lookup_error: None,
                preresolved: true,
            });
            continue;
        }

        let resolution = match modrinth_client
            .fetch_content_pack_versions(entry_id, mc_version)
            .await
        {
            Ok(versions) => ContentEntryResolution {
                entry_id: (*entry_id).to_string(),
                version: select_preferred_version(versions),
                lookup_error: None,
                preresolved: false,
            },
            Err(error) => ContentEntryResolution {
                entry_id: (*entry_id).to_string(),
                version: None,
                lookup_error: Some(format!("{error:#}")),
                preresolved: false,
            },
        };
        resolutions.push(resolution);
    }

    resolutions
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
///
/// `resolved_content` is the pre-check's answer (D58). Without it the launch
/// re-resolves from scratch, which is what it always did — and which is also
/// how a popup's answer used to be ignored in silence, since nothing carried
/// it here.
pub(super) async fn resolve_and_install_content_packs(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    modrinth_client: &ModrinthClient,
    modlist_name: &str,
    target: &ResolutionTarget,
    instance_root: &Path,
    resolved_content: Option<&ResolvedContentVersions>,
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

    // Degraded, never fatal: without the metadata every entry is resolved the
    // usual way, which is exactly the launch of before this field existed.
    let plan = match ContentPackPlan::prepare(modrinth_client, resolved_content).await {
        Ok(plan) => plan,
        Err(error) => {
            emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Content] Could not look up the pre-checked pack versions; resolving every entry the usual way ({error:#})"
                ),
            )?;
            ContentPackPlan::unplanned()
        }
    };
    if plan.covered() > 0 {
        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Content] Installing the pre-checked version of {} entr{}",
                plan.covered(),
                if plan.covered() == 1 { "y" } else { "ies" }
            ),
        )?;
    }
    let dropped = plan.dropped();
    if !dropped.is_empty() {
        emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Content] Modrinth no longer returns the pre-checked version of {}; resolving the usual way",
                dropped.join(", ")
            ),
        )?;
    }

    for (content_type, instance_subdir) in CONTENT_CATEGORIES
        .iter()
        .filter(|(content_type, _)| *content_type != DATAPACK_CATEGORY)
        .copied()
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

        for entry in active_entries
            .iter()
            .filter(|entry| entry.source != "modrinth" && entry.source != "local")
        {
            // The old code skipped everything non-Modrinth without a word, so a
            // typo in `source` looked like an empty list.
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Content] Skipping '{}': unknown source '{}'",
                    entry.id, entry.source
                ),
            )?;
        }

        let modrinth_entry_ids: Vec<&str> = active_entries
            .iter()
            .filter(|entry| entry.source == "modrinth")
            .map(|entry| entry.id.as_str())
            .collect();

        for resolution in resolve_content_entry_versions(
            modrinth_client,
            &plan,
            content_type,
            mc_version,
            &modrinth_entry_ids,
        )
        .await
        {
            let entry_id = resolution.entry_id;
            if let Some(error) = resolution.lookup_error {
                // Not an answer: we do not know which file this entry wants,
                // so nothing may be removed on this pass.
                lookups_complete = false;
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!("[Content] Failed to fetch versions for '{entry_id}': {error}"),
                )?;
                continue;
            }

            let Some(version) = resolution.version else {
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] No compatible version found for '{entry_id}' on {mc_version}"
                    ),
                )?;
                continue;
            };
            let Some(file) = version.primary_file() else {
                continue;
            };

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
                        format!("[Content] Downloading {} ({})", entry_id, file.filename),
                    )
                },
            )
            .await
            .with_context(|| format!("failed to download content pack '{entry_id}'"))?;
            let target_path = instance_dir.join(&file.filename);
            crate::instance_mods::create_file_link(&cached_path, &target_path).with_context(
                || format!("failed to link content pack '{entry_id}' into instance"),
            )?;
            if !installed.iter().any(|name| name == &file.filename) {
                installed.push(file.filename.clone());
            }
            let cache_label = if was_cached { " (cached)" } else { "" };
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!("[Content] {entry_id} -> {instance_subdir}{cache_label}"),
            )?;
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
        &plan,
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
    plan: &ContentPackPlan<'_>,
    mc_version: &str,
    loader_str: &str,
    instance_root: &Path,
) -> Result<()> {
    // Data packs are world-specific, so put them in a top-level datapacks
    // folder supported by mods such as Open Loader.
    let list =
        load_content_list(modlist_dir, DATAPACK_CATEGORY).unwrap_or_else(|_| ContentList {
            content_type: DATAPACK_CATEGORY.to_string(),
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
        DATAPACK_CATEGORY,
        instance_root,
        "datapacks",
        &instance_dir,
        &active_entries,
    )?);

    for entry in active_entries
        .iter()
        .filter(|entry| entry.source != "modrinth" && entry.source != "local")
    {
        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Content] Skipping '{}': unknown source '{}'",
                entry.id, entry.source
            ),
        )?;
    }

    let modrinth_entry_ids: Vec<&str> = active_entries
        .iter()
        .filter(|entry| entry.source == "modrinth")
        .map(|entry| entry.id.as_str())
        .collect();

    for resolution in resolve_content_entry_versions(
        modrinth_client,
        plan,
        DATAPACK_CATEGORY,
        mc_version,
        &modrinth_entry_ids,
    )
    .await
    {
        let entry_id = resolution.entry_id;
        if let Some(error) = resolution.lookup_error {
            // Not an answer: we do not know which file this entry wants, so
            // nothing may be removed on this pass.
            lookups_complete = false;
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!("[Content] Failed to fetch versions for '{entry_id}': {error}"),
            )?;
            continue;
        }

        // A data pack with no compatible version is silent here, as it has
        // always been; the resource pack loop says so out loud. The pre-check
        // reports both (D60), and aligning the two logs is not this task.
        let Some(version) = resolution.version else {
            continue;
        };
        let Some(file) = version.primary_file() else {
            continue;
        };

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
                    format!("[Content] Downloading {} ({})", entry_id, file.filename),
                )
            },
        )
        .await
        .with_context(|| format!("failed to download data pack '{entry_id}'"))?;
        let target_path = instance_dir.join(&file.filename);
        crate::instance_mods::create_file_link(&cached_path, &target_path)
            .with_context(|| format!("failed to link data pack '{entry_id}' into instance"))?;
        if !installed.iter().any(|name| name == &file.filename) {
            installed.push(file.filename.clone());
        }
        let cache_label = if was_cached { " (cached)" } else { "" };
        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            format!("[Content] {entry_id} -> datapacks{cache_label}"),
        )?;
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
    use std::sync::{Arc, Mutex};
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

    /// A server that answers a body per path fragment and records every
    /// request line, so a test can prove which endpoint was asked — and, more
    /// to the point, which was not.
    fn routing_server(routes: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener
            .local_addr()
            .expect("test server has an address")
            .port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut scratch = [0u8; 4096];
                let read = stream.read(&mut scratch).unwrap_or(0);
                let request_line = String::from_utf8_lossy(&scratch[..read])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                recorder.lock().expect("recorder").push(request_line.clone());

                let body = routes
                    .iter()
                    .find(|(fragment, _)| request_line.contains(fragment))
                    .map_or_else(|| "[]".to_string(), |(_, body)| body.clone());
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        (format!("http://127.0.0.1:{port}"), requests)
    }

    fn version_json(
        project_id: &str,
        version_id: &str,
        version_number: &str,
        version_type: &str,
        date_published: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": version_id,
            "project_id": project_id,
            "version_number": version_number,
            "name": version_number,
            "game_versions": ["1.20.1"],
            "loaders": ["minecraft"],
            "version_type": version_type,
            "files": [{
                "hashes": { "sha1": "0c4978ce9daa8343189cffb55f0ad0f096718f50" },
                "url": format!("http://example.invalid/{version_id}.zip"),
                "filename": format!("{version_number}.zip"),
                "primary": true,
                "size": 4,
            }],
            "date_published": date_published,
        })
    }

    fn plan_map(entries: &[(&str, &str, &str)]) -> ResolvedContentVersions {
        let mut map = ResolvedContentVersions::new();
        for (category, entry_id, version_id) in entries {
            map.entry((*category).to_string())
                .or_default()
                .insert((*entry_id).to_string(), (*version_id).to_string());
        }
        map
    }

    /// D58, measured where it matters: with the pre-check's map in hand the
    /// launch installs the named version and asks the project endpoint
    /// **nothing**. Before this parameter existed the launch re-resolved, so a
    /// popup answer could be contradicted in silence.
    #[tokio::test]
    async fn a_pre_checked_version_is_installed_without_resolving_again() {
        let (base_url, requests) = routing_server(vec![(
            "/project/",
            serde_json::json!([version_json(
                "50dA9Sha",
                "a-different-one",
                "1.11.0",
                "beta",
                "2026-09-01"
            )])
            .to_string(),
        )]);
        let client = ModrinthClient::with_base_url(base_url);
        let named = plan_map(&[("resourcepack", "fresh-animations", "xN57JJts")]);
        let metadata = HashMap::from([(
            "xN57JJts".to_string(),
            serde_json::from_value::<ModrinthVersion>(version_json(
                "50dA9Sha",
                "xN57JJts",
                "1.10.4",
                "beta",
                "2026-02-24",
            ))
            .expect("the version payload should deserialize"),
        )]);
        let plan = ContentPackPlan::new(Some(&named), metadata);

        let resolutions = resolve_content_entry_versions(
            &client,
            &plan,
            "resourcepack",
            "1.20.1",
            &["fresh-animations"],
        )
        .await;

        assert_eq!(
            resolutions
                .iter()
                .map(|resolution| (
                    resolution.version.as_ref().map(|version| version.id.as_str()),
                    resolution.preresolved
                ))
                .collect::<Vec<_>>(),
            vec![(Some("xN57JJts"), true)]
        );
        assert!(
            requests.lock().expect("recorder").is_empty(),
            "the pre-checked entry must cost no request at all, got {:?}",
            requests.lock().expect("recorder")
        );
    }

    /// The other half of D58's cost: the ids the map names are looked up
    /// **once**, for every category together, and an id Modrinth no longer
    /// returns is named instead of silently installing something else.
    #[tokio::test]
    async fn the_named_versions_cost_one_request_and_a_dropped_one_is_reported() {
        let (base_url, requests) = routing_server(vec![(
            "/versions",
            serde_json::json!([
                version_json("50dA9Sha", "xN57JJts", "1.10.4", "beta", "2026-02-24"),
                version_json("8d8M3Qoz", "3c40Y4EH", "12.2", "release", "2025-01-28"),
            ])
            .to_string(),
        )]);
        let client = ModrinthClient::with_base_url(base_url);
        let named = plan_map(&[
            ("resourcepack", "fresh-animations", "xN57JJts"),
            ("resourcepack", "nature-x", "3c40Y4EH"),
            ("datapack", "gone-from-modrinth", "deleted1"),
        ]);

        let plan = ContentPackPlan::prepare(&client, Some(&named))
            .await
            .expect("the metadata lookup should succeed");

        let recorded = requests.lock().expect("recorder").clone();
        assert_eq!(recorded.len(), 1, "one request for three ids, got {recorded:?}");
        assert!(
            recorded[0].contains("/versions?ids="),
            "the bulk metadata endpoint is the one asked, got {recorded:?}"
        );
        assert_eq!(plan.covered(), 2);
        assert_eq!(plan.dropped(), vec!["datapack/gone-from-modrinth".to_string()]);
        assert_eq!(
            plan.version_for("resourcepack", "nature-x")
                .map(|version| version.version_number.as_str()),
            Some("12.2")
        );
        assert!(
            plan.version_for("shader", "nature-x").is_none(),
            "the category is part of the key, not decoration"
        );
    }

    /// D56 on the install path: an entry the map does not cover is resolved
    /// the way a launch without a pre-check always has, and the preference is
    /// now the mods' — channel first, date second. The beta here is the newest
    /// by date and still does not win.
    #[tokio::test]
    async fn an_uncovered_entry_is_resolved_by_channel_then_date() {
        let (base_url, requests) = routing_server(vec![(
            "/project/enhanced-boss-bars/version",
            serde_json::json!([
                version_json("U5SedJ9S", "release16", "1.6", "release", "2026-06-13"),
                version_json("U5SedJ9S", "beta17", "1.7-beta", "beta", "2026-08-01"),
            ])
            .to_string(),
        )]);
        let client = ModrinthClient::with_base_url(base_url);

        let resolutions = resolve_content_entry_versions(
            &client,
            &ContentPackPlan::unplanned(),
            "resourcepack",
            "1.20.1",
            &["enhanced-boss-bars"],
        )
        .await;

        assert_eq!(
            resolutions[0]
                .version
                .as_ref()
                .map(|version| version.id.as_str()),
            Some("release16"),
            "the newest by date is the beta; the preferred one is the release"
        );
        assert!(!resolutions[0].preresolved);
        assert_eq!(requests.lock().expect("recorder").len(), 1);
    }
}
