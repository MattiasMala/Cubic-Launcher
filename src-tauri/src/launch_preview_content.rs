use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::content_packs::{load_content_list, ContentEntry};
use crate::launcher_paths::LauncherPaths;
use crate::local_content_packs::{link_local_pack_as, plan_local_pack_installs, PackLinkOutcome};
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::path_safety::validate_path_component;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{ModLoader, ResolutionTarget};
use crate::rules::VersionRuleKind;

use super::{download_file, emit_launcher_issue, emit_log, select_preferred_version};

/// The three content categories, each with the subdirectory its packs are
/// linked into: of the instance, or for data packs of each world (D144).
///
/// One list because the pre-check keys [`ResolvedContentVersions`] by the
/// category string and the launch looks its own entries up by that same
/// string: a renamed or added category has to move both sides at once.
pub(super) const CONTENT_CATEGORIES: [(&str, &str); 3] = [
    ("resourcepack", "resourcepacks"),
    ("shader", "shaderpacks"),
    ("datapack", "datapacks"),
];

/// The category the loop below does not install: data packs go into every
/// world of the instance, through [`install_datapacks`] (D144).
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
pub(super) fn is_content_entry_active(
    entry: &ContentEntry,
    mc_version: &str,
    loader: &str,
) -> bool {
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
                entries
                    .iter()
                    .filter(move |&(_, version_id)| !self.metadata.contains_key(version_id))
                    .map(move |(entry_id, _)| format!("{category}/{entry_id}"))
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
/// pre-check does, with the same preference the mods use (D56), among the
/// versions that are packs of the category (contentpack-8).
pub(super) async fn resolve_content_entry_versions(
    modrinth_client: &ModrinthClient,
    plan: &ContentPackPlan<'_>,
    category: &str,
    mc_version: &str,
    entry_ids: &[&str],
) -> Vec<ContentEntryResolution> {
    let mut resolutions = Vec::with_capacity(entry_ids.len());

    for entry_id in entry_ids {
        // contentpack-8: a named version that is not a pack of this category
        // — a `.jar` an older launch installed, kept by a "Skip" — is not
        // installed; the entry is resolved again among its own packs.
        if let Some(version) = plan
            .version_for(category, entry_id)
            .filter(|version| version_fits_category(category, version))
        {
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
                version: select_preferred_version(versions_for_category(category, versions)),
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

/// Where the linking steps write their launch-log lines: `emit_log` in the
/// launch, a vector in the tests, which have no `tauri::AppHandle`.
pub(super) type ContentLog<'a> = dyn FnMut(ProcessLogStream, String) -> Result<()> + 'a;

/// One pack this launch links, after every decision and download: what the
/// per-directory linking needs and nothing else.
///
/// Pulled out of the linking so that data packs can be prepared once and
/// linked into every world (D144): the lookups and the downloads happen once
/// per launch, the links once per directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedPack {
    /// The name the pack gets in the target directory.
    pub(super) file_name: String,
    pub(super) source: PreparedPackSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PreparedPackSource {
    /// A `source: "local"` pack, file or folder, inside the mod list.
    Local { source_path: PathBuf },
    /// A Modrinth pack in the cache, with the identity the manifest records.
    Modrinth {
        entry_id: String,
        version_id: String,
        cached_path: PathBuf,
        was_cached: bool,
    },
}

/// Every pack of one category this launch links, and whether that list is
/// the whole answer.
#[derive(Debug)]
pub(super) struct PreparedCategory {
    pub(super) packs: Vec<PreparedPack>,
    /// `false` when at least one Modrinth entry is unknown: a failed lookup,
    /// or an account that plays offline (D157). Nothing may be removed then.
    pub(super) lookups_complete: bool,
    /// The entries whose lookup failed — not an answer, a network or Modrinth
    /// error. They are what holds the category's removals back, and the user
    /// is told so (todo.md:1691).
    pub(super) lookup_failures: Vec<String>,
}

/// The Modrinth loaders a category's versions are published under
/// (contentpack-8).
///
/// A project can publish a data pack and a mod under one slug — Terralith
/// does — and the version list asked by game version alone holds both. Picked
/// by channel and date, the mod's `.jar` won and was linked as a data pack.
pub(super) fn category_loaders(category: &str) -> &'static [&'static str] {
    match category {
        "datapack" => &["datapack"],
        "resourcepack" => &["minecraft"],
        "shader" => &["iris", "optifine", "canvas", "vanilla"],
        _ => &[],
    }
}

/// Whether a Modrinth version is a pack of this category: published under one
/// of its loaders, and not a `.jar`, which no pack directory can load.
pub(super) fn version_fits_category(category: &str, version: &ModrinthVersion) -> bool {
    let loaders = category_loaders(category);
    let loader_fits = version.loaders.iter().any(|loader| {
        loaders
            .iter()
            .any(|wanted| loader.eq_ignore_ascii_case(wanted))
    });
    let is_jar = version
        .primary_file()
        .is_some_and(|file| file.filename.to_ascii_lowercase().ends_with(".jar"));
    loader_fits && !is_jar
}

/// The versions of a list that belong to the category, in their order: the
/// filter both the launch and the pre-check apply before choosing.
pub(super) fn versions_for_category(
    category: &str,
    versions: Vec<ModrinthVersion>,
) -> Vec<ModrinthVersion> {
    versions
        .into_iter()
        .filter(|version| version_fits_category(category, version))
        .collect()
}

/// Resolve and download one category's packs, local and Modrinth, without
/// linking anything yet.
// Dieci input distinti di un passo del lancio, gli stessi delle due funzioni
// che la chiamano: il lint è sospeso solo qui, come lì (D120).
#[allow(clippy::too_many_arguments)]
async fn prepare_category_packs(
    app_handle: &tauri::AppHandle,
    http_client: &reqwest::Client,
    modrinth_client: &ModrinthClient,
    plan: &ContentPackPlan<'_>,
    modlist_dir: &Path,
    cache_dir: &Path,
    content_type: &str,
    mc_version: &str,
    active_entries: &[&ContentEntry],
    plays_offline: bool,
) -> Result<PreparedCategory> {
    let mut prepared = PreparedCategory {
        packs: Vec::new(),
        lookups_complete: true,
        lookup_failures: Vec::new(),
    };

    // A missing file is an answer, not a failure: it is logged and does not
    // freeze the category's removals, because we know exactly which pack is
    // gone. Nothing here touches the network.
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
        prepared.packs.push(PreparedPack {
            file_name: pack.file_name,
            source: PreparedPackSource::Local {
                source_path: pack.source_path,
            },
        });
    }

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

    // D157: with the account offline the local packs above are installed all
    // the same; only the Modrinth entries are not looked up. What they linked
    // last time is unknown to this launch, so it stays where it is.
    if plays_offline {
        if !modrinth_entry_ids.is_empty() {
            prepared.lookups_complete = false;
        }
        return Ok(prepared);
    }

    for resolution in resolve_content_entry_versions(
        modrinth_client,
        plan,
        content_type,
        mc_version,
        &modrinth_entry_ids,
    )
    .await
    {
        let entry_id = resolution.entry_id;
        if let Some(error) = resolution.lookup_error {
            // Not an answer: we do not know which file this entry wants, so
            // nothing may be removed on this pass.
            prepared.lookups_complete = false;
            prepared.lookup_failures.push(entry_id.clone());
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
                format!("[Content] No compatible version found for '{entry_id}' on {mc_version}"),
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

        prepared.packs.push(PreparedPack {
            file_name: file.filename.clone(),
            source: PreparedPackSource::Modrinth {
                entry_id,
                version_id: version.id.clone(),
                cached_path,
                was_cached,
            },
        });
    }

    Ok(prepared)
}

/// What [`link_prepared_packs`] did in one directory.
#[derive(Debug, Default)]
pub(super) struct LinkedPacks {
    /// What reached the directory: the set the sync compares the previous
    /// manifest against.
    pub(super) installed: crate::instance_content::InstalledContent,
    /// The packs left out because something the launcher did not create
    /// holds their name (D1, D103). The user is told, not only the log.
    pub(super) skipped: Vec<SkippedPack>,
    /// The error that stopped the linking halfway (contentpack-6). Whatever
    /// was linked before it is in `installed`, so the caller records it in
    /// the manifest before it lets the launch fail: a link the manifest does
    /// not name is, on the next launch, a file the launcher "did not create"
    /// — skipped, never updated, never removed, and twice in game after an
    /// update.
    pub(super) interrupted: Option<anyhow::Error>,
}

/// One pack [`link_prepared_packs`] did not install, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkippedPack {
    /// The entry id of a Modrinth pack, the file name of a local one.
    pub(super) pack: String,
    pub(super) path: PathBuf,
    /// `a file` or `a folder`: what sits under the pack's name.
    pub(super) what: &'static str,
}

/// Link prepared packs into one directory, applying the manifest rule on the
/// way in (D1), and return what reached it.
///
/// What is returned is the point of this function: `installed` is the set
/// [`crate::instance_content::sync_managed_content`] compares the previous
/// manifest against, with the origin of every Modrinth file and the shape of
/// every copied folder. A pack that is linked but left out of that set is
/// removed on the *next* launch — the wipe C1 closed, reappearing in a form
/// that takes two launches to notice. For the same reason an error does not
/// return early: it stops the loop and travels in `interrupted`, next to what
/// was already linked.
///
/// `label` is how the log names the directory (`resourcepacks`,
/// `saves/<world>/datapacks`).
pub(super) fn link_prepared_packs(
    log: &mut ContentLog<'_>,
    packs: &[PreparedPack],
    target_dir: &Path,
    previous: &crate::instance_content::PreviousContent,
    label: &str,
    kind: crate::instance_content::LinkKind,
) -> LinkedPacks {
    let mut linked = LinkedPacks::default();

    if !packs.is_empty() {
        if let Err(error) = std::fs::create_dir_all(target_dir)
            .with_context(|| format!("failed to create {}", target_dir.display()))
        {
            linked.interrupted = Some(error);
            return linked;
        }
    }

    for pack in packs {
        if let Err(error) = link_one_pack(log, pack, target_dir, previous, label, kind, &mut linked)
        {
            linked.interrupted = Some(error);
            break;
        }
    }

    linked
}

/// One step of [`link_prepared_packs`]: what it linked or skipped goes into
/// `linked` before the next fallible call.
fn link_one_pack(
    log: &mut ContentLog<'_>,
    pack: &PreparedPack,
    target_dir: &Path,
    previous: &crate::instance_content::PreviousContent,
    label: &str,
    kind: crate::instance_content::LinkKind,
    linked: &mut LinkedPacks,
) -> Result<()> {
    let target_path = target_dir.join(&pack.file_name);
    // A folder under the name is ours only if the manifest says we copied it
    // there (todo.md:1689); anything else, if the manifest lists the name.
    let target_is_ours = previous.owns_target(&pack.file_name, &target_path);

    match &pack.source {
        PreparedPackSource::Local { source_path } => {
            let outcome = link_local_pack_as(source_path, &target_path, target_is_ours, kind)
                .with_context(|| {
                    format!(
                        "failed to install local content pack '{}' into {label}",
                        pack.file_name
                    )
                })?;

            let how = match outcome {
                PackLinkOutcome::Linked => "",
                // Not silent: a copied folder is a second set of bytes on
                // disk, and whoever reads the log has to be able to tell why.
                PackLinkOutcome::Copied if kind == crate::instance_content::LinkKind::NoSymlink => {
                    " (copied: a world cannot hold a link to a folder)"
                }
                PackLinkOutcome::Copied => " (copied: this platform refused a directory link)",
                PackLinkOutcome::SkippedForeignDirectory | PackLinkOutcome::SkippedForeignFile => {
                    // Left out of `installed` on purpose: the name is not in
                    // the manifest, so the sync will not remove it either,
                    // and what the user put there stays exactly as it is.
                    let what = if outcome == PackLinkOutcome::SkippedForeignFile {
                        "a file"
                    } else {
                        "a folder"
                    };
                    linked.skipped.push(SkippedPack {
                        pack: pack.file_name.clone(),
                        path: target_path.clone(),
                        what,
                    });
                    return log(
                        ProcessLogStream::Stdout,
                        format!(
                            "[Content] Skipped local pack '{}': {} is {what} the launcher did not create",
                            pack.file_name,
                            target_path.display()
                        ),
                    );
                }
            };

            if outcome == PackLinkOutcome::Copied {
                linked.installed.add_copied_directory(&pack.file_name);
            } else {
                linked.installed.add(&pack.file_name, None);
            }
            log(
                ProcessLogStream::Stdout,
                format!("[Content] {} -> {label}{how}", pack.file_name),
            )
        }
        PreparedPackSource::Modrinth {
            entry_id,
            version_id,
            cached_path,
            was_cached,
        } => {
            let outcome = crate::instance_content::link_content_file_as(
                cached_path,
                &target_path,
                target_is_ours,
                kind,
            )
            .with_context(|| format!("failed to link content pack '{entry_id}' into {label}"))?;
            let what = match outcome {
                crate::instance_content::ContentLinkOutcome::Linked => None,
                crate::instance_content::ContentLinkOutcome::SkippedForeignFile => Some("a file"),
                // todo.md:1558: a real folder under the name used to fail the
                // launch; it is skipped like a file, and kept.
                crate::instance_content::ContentLinkOutcome::SkippedForeignDirectory => {
                    Some("a folder")
                }
            };
            if let Some(what) = what {
                // Left out of `installed`: the name is not in the manifest,
                // so the sync will not remove it either.
                linked.skipped.push(SkippedPack {
                    pack: entry_id.clone(),
                    path: target_path.clone(),
                    what,
                });
                return log(
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] Skipped '{entry_id}': {} is {what} the launcher did not create",
                        target_path.display()
                    ),
                );
            }

            linked.installed.add(
                &pack.file_name,
                Some(crate::instance_content::ManagedFileOrigin {
                    entry_id: entry_id.clone(),
                    version_id: version_id.clone(),
                }),
            );
            let cache_label = if *was_cached { " (cached)" } else { "" };
            log(
                ProcessLogStream::Stdout,
                format!("[Content] {entry_id} -> {label}{cache_label}"),
            )
        }
    }
}

/// D103: the packs a file or folder of the user's kept out of this launch, in
/// one notice the user sees. The log line alone left the pack missing in game
/// with no reason anywhere on screen (todo.md:1551).
pub(super) fn describe_skipped_packs(
    noun: &str,
    skipped: &[SkippedPack],
) -> Option<(String, String, String)> {
    if skipped.is_empty() {
        return None;
    }
    let names = skipped
        .iter()
        .map(|skip| format!("'{}'", skip.pack))
        .collect::<Vec<_>>()
        .join(", ");
    let places = skipped
        .iter()
        .map(|skip| format!("{} is {}", skip.path.display(), skip.what))
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        format!("Some {noun}s were not installed"),
        format!(
            "{names}: something the launcher did not create already sits under that name, so the pack was left out of this launch."
        ),
        format!(
            "{places}. The launcher never replaces a file or folder it did not put there. Move or rename it, and the next launch installs the pack."
        ),
    ))
}

/// todo.md:1691: a failed lookup holds back every removal of the category, so
/// a pack dropped from the list stays in the game. Said once per category,
/// with what stays, while it lasts.
pub(super) fn describe_frozen_removals(
    noun: &str,
    lookup_failures: &[String],
    frozen: &[String],
) -> Option<(String, String, String)> {
    if lookup_failures.is_empty() {
        return None;
    }
    let mut failures = lookup_failures.to_vec();
    failures.sort();
    failures.dedup();
    let mut frozen = frozen.to_vec();
    frozen.sort();
    frozen.dedup();
    let staying = if frozen.is_empty() {
        String::new()
    } else {
        format!(
            " These stay in the game although they are no longer in the list: {}.",
            frozen.join(", ")
        )
    };
    let mut title = format!("{noun} removals are on hold");
    // The nouns are ASCII; the title starts with a capital like the others.
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some((
        title,
        format!(
            "Modrinth could not be asked about {}, so this launch removed no {noun} from the instance.{staying}",
            failures.join(", ")
        ),
        format!(
            "Until those entries can be looked up again the launcher cannot tell which file they want, and it keeps every {noun} it installed before rather than delete one still in use. A project deleted from Modrinth never answers: remove its entry from the list to lift the hold."
        ),
    ))
}

/// Send one of the two notices above, if there is one.
fn emit_content_notice(
    app_handle: &tauri::AppHandle,
    notice: Option<(String, String, String)>,
) -> Result<()> {
    match notice {
        Some((title, message, detail)) => {
            emit_launcher_issue(app_handle, &title, &message, &detail, "warning", "launch")
        }
        None => Ok(()),
    }
}

/// The active entries of one category list. A missing file is an empty
/// list; one that cannot be read is an error (contentpack-11). It used to be
/// an empty list too, and the sync then removed every pack the launcher had
/// put in the instance for that category, saying so only in the log.
pub(super) fn active_category_entries(
    modlist_dir: &Path,
    content_type: &str,
    mc_version: &str,
    loader: &str,
) -> Result<Vec<ContentEntry>> {
    let list = load_content_list(modlist_dir, content_type)?;
    Ok(list
        .entries
        .into_iter()
        .filter(|entry| is_content_entry_active(entry, mc_version, loader))
        .collect())
}

/// How a sentence the user reads names one category's packs.
pub(super) fn category_noun(content_type: &str) -> &'static str {
    match content_type {
        "resourcepack" => "resource pack",
        "shader" => "shader",
        "datapack" => "data pack",
        _ => "content pack",
    }
}

/// A category list that cannot be read: the launch leaves that category as it
/// is — nothing installed, nothing removed — and says so as an error the user
/// sees, not only as a log line (contentpack-11, todo.md:1755).
fn report_unreadable_list(
    app_handle: &tauri::AppHandle,
    content_type: &str,
    error: &anyhow::Error,
) -> Result<()> {
    let noun = category_noun(content_type);
    let file = crate::content_packs::filename_for_type(content_type);
    emit_log(
        app_handle,
        ProcessLogStream::Stderr,
        format!("[Content] Could not read {file}; the {noun}s of the instance are left as they are: {error:#}"),
    )?;
    emit_launcher_issue(
        app_handle,
        &format!("Could not read the {noun} list"),
        &format!(
            "{file} of this mod list could not be read, so this launch left the instance's {noun}s exactly as they were: none installed, none removed."
        ),
        &format!("{error:#}. Fix or restore the file; the editor shows the same error on its tab."),
        "error",
        "launch",
    )
}

/// Resolve, download and install content packs into the instance.
///
/// `resolved_content` is the pre-check's answer (D58). Without it the launch
/// re-resolves from scratch, which is what it always did — and which is also
/// how a popup's answer used to be ignored in silence, since nothing carried
/// it here.
///
/// `plays_offline` (D157): the local packs are installed all the same, the
/// Modrinth ones are neither looked up nor installed, and what they linked on
/// the previous launch stays.
// Nove input distinti di un passo del lancio. Una struct di parametri
// cambierebbe i chiamanti del percorso di lancio, fuori da una modifica di
// solo stile (gate fmt/clippy, D120): il lint è sospeso solo qui.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_and_install_content_packs(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    http_client: &reqwest::Client,
    modrinth_client: &ModrinthClient,
    modlist_name: &str,
    target: &ResolutionTarget,
    instance_root: &Path,
    resolved_content: Option<&ResolvedContentVersions>,
    plays_offline: bool,
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

    let plan = if plays_offline {
        emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            "[Content] The active account plays offline: Modrinth packs are not looked up or installed, local packs are; the instance keeps the Modrinth packs of the previous launch".to_string(),
        )?;
        ContentPackPlan::unplanned()
    } else {
        // Degraded, never fatal: without the metadata every entry is resolved
        // the usual way, which is exactly the launch of before this field
        // existed.
        match ContentPackPlan::prepare(modrinth_client, resolved_content).await {
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
        // contentpack-3: Vanilla has no shader loader, so its instance gets
        // resource packs and data packs, and the shader list is left alone.
        if content_type == "shader" && target.mod_loader == ModLoader::Vanilla {
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                "[Content] Vanilla loads no shaders: shader packs are skipped".to_string(),
            )?;
            continue;
        }
        let entries =
            match active_category_entries(&modlist_dir, content_type, mc_version, loader_str) {
                Ok(entries) => entries,
                Err(error) => {
                    report_unreadable_list(app_handle, content_type, &error)?;
                    continue;
                }
            };
        let active_entries: Vec<&ContentEntry> = entries.iter().collect();
        let prepared = prepare_category_packs(
            app_handle,
            http_client,
            modrinth_client,
            &plan,
            &modlist_dir,
            cache_dir,
            content_type,
            mc_version,
            &active_entries,
            plays_offline,
        )
        .await?;

        let instance_dir = instance_root.join(instance_subdir);
        // What the previous launch linked here, from the manifest; for an
        // instance a launcher before the manifest set up, what is provably
        // its own (r2-aggiornamento-da-0.1.2-1). Only the difference with this
        // launch is removed; nothing else in the directory is touched.
        let previous = crate::instance_content::previous_content(
            instance_root,
            instance_subdir,
            &instance_dir,
            Some(cache_dir),
        );
        let linked = link_prepared_packs(
            &mut |stream, line| emit_log(app_handle, stream, line),
            &prepared.packs,
            &instance_dir,
            &previous,
            instance_subdir,
            crate::instance_content::LinkKind::Symlink,
        );

        // contentpack-6: what reached the directory goes into the manifest
        // even when the linking stopped halfway — as an incomplete set, so
        // nothing is removed on a pass that did not finish.
        let synced = crate::instance_content::sync_managed_content(
            instance_root,
            instance_subdir,
            &instance_dir,
            &previous,
            &linked.installed,
            prepared.lookups_complete && linked.interrupted.is_none(),
        );
        if let Some(error) = linked.interrupted {
            return Err(error);
        }
        let outcome = synced?;
        let noun = category_noun(content_type);
        emit_content_notice(app_handle, describe_skipped_packs(noun, &linked.skipped))?;
        emit_content_notice(
            app_handle,
            describe_frozen_removals(noun, &prepared.lookup_failures, &outcome.frozen),
        )?;
        for name in &outcome.kept_foreign_dirs {
            emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Content] Kept {name} in {instance_subdir}: it is a folder the launcher did not create, so it is never deleted"
                ),
            )?;
        }
        for name in outcome.removed {
            emit_log(
                app_handle,
                ProcessLogStream::Stdout,
                format!("[Content] Removed {} from {}", name, instance_subdir),
            )?;
        }
        for (name, error) in &outcome.failed_removals {
            emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Content] Could not remove {name} from {instance_subdir} ({error}); it stays, and the next launch tries again"
                ),
            )?;
        }
        // D145: an update renamed the file, and the game still calls the pack
        // by its old name. Never fatal: at worst the pack comes back disabled,
        // which is what happened before.
        match crate::instance_content::rewrite_renamed_pack_references(
            instance_root,
            instance_subdir,
            &outcome.renamed,
        ) {
            Ok(rewritten) => {
                for path in rewritten {
                    emit_log(
                        app_handle,
                        ProcessLogStream::Stdout,
                        format!(
                            "[Content] Updated {} for the renamed {}: {}",
                            path.strip_prefix(instance_root).unwrap_or(&path).display(),
                            instance_subdir,
                            describe_renames(&outcome.renamed)
                        ),
                    )?;
                }
            }
            Err(error) => emit_log(
                app_handle,
                ProcessLogStream::Stderr,
                format!(
                    "[Content] Could not point the game's settings at the renamed {instance_subdir} ({}): {error:#}",
                    describe_renames(&outcome.renamed)
                ),
            )?,
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
        plays_offline,
    )
    .await
}

fn describe_renames(renamed: &[(String, String)]) -> String {
    renamed
        .iter()
        .map(|(old, new)| format!("{old} -> {new}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Data packs go into `saves/<world>/datapacks/` of every world of the
/// instance, shared worlds included, at every launch (D144): the only place
/// Minecraft reads them from. Each world keeps its own manifest in
/// `<world>/.cubic/`, so a pack dropped from the list leaves every world it
/// was linked into, and a data pack the user put in a world by hand is never
/// touched. A world created in game gets them on the next launch. They are
/// hard links or copies, never symlinks: see
/// [`crate::instance_content::LinkKind::NoSymlink`].
///
/// Until D144 they were linked into `<instance>/datapacks/`, a folder nothing
/// reads; what the launcher put there is cleaned up and nothing else.
// Dieci input distinti di un passo del lancio. Una struct di parametri
// cambierebbe i chiamanti del percorso di lancio, fuori da una modifica di
// solo stile (gate fmt/clippy, D120): il lint è sospeso solo qui.
#[allow(clippy::too_many_arguments)]
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
    plays_offline: bool,
) -> Result<()> {
    match crate::instance_content::retire_legacy_content_dir(
        instance_root,
        LEGACY_DATAPACKS_DIR,
        cache_dir,
    ) {
        Ok(removed) => {
            for name in removed {
                emit_log(
                    app_handle,
                    ProcessLogStream::Stdout,
                    format!(
                        "[Content] Removed {name} from {LEGACY_DATAPACKS_DIR} (data packs now go into each world)"
                    ),
                )?;
            }
        }
        Err(error) => emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Content] Could not clean up the old {LEGACY_DATAPACKS_DIR} folder: {error:#}"
            ),
        )?,
    }

    let entries =
        match active_category_entries(modlist_dir, DATAPACK_CATEGORY, mc_version, loader_str) {
            Ok(entries) => entries,
            Err(error) => return report_unreadable_list(app_handle, DATAPACK_CATEGORY, &error),
        };
    let active_entries: Vec<&ContentEntry> = entries.iter().collect();
    let prepared = prepare_category_packs(
        app_handle,
        http_client,
        modrinth_client,
        plan,
        modlist_dir,
        cache_dir,
        DATAPACK_CATEGORY,
        mc_version,
        &active_entries,
        plays_offline,
    )
    .await?;

    let report = link_datapacks_into_worlds(
        &mut |stream, line| emit_log(app_handle, stream, line),
        instance_root,
        &prepared,
    )?;
    let noun = category_noun(DATAPACK_CATEGORY);
    emit_content_notice(app_handle, describe_skipped_packs(noun, &report.skipped))?;
    emit_content_notice(
        app_handle,
        describe_frozen_removals(noun, &prepared.lookup_failures, &report.frozen),
    )
}

/// What the data packs met in the worlds, for the notices of the launch.
#[derive(Debug, Default)]
pub(super) struct WorldDatapacksReport {
    /// Every world's skipped packs, each with its own path.
    pub(super) skipped: Vec<SkippedPack>,
    /// What a failed lookup kept, world by world (`saves/<world>: name`).
    pub(super) frozen: Vec<String>,
}

/// Link the prepared data packs into every world of the instance, each world
/// with its own manifest (D144).
pub(super) fn link_datapacks_into_worlds(
    log: &mut ContentLog<'_>,
    instance_root: &Path,
    prepared: &PreparedCategory,
) -> Result<WorldDatapacksReport> {
    let mut report = WorldDatapacksReport::default();
    let worlds = match crate::instance_content::instance_world_dirs(instance_root) {
        Ok(worlds) => worlds,
        Err(error) => {
            log(
                ProcessLogStream::Stderr,
                format!("[Content] Could not list the worlds for the data packs: {error:#}"),
            )?;
            return Ok(report);
        }
    };
    if worlds.is_empty() && !prepared.packs.is_empty() {
        log(
            ProcessLogStream::Stdout,
            format!(
                "[Content] No world in this instance yet: {} data pack(s) go into each world from the launch after it is created",
                prepared.packs.len()
            ),
        )?;
    }

    for world_dir in worlds {
        let world_name = world_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = format!("saves/{world_name}/{WORLD_DATAPACKS_DIR}");
        // One world that cannot be updated — a file held open on Windows, a
        // read-only folder — must not stop the launch nor the other worlds.
        if let Err(error) = sync_world_datapacks(log, &world_dir, prepared, &label, &mut report) {
            log(
                ProcessLogStream::Stderr,
                format!("[Content] Could not update the data packs of {label}: {error:#}"),
            )?;
        }
    }

    Ok(report)
}

/// Where a world keeps its data packs, and the category name of its manifest.
const WORLD_DATAPACKS_DIR: &str = "datapacks";

/// The instance-root folder data packs went to before D144.
const LEGACY_DATAPACKS_DIR: &str = "datapacks";

fn sync_world_datapacks(
    log: &mut ContentLog<'_>,
    world_dir: &Path,
    prepared: &PreparedCategory,
    label: &str,
    report: &mut WorldDatapacksReport,
) -> Result<()> {
    let datapacks_dir = world_dir.join(WORLD_DATAPACKS_DIR);
    // No adoption here: no launcher before D144 wrote into a world, so a file
    // in it that the manifest does not name is the user's.
    let previous = crate::instance_content::previous_content(
        world_dir,
        WORLD_DATAPACKS_DIR,
        &datapacks_dir,
        None,
    );
    let linked = link_prepared_packs(
        log,
        &prepared.packs,
        &datapacks_dir,
        &previous,
        label,
        crate::instance_content::LinkKind::NoSymlink,
    );
    // contentpack-6, as for the instance's own folders: recorded first.
    let synced = crate::instance_content::sync_managed_content(
        world_dir,
        WORLD_DATAPACKS_DIR,
        &datapacks_dir,
        &previous,
        &linked.installed,
        prepared.lookups_complete && linked.interrupted.is_none(),
    );
    if let Some(error) = linked.interrupted {
        return Err(error);
    }
    let outcome = synced?;
    report.skipped.extend(linked.skipped);
    report
        .frozen
        .extend(outcome.frozen.iter().map(|name| format!("{label}/{name}")));
    for name in &outcome.kept_foreign_dirs {
        log(
            ProcessLogStream::Stderr,
            format!(
                "[Content] Kept {name} in {label}: it is a folder the launcher did not create, so it is never deleted"
            ),
        )?;
    }
    for name in outcome.removed {
        log(
            ProcessLogStream::Stdout,
            format!("[Content] Removed {name} from {label}"),
        )?;
    }
    for (name, error) in &outcome.failed_removals {
        log(
            ProcessLogStream::Stderr,
            format!(
                "[Content] Could not remove {name} from {label} ({error}); it stays, and the next launch tries again"
            ),
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

        env::temp_dir().join(format!(
            "cubic-launcher-content-cache-test-{label}-{timestamp}"
        ))
    }

    /// A server that answers one fixed body and counts what it was asked for,
    /// so a test can tell "downloaded" from "served from the cache" by
    /// observing the requests instead of the file.
    fn counting_server(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener
            .local_addr()
            .expect("test server has an address")
            .port();
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
                recorder
                    .lock()
                    .expect("recorder")
                    .push(request_line.clone());

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
                    resolution
                        .version
                        .as_ref()
                        .map(|version| version.id.as_str()),
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
        assert_eq!(
            recorded.len(),
            1,
            "one request for three ids, got {recorded:?}"
        );
        assert!(
            recorded[0].contains("/versions?ids="),
            "the bulk metadata endpoint is the one asked, got {recorded:?}"
        );
        assert_eq!(plan.covered(), 2);
        assert_eq!(
            plan.dropped(),
            vec!["datapack/gone-from-modrinth".to_string()]
        );
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

    // ── contentpack-8: only packs of the category ───────────────────────────

    fn pack_version_json(
        version_id: &str,
        version_type: &str,
        date_published: &str,
        loaders: &[&str],
        filename: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": version_id,
            "project_id": "bPFzA8pz",
            "version_number": version_id,
            "name": version_id,
            "game_versions": ["1.20.1"],
            "loaders": loaders,
            "version_type": version_type,
            "files": [{
                "hashes": { "sha1": "0c4978ce9daa8343189cffb55f0ad0f096718f50" },
                "url": format!("http://example.invalid/{filename}"),
                "filename": filename,
                "primary": true,
                "size": 4,
            }],
            "date_published": date_published,
        })
    }

    fn pack_version(value: serde_json::Value) -> ModrinthVersion {
        serde_json::from_value(value).expect("the version payload should deserialize")
    }

    /// The shape of Terralith on Modrinth: one project, a data pack and a mod.
    /// Asked by game version alone the list holds both, and the newest
    /// release is the Fabric `.jar` — which used to land in `datapacks/`.
    #[tokio::test]
    async fn a_data_pack_entry_never_resolves_to_a_mod_jar() {
        let (base_url, _requests) = routing_server(vec![(
            "/project/terralith/version",
            serde_json::json!([
                pack_version_json(
                    "fabricJar",
                    "release",
                    "2026-08-01",
                    &["fabric", "quilt"],
                    "Terralith_1.20.x_v2.5.5.jar"
                ),
                pack_version_json(
                    "dataZip",
                    "release",
                    "2026-07-01",
                    &["datapack"],
                    "Terralith_1.20.x_v2.5.4.zip"
                ),
            ])
            .to_string(),
        )]);
        let client = ModrinthClient::with_base_url(base_url);

        let resolutions = resolve_content_entry_versions(
            &client,
            &ContentPackPlan::unplanned(),
            "datapack",
            "1.20.1",
            &["terralith"],
        )
        .await;

        assert_eq!(
            resolutions[0]
                .version
                .as_ref()
                .map(|version| version.id.as_str()),
            Some("dataZip")
        );
    }

    /// The same filter on a version the pre-check's map names: a `.jar` an
    /// older launch installed and a "Skip" kept is not installed again.
    #[tokio::test]
    async fn a_named_jar_is_not_installed_as_a_data_pack() {
        let (base_url, requests) = routing_server(vec![(
            "/project/terralith/version",
            serde_json::json!([pack_version_json(
                "dataZip",
                "release",
                "2026-07-01",
                &["datapack"],
                "Terralith.zip"
            )])
            .to_string(),
        )]);
        let client = ModrinthClient::with_base_url(base_url);
        let named = plan_map(&[("datapack", "terralith", "fabricJar")]);
        let metadata = HashMap::from([(
            "fabricJar".to_string(),
            pack_version(pack_version_json(
                "fabricJar",
                "release",
                "2026-08-01",
                &["fabric"],
                "Terralith.jar",
            )),
        )]);
        let plan = ContentPackPlan::new(Some(&named), metadata);

        let resolutions =
            resolve_content_entry_versions(&client, &plan, "datapack", "1.20.1", &["terralith"])
                .await;

        assert_eq!(
            resolutions[0]
                .version
                .as_ref()
                .map(|version| version.id.as_str()),
            Some("dataZip")
        );
        assert!(!resolutions[0].preresolved);
        assert_eq!(requests.lock().expect("recorder").len(), 1);
    }

    /// contentpack-11: an unreadable list stops the category, it does not
    /// empty it — an empty list made the sync remove every managed pack.
    #[test]
    fn an_unreadable_category_list_is_an_error_not_an_empty_list() {
        let modlist = unique_test_root("broken-list");
        fs::create_dir_all(&modlist).expect("mod-list dir");
        fs::write(modlist.join("shaders.json"), b"{ not json").expect("broken list");

        assert!(active_category_entries(&modlist, "shader", "1.20.1", "fabric").is_err());
        assert!(
            active_category_entries(&modlist, "resourcepack", "1.20.1", "fabric")
                .expect("a missing list is empty")
                .is_empty()
        );

        fs::remove_dir_all(&modlist).ok();
    }

    #[test]
    fn each_category_takes_its_own_loaders() {
        let resource = pack_version(pack_version_json(
            "rp",
            "release",
            "2026-01-01",
            &["minecraft"],
            "Pack.zip",
        ));
        let shader = pack_version(pack_version_json(
            "sh",
            "release",
            "2026-01-01",
            &["iris", "optifine"],
            "Shader.zip",
        ));
        let data = pack_version(pack_version_json(
            "dp",
            "release",
            "2026-01-01",
            &["datapack"],
            "Data.zip",
        ));
        // Loader right, file wrong: a `.jar` is no pack in any directory.
        let odd = pack_version(pack_version_json(
            "odd",
            "release",
            "2026-01-01",
            &["datapack"],
            "Data.JAR",
        ));

        assert!(version_fits_category("resourcepack", &resource));
        assert!(!version_fits_category("datapack", &resource));
        assert!(version_fits_category("shader", &shader));
        assert!(!version_fits_category("resourcepack", &shader));
        assert!(version_fits_category("datapack", &data));
        assert!(!version_fits_category("shader", &data));
        assert!(!version_fits_category("datapack", &odd));
    }

    // ── D144: data packs go into every world ────────────────────────────────

    fn make_world(dir: &Path) {
        fs::create_dir_all(dir).expect("world dir");
        fs::write(dir.join("level.dat"), b"nbt").expect("level.dat");
    }

    fn modrinth_pack(cache: &Path, entry_id: &str, version_id: &str, name: &str) -> PreparedPack {
        let dir = cache.join(version_id);
        fs::create_dir_all(&dir).expect("cache entry");
        fs::write(dir.join(name), version_id.as_bytes()).expect("cached pack");
        PreparedPack {
            file_name: name.to_string(),
            source: PreparedPackSource::Modrinth {
                entry_id: entry_id.to_string(),
                version_id: version_id.to_string(),
                cached_path: dir.join(name),
                was_cached: true,
            },
        }
    }

    fn local_pack(modlist: &Path, name: &str) -> PreparedPack {
        let dir = modlist.join("datapacks");
        fs::create_dir_all(&dir).expect("local packs dir");
        fs::write(dir.join(name), b"local").expect("local pack");
        PreparedPack {
            file_name: name.to_string(),
            source: PreparedPackSource::Local {
                source_path: dir.join(name),
            },
        }
    }

    fn link_into_worlds(
        instance_root: &Path,
        packs: Vec<PreparedPack>,
        complete: bool,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        link_datapacks_into_worlds(
            &mut |_, line| {
                lines.push(line);
                Ok(())
            },
            instance_root,
            &PreparedCategory {
                packs,
                lookups_complete: complete,
                lookup_failures: Vec::new(),
            },
        )
        .expect("linking into the worlds should succeed");
        lines
    }

    /// A temporary instance with two worlds — one of its own, one shared (a
    /// link to the canonical folder, as `shared_worlds` makes it) — and a
    /// folder in `saves/` that is no world. Both worlds get both packs, each
    /// with its own manifest; a pack dropped from the list leaves both; the
    /// data pack the user put in a world by hand stays; a world created later
    /// gets the packs on the next launch.
    #[cfg(unix)]
    #[test]
    fn data_packs_reach_every_world_and_leave_every_world() {
        let root = unique_test_root("datapacks-worlds");
        let cache = root.join("cache/content-packs");
        let modlist = root.join("mod-lists/Pack");
        let instance_root = modlist.join("instances/1.20.1-fabric");
        let own_world = instance_root.join("saves/Survival");
        let shared_world = root.join("worlds/Shared");
        make_world(&own_world);
        make_world(&shared_world);
        std::os::unix::fs::symlink(&shared_world, instance_root.join("saves/Shared"))
            .expect("the shared world's link");
        fs::create_dir_all(instance_root.join("saves/not a world")).expect("a stray folder");
        fs::create_dir_all(own_world.join("datapacks")).expect("the world's datapacks");
        fs::write(own_world.join("datapacks/hand-made.zip"), b"user").expect("user pack");

        let first = link_into_worlds(
            &instance_root,
            vec![
                modrinth_pack(&cache, "terralith", "VerA0001", "Terralith.zip"),
                local_pack(&modlist, "mine.zip"),
            ],
            true,
        );

        for world in [&own_world, &shared_world] {
            assert_eq!(
                fs::read(world.join("datapacks/Terralith.zip")).expect("linked"),
                b"VerA0001",
                "{} has the Modrinth pack",
                world.display()
            );
            assert!(world.join("datapacks/mine.zip").exists());
            for name in ["Terralith.zip", "mine.zip"] {
                assert!(
                    !fs::symlink_metadata(world.join("datapacks").join(name))
                        .expect("placed")
                        .file_type()
                        .is_symlink(),
                    "Minecraft refuses a world with a symlink inside: {name} must be a real file"
                );
            }
            let manifest = crate::instance_content::read_manifest(
                &crate::instance_content::manifest_path(world, "datapacks"),
            )
            .expect("each world has its own manifest");
            assert_eq!(
                manifest
                    .origins
                    .get("Terralith.zip")
                    .map(|o| o.version_id.as_str()),
                Some("VerA0001")
            );
        }
        assert!(
            !instance_root.join("saves/not a world/datapacks").exists(),
            "a folder without level.dat is no world"
        );
        assert!(
            first
                .iter()
                .any(|line| line == "[Content] terralith -> saves/Shared/datapacks (cached)"),
            "the log names the world, got {first:?}"
        );

        // The same packs again: our own hard links give way, nothing fails.
        link_into_worlds(
            &instance_root,
            vec![
                modrinth_pack(&cache, "terralith", "VerA0001", "Terralith.zip"),
                local_pack(&modlist, "mine.zip"),
            ],
            true,
        );
        assert!(own_world.join("datapacks/Terralith.zip").exists());

        // A world created in game since the last launch.
        let new_world = instance_root.join("saves/New World");
        make_world(&new_world);

        link_into_worlds(&instance_root, vec![local_pack(&modlist, "mine.zip")], true);

        for world in [&own_world, &shared_world] {
            assert!(
                fs::symlink_metadata(world.join("datapacks/Terralith.zip")).is_err(),
                "the dropped pack left {}",
                world.display()
            );
            assert!(world.join("datapacks/mine.zip").exists());
        }
        assert!(new_world.join("datapacks/mine.zip").exists());
        assert_eq!(
            fs::read(own_world.join("datapacks/hand-made.zip")).expect("the user's pack"),
            b"user",
            "a data pack the manifest does not name is never touched"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// D157 and a failed lookup look the same from a world: the Modrinth
    /// entries are unknown this launch, so what they linked stays.
    #[test]
    fn an_incomplete_launch_removes_nothing_from_the_worlds() {
        let root = unique_test_root("datapacks-incomplete");
        let cache = root.join("cache/content-packs");
        let modlist = root.join("mod-lists/Pack");
        let instance_root = modlist.join("instances/1.20.1-fabric");
        let world = instance_root.join("saves/Survival");
        make_world(&world);

        link_into_worlds(
            &instance_root,
            vec![modrinth_pack(
                &cache,
                "terralith",
                "VerA0001",
                "Terralith.zip",
            )],
            true,
        );
        link_into_worlds(
            &instance_root,
            vec![local_pack(&modlist, "mine.zip")],
            false,
        );

        assert!(world.join("datapacks/Terralith.zip").exists());
        assert!(world.join("datapacks/mine.zip").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_instance_without_worlds_says_when_the_packs_arrive() {
        let root = unique_test_root("datapacks-no-world");
        let modlist = root.join("mod-lists/Pack");
        let instance_root = modlist.join("instances/1.20.1-fabric");
        fs::create_dir_all(&instance_root).expect("instance");

        let lines = link_into_worlds(&instance_root, vec![local_pack(&modlist, "mine.zip")], true);

        assert!(
            lines
                .iter()
                .any(|line| line.contains("go into each world from the launch after it is created")),
            "got {lines:?}"
        );
        assert!(!instance_root.join("datapacks").exists());

        let _ = fs::remove_dir_all(&root);
    }

    /// An unpacked local data pack is a folder: copied into the world, never
    /// linked, and replaced by the next launch because the manifest names it.
    #[test]
    fn a_folder_data_pack_is_copied_into_the_world() {
        let root = unique_test_root("datapacks-folder");
        let modlist = root.join("mod-lists/Pack");
        let instance_root = modlist.join("instances/1.20.1-fabric");
        let world = instance_root.join("saves/Survival");
        make_world(&world);
        let folder = modlist.join("datapacks/my-pack");
        fs::create_dir_all(folder.join("data")).expect("pack folder");
        fs::write(folder.join("pack.mcmeta"), b"{}").expect("mcmeta");
        let pack = PreparedPack {
            file_name: "my-pack".to_string(),
            source: PreparedPackSource::Local {
                source_path: folder.clone(),
            },
        };

        let lines = link_into_worlds(&instance_root, vec![pack.clone()], true);
        link_into_worlds(&instance_root, vec![pack], true);

        let placed = world.join("datapacks/my-pack");
        let file_type = fs::symlink_metadata(&placed).expect("placed").file_type();
        assert!(file_type.is_dir() && !file_type.is_symlink());
        assert!(placed.join("pack.mcmeta").is_file());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("(copied: a world cannot hold a link to a folder)")),
            "got {lines:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // ── contentpack-6, todo.md:1551, :1558, :1691 ───────────────────────────

    fn link_into(
        dir: &Path,
        packs: &[PreparedPack],
        previous: &crate::instance_content::PreviousContent,
    ) -> LinkedPacks {
        link_prepared_packs(
            &mut |_, _| Ok(()),
            packs,
            dir,
            previous,
            "resourcepacks",
            crate::instance_content::LinkKind::Symlink,
        )
    }

    /// contentpack-6: a link that fails halfway leaves what was linked before
    /// it in the manifest, so the next launch still knows those files are
    /// its own — on Windows a hard link, which without the manifest reads as
    /// "a file the launcher did not create" forever after.
    #[test]
    fn a_launch_stopped_halfway_records_what_it_already_linked() {
        let root = unique_test_root("interrupted-link");
        let cache = root.join("cache/content-packs");
        let instance_root = root.join("instance");
        let instance_dir = instance_root.join("resourcepacks");
        let first = modrinth_pack(&cache, "fresh-animations", "VerA0001", "Fresh.zip");
        let broken = PreparedPack {
            file_name: "Gone.zip".to_string(),
            source: PreparedPackSource::Local {
                source_path: root.join("mod-list/resourcepacks/Gone.zip"),
            },
        };
        let last = modrinth_pack(&cache, "never-reached", "VerB0001", "Last.zip");
        let packs = vec![first.clone(), broken, last];

        let previous = crate::instance_content::previous_content(
            &instance_root,
            "resourcepacks",
            &instance_dir,
            None,
        );
        let linked = link_into(&instance_dir, &packs, &previous);

        assert!(linked.interrupted.is_some(), "the missing source stops it");
        assert_eq!(linked.installed.files, vec!["Fresh.zip".to_string()]);
        crate::instance_content::sync_managed_content(
            &instance_root,
            "resourcepacks",
            &instance_dir,
            &previous,
            &linked.installed,
            false,
        )
        .expect("the partial set is recorded");
        assert_eq!(
            crate::instance_content::read_manifest_files(&crate::instance_content::manifest_path(
                &instance_root,
                "resourcepacks"
            )),
            vec!["Fresh.zip".to_string()]
        );

        // What Windows without Developer Mode leaves: a hard link, a real file.
        fs::remove_file(instance_dir.join("Fresh.zip")).expect("link removed");
        fs::write(instance_dir.join("Fresh.zip"), b"VerA0001").expect("hard-link stand-in");
        let previous = crate::instance_content::previous_content(
            &instance_root,
            "resourcepacks",
            &instance_dir,
            None,
        );
        let relinked = link_into(&instance_dir, &[first], &previous);

        assert!(relinked.interrupted.is_none());
        assert!(relinked.skipped.is_empty(), "ours, not the user's");
        assert_eq!(relinked.installed.files, vec!["Fresh.zip".to_string()]);

        let _ = fs::remove_dir_all(&root);
    }

    /// D103 and todo.md:1558: a file or a folder of the user's under a pack's
    /// name skips that pack — never an error — and the skip is reported for
    /// the notice, not only logged.
    #[test]
    fn a_users_file_or_folder_under_a_pack_name_is_skipped_and_reported() {
        let root = unique_test_root("skipped-packs");
        let cache = root.join("cache/content-packs");
        let instance_dir = root.join("instance/resourcepacks");
        fs::create_dir_all(instance_dir.join("Folder.zip/assets")).expect("user folder");
        fs::write(instance_dir.join("File.zip"), b"user").expect("user file");
        let packs = vec![
            modrinth_pack(&cache, "by-file", "VerC0001", "File.zip"),
            modrinth_pack(&cache, "by-folder", "VerD0001", "Folder.zip"),
            modrinth_pack(&cache, "free", "VerE0001", "Free.zip"),
        ];

        let linked = link_into(
            &instance_dir,
            &packs,
            &crate::instance_content::PreviousContent::default(),
        );

        assert!(linked.interrupted.is_none(), "{:?}", linked.interrupted);
        assert_eq!(linked.installed.files, vec!["Free.zip".to_string()]);
        assert_eq!(
            linked
                .skipped
                .iter()
                .map(|skip| (skip.pack.as_str(), skip.what))
                .collect::<Vec<_>>(),
            vec![("by-file", "a file"), ("by-folder", "a folder")]
        );
        assert_eq!(
            fs::read(instance_dir.join("File.zip")).expect("user file"),
            b"user"
        );
        assert!(instance_dir.join("Folder.zip/assets").is_dir());

        let (title, message, detail) =
            describe_skipped_packs("resource pack", &linked.skipped).expect("a notice");
        assert_eq!(title, "Some resource packs were not installed");
        assert!(message.contains("'by-file'") && message.contains("'by-folder'"));
        assert!(detail.contains("Folder.zip is a folder"));
        assert!(describe_skipped_packs("resource pack", &[]).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    /// The same skip inside a world reaches the report the notice is built on.
    #[test]
    fn a_users_data_pack_in_a_world_is_reported_as_skipped() {
        let root = unique_test_root("skipped-world-pack");
        let cache = root.join("cache/content-packs");
        let instance_root = root.join("mod-lists/Pack/instances/1.20.1-fabric");
        let world = instance_root.join("saves/Survival");
        make_world(&world);
        fs::create_dir_all(world.join("datapacks")).expect("datapacks dir");
        fs::write(world.join("datapacks/Terralith.zip"), b"user").expect("user pack");

        let report = link_datapacks_into_worlds(
            &mut |_, _| Ok(()),
            &instance_root,
            &PreparedCategory {
                packs: vec![modrinth_pack(
                    &cache,
                    "terralith",
                    "VerA0001",
                    "Terralith.zip",
                )],
                lookups_complete: true,
                lookup_failures: Vec::new(),
            },
        )
        .expect("linking into the worlds should succeed");

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].pack, "terralith");
        assert_eq!(
            fs::read(world.join("datapacks/Terralith.zip")).expect("user pack"),
            b"user"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// todo.md:1691: the notice exists only when a lookup failed, and names
    /// what the hold keeps in the game.
    #[test]
    fn frozen_removals_are_announced_only_after_a_failed_lookup() {
        assert!(describe_frozen_removals("shader", &[], &["Old.zip".to_string()]).is_none());

        let (title, message, detail) = describe_frozen_removals(
            "shader",
            &["deleted-project".to_string()],
            &["Old.zip".to_string()],
        )
        .expect("a notice");
        assert_eq!(title, "Shader removals are on hold");
        assert!(message.contains("deleted-project"));
        assert!(message.contains("Old.zip"));
        assert!(detail.contains("remove its entry from the list"));
    }
}
