// The content-pack side of the update pre-check.
//
// It answers in the **same** payload as the mod side (D57): a second command
// would open a second double-click window, and the frontend guard
// (`App.tsx:927`) protects one call, not two. The rows carry their category so
// one list can be grouped instead of sequenced through two modals.
//
// What is different from the mods, and it is the whole difficulty: there is no
// table that records which version of a pack is installed. `mod_cache` answers
// that for a mod; for a pack the only local trace is the file the launch
// linked into the instance. Since precheck-3 the manifest records, for every
// Modrinth file, the entry and the version id it was linked as
// (`instance_content.rs`); a manifest written before that only has names, and
// then the version id is read from the link target, which since C5 points at
// `cache/content-packs/<version id>/<filename>` (`content_pack_cache_path`).
// The chain is entry → its versions for this target → the installed id.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::content_packs::{load_content_list, ContentEntry, ContentList};
use crate::launcher_paths::LauncherPaths;
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::process_streaming::ProcessLogStream;
use crate::resolver::ResolutionTarget;

use super::{
    build_instance_root, emit_log, is_content_entry_active, preferred_version,
    versions_for_category, ResolvedContentVersions, CONTENT_CATEGORIES, DATAPACK_CATEGORY,
};

/// One content-pack row of the popup.
///
/// `current_version_number` is not optional, unlike `ModUpdateRow`'s: the
/// installed version is recognised *inside* the entry's own version list, so
/// the number arrives with the same response that produced the row. There is
/// no second lookup to fail, hence no pack counterpart to
/// `version_number_lookup_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentUpdateRow {
    /// `resourcepack`, `shader` or `datapack` — the key of
    /// [`super::ResolvedContentVersions`] and the grouping the popup needs.
    pub category: String,
    /// The Modrinth slug as it is written in the category's JSON file. Stable
    /// identity for the row, and the key the answer travels back under.
    pub entry_id: String,
    /// Canonical Modrinth project id, from the candidate version: the frontend
    /// resolves icon and name from it exactly as it does for a mod.
    pub project_id: String,
    pub current_version_id: String,
    pub current_version_number: String,
    pub candidate_version_id: String,
    pub candidate_version_number: String,
}

/// A selected entry Modrinth has no version of for this target (D60).
///
/// `visual-effects-plus` is in `resourcepacks.json` and installs nothing on
/// 1.20.1; until now only `launcher.log` said so, once, during a launch.
/// Reporting it is all this does — how to show it is phase 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentEntryWithoutVersions {
    pub category: String,
    pub entry_id: String,
}

/// A selected entry the pre-check could not read the versions of (D63).
///
/// Deliberately a **different** list from
/// [`ContentEntryWithoutVersions`]: "Modrinth has no version for this target"
/// is a fact about the pack, "I could not ask" is a fact about the network,
/// and telling the user the first when the second happened is the silence A5
/// closed for the mods, dressed up as an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentLookupFailure {
    pub category: String,
    pub entry_id: String,
    /// The error as it was logged, for the detail line of a UI notice.
    pub error: String,
}

/// What one category contributed to the payload.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CategoryPrecheck {
    pub(super) updates: Vec<ContentUpdateRow>,
    /// `entry id → version id` for **every** Modrinth entry of the category
    /// that has a version, the unchanged and the never-installed included
    /// (D16, D17).
    pub(super) resolved: HashMap<String, String>,
    pub(super) without_versions: Vec<ContentEntryWithoutVersions>,
}

/// The three categories together, in the shape `UpdatePrecheckResult` carries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ContentPrecheck {
    pub(super) updates: Vec<ContentUpdateRow>,
    pub(super) resolved: ResolvedContentVersions,
    pub(super) without_versions: Vec<ContentEntryWithoutVersions>,
    pub(super) lookup_failures: Vec<ContentLookupFailure>,
}

/// What one category of the instance has installed, as far as the launcher
/// can tell.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct InstalledPacks {
    /// Every Modrinth version id found, attributed to an entry or not.
    pub(super) version_ids: HashSet<String>,
    /// `entry id → version id`, for the files the manifest attributes: what a
    /// failed lookup keeps (contentpack-4).
    pub(super) by_entry: HashMap<String, String>,
}

/// The directories whose `.cubic/` manifest describes one category: the
/// instance root, and for data packs every world as well (D144). The instance
/// root stays in for data packs because a launch before D144 linked them
/// there, and the installed version is the one the user has.
pub(super) fn category_manifest_roots(category: &str, instance_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if category == DATAPACK_CATEGORY {
        roots.extend(
            crate::instance_content::instance_world_dirs(instance_root).unwrap_or_default(),
        );
    }
    roots.push(instance_root.to_path_buf());
    roots
}

/// The Modrinth versions this launcher linked into one category.
///
/// The manifest's origin is the answer when there is one (precheck-3): it is
/// written at link time and does not depend on the shape of the link, so it
/// holds for the hard link Windows without Developer Mode gets. A manifest
/// written before origins existed falls back to the link target, the only
/// place the version id was then: C5 put it in the cache path. A name whose
/// link cannot be followed contributes nothing — a pre-C5 flat path, an old
/// hard link, a real file the user dropped in. "Unknown" costs a row, never a
/// wrong one: the entry then looks like a first install, which is exactly
/// what D17 already covers.
pub(super) fn installed_content_versions(
    cache_dir: &Path,
    manifest_roots: &[PathBuf],
    instance_subdir: &str,
) -> InstalledPacks {
    let mut installed = InstalledPacks::default();

    for root in manifest_roots {
        let manifest_path = crate::instance_content::manifest_path(root, instance_subdir);
        let Some(manifest) = crate::instance_content::read_manifest(&manifest_path) else {
            continue;
        };
        let instance_dir = root.join(instance_subdir);

        for name in &manifest.files {
            if let Some(origin) = manifest.origins.get(name) {
                installed.version_ids.insert(origin.version_id.clone());
                installed
                    .by_entry
                    .entry(origin.entry_id.clone())
                    .or_insert_with(|| origin.version_id.clone());
                continue;
            }

            let Ok(link_target) = std::fs::read_link(instance_dir.join(name)) else {
                continue;
            };
            let Some(version_dir) = link_target.parent() else {
                continue;
            };
            // The one shape that means anything: `<cache dir>/<version id>/<file>`.
            // Anything else is not a version the launcher can name.
            if version_dir.parent() != Some(cache_dir) {
                continue;
            }
            let Some(version_id) = version_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            installed.version_ids.insert(version_id.to_string());
        }
    }

    installed
}

/// Build one category's rows and version map from what the network answered.
///
/// Pure on purpose, like `build_precheck_result`: D16, D17, D56 and D60 are all
/// decided here, and this is what the tests can exercise without a request.
///
/// An entry missing from `versions_by_entry` had a failed lookup and is no
/// row and no "no versions". When the manifest says which version of it is
/// installed, that version goes into `resolved`, so the launch keeps it
/// instead of looking it up again — what the popup promises with "the launch
/// keeps what you have" (contentpack-4). With nothing installed there is
/// nothing to keep, and the launch resolves it the usual way, which is the
/// mods' D19 degradation applied to a single entry.
pub(super) fn build_category_precheck(
    category: &str,
    entries: &[&ContentEntry],
    versions_by_entry: &HashMap<&str, Vec<ModrinthVersion>>,
    installed: &InstalledPacks,
) -> CategoryPrecheck {
    let mut result = CategoryPrecheck::default();
    let mut seen = HashSet::new();

    for entry in entries {
        // A `source: "local"` pack has no Modrinth versions and no update to
        // offer, the same exclusion the non-Modrinth mods get
        // (`launch_preview_precheck.rs:164-166`). Its id is a file name, not a
        // slug.
        if entry.source != "modrinth" || !seen.insert(entry.id.as_str()) {
            continue;
        }
        let Some(versions) = versions_by_entry.get(entry.id.as_str()) else {
            if let Some(version_id) = installed.by_entry.get(&entry.id) {
                result.resolved.insert(entry.id.clone(), version_id.clone());
            }
            continue;
        };

        // D60: the list has the entry, Modrinth has nothing for the target.
        let Some(candidate) = preferred_version(versions) else {
            result.without_versions.push(ContentEntryWithoutVersions {
                category: category.to_string(),
                entry_id: entry.id.clone(),
            });
            continue;
        };

        result
            .resolved
            .insert(entry.id.clone(), candidate.id.clone());

        // The installed version, attributed by the id the instance link names.
        // Version ids are unique to a project, so a hit inside this entry's own
        // list is the attribution: no hash, no file name matching, no
        // `POST /v2/version_files`.
        //
        // D17: nothing recognised means a first install, not an update. It
        // stays in `resolved` or it would never be installed.
        let Some(current) = versions
            .iter()
            .find(|version| installed.version_ids.contains(&version.id))
        else {
            continue;
        };
        if current.id == candidate.id {
            continue;
        }

        result.updates.push(ContentUpdateRow {
            category: category.to_string(),
            entry_id: entry.id.clone(),
            project_id: candidate.project_id.clone(),
            current_version_id: current.id.clone(),
            current_version_number: current.version_number.clone(),
            candidate_version_id: candidate.id.clone(),
            candidate_version_number: candidate.version_number.clone(),
        });
    }

    result
}

/// What a launch of this mod-list on this target would change for the packs.
///
/// One `GET /project/{id}/version` per active Modrinth entry — the same
/// request the launch makes today, and deliberately not the bulk
/// `POST /v2/version_files/update`: that endpoint is keyed by file hash and
/// answers only with *newer* versions, so it can neither name the version that
/// is installed now nor cover an entry that was never installed (D16). The
/// per-project list gives the candidate, the installed version and its number
/// in one response.
pub(super) async fn run_content_precheck(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    modrinth_client: &ModrinthClient,
    modlist_name: &str,
    target: &ResolutionTarget,
) -> Result<ContentPrecheck> {
    let modlist_dir = launcher_paths.modlists_dir().join(modlist_name);
    let instance_root = build_instance_root(launcher_paths, modlist_name, target)?;
    let cache_dir = launcher_paths.content_packs_cache_dir();
    let mc_version = &target.minecraft_version;
    let loader_str = target.mod_loader.as_modrinth_loader();

    let mut precheck = ContentPrecheck::default();

    for (category, instance_subdir) in CONTENT_CATEGORIES {
        // A missing category file is an empty list, not an error, exactly as
        // it is for the launch (`content_packs.rs:86-94`).
        let list = load_content_list(&modlist_dir, category).unwrap_or_else(|_| ContentList {
            content_type: category.to_string(),
            entries: vec![],
            groups: vec![],
        });
        let active_entries: Vec<&ContentEntry> = list
            .entries
            .iter()
            .filter(|entry| is_content_entry_active(entry, mc_version, loader_str))
            .collect();
        if active_entries.is_empty() {
            continue;
        }

        let installed = installed_content_versions(
            cache_dir,
            &category_manifest_roots(category, &instance_root),
            instance_subdir,
        );
        let mut versions_by_entry: HashMap<&str, Vec<ModrinthVersion>> = HashMap::new();
        for entry in active_entries
            .iter()
            .filter(|entry| entry.source == "modrinth")
        {
            if versions_by_entry.contains_key(entry.id.as_str()) {
                continue;
            }
            match modrinth_client
                .fetch_content_pack_versions(&entry.id, mc_version)
                .await
            {
                Ok(versions) => {
                    // contentpack-8: the same filter the launch applies, so
                    // the popup never offers a mod `.jar` as a pack update.
                    versions_by_entry
                        .insert(entry.id.as_str(), versions_for_category(category, versions));
                }
                Err(error) => {
                    // Left out of the rows and of the map — promising a
                    // version we could not read would be worse than letting
                    // the launch resolve it — but **not** left out of the
                    // payload: D63. The list it lands in is not the D60 one,
                    // because "no version exists" and "I could not ask" are
                    // different sentences to the user.
                    let error = format!("{error:#}");
                    let outcome = if installed.by_entry.contains_key(&entry.id) {
                        "the launch keeps the installed version"
                    } else {
                        "the launch will resolve it"
                    };
                    let _ = emit_log(
                        app_handle,
                        ProcessLogStream::Stderr,
                        format!(
                            "[Precheck] could not read the versions of '{}' ({category}); {outcome} ({error})",
                            entry.id
                        ),
                    );
                    precheck.lookup_failures.push(ContentLookupFailure {
                        category: category.to_string(),
                        entry_id: entry.id.clone(),
                        error,
                    });
                }
            }
        }

        let category_precheck =
            build_category_precheck(category, &active_entries, &versions_by_entry, &installed);

        precheck.updates.extend(category_precheck.updates);
        precheck
            .without_versions
            .extend(category_precheck.without_versions);
        if !category_precheck.resolved.is_empty() {
            precheck
                .resolved
                .insert(category.to_string(), category_precheck.resolved);
        }
    }

    Ok(precheck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn entry(id: &str, source: &str) -> ContentEntry {
        ContentEntry {
            id: id.into(),
            source: source.into(),
            version_rules: Vec::new(),
            name: None,
            file_name: None,
            icon_path: None,
            description: None,
        }
    }

    fn version(
        project_id: &str,
        version_id: &str,
        version_number: &str,
        version_type: &str,
        date_published: &str,
    ) -> ModrinthVersion {
        ModrinthVersion {
            id: version_id.into(),
            project_id: project_id.into(),
            version_number: version_number.into(),
            name: version_number.into(),
            game_versions: vec!["1.20.1".into()],
            loaders: vec!["minecraft".into()],
            version_type: version_type.into(),
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: date_published.into(),
        }
    }

    fn installed(version_ids: &[&str]) -> InstalledPacks {
        InstalledPacks {
            version_ids: version_ids
                .iter()
                .map(|version_id| (*version_id).to_string())
                .collect(),
            by_entry: HashMap::new(),
        }
    }

    fn resolved_pairs(result: &CategoryPrecheck) -> Vec<(String, String)> {
        let mut pairs = result
            .resolved
            .iter()
            .map(|(entry_id, version_id)| (entry_id.clone(), version_id.clone()))
            .collect::<Vec<_>>();
        pairs.sort();
        pairs
    }

    fn unique_test_root(label: &str) -> std::path::PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!(
            "cubic-launcher-precheck-content-{label}-{timestamp}"
        ))
    }

    /// D16 for the packs: the launch needs a version for every selected entry,
    /// not only for the ones that moved. The unchanged `boss-refreshed` and the
    /// never-installed `nature-x` are both in the map, and neither is a row.
    #[test]
    fn every_selected_pack_is_in_the_map_even_when_it_is_not_an_update() {
        let entries = [
            entry("boss-refreshed", "modrinth"),
            entry("nature-x", "modrinth"),
        ];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let versions = HashMap::from([
            (
                "boss-refreshed",
                vec![version(
                    "ZbZNRA1g",
                    "YItTWWdj",
                    "v2",
                    "release",
                    "2025-05-30",
                )],
            ),
            (
                "nature-x",
                vec![version(
                    "8d8M3Qoz",
                    "3c40Y4EH",
                    "12.2",
                    "release",
                    "2025-01-28",
                )],
            ),
        ]);

        let result = build_category_precheck(
            "resourcepack",
            &entries,
            &versions,
            &installed(&["YItTWWdj"]),
        );

        assert!(
            result.updates.is_empty(),
            "neither entry moved, so neither is a row"
        );
        assert_eq!(
            resolved_pairs(&result),
            vec![
                ("boss-refreshed".to_string(), "YItTWWdj".to_string()),
                ("nature-x".to_string(), "3c40Y4EH".to_string()),
            ],
            "D17: the never-installed entry is in the map or it is never installed"
        );
    }

    /// The row, with the left-hand side that only the installed version id can
    /// give: `enhanced-boss-bars` 1.5 in the instance, 1.6 on Modrinth.
    #[test]
    fn an_entry_whose_installed_version_moved_becomes_a_row() {
        let entries = [entry("enhanced-boss-bars", "modrinth")];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let versions = HashMap::from([(
            "enhanced-boss-bars",
            vec![
                version("U5SedJ9S", "olderrrr", "1.5", "release", "2025-11-02"),
                version("U5SedJ9S", "fzlnlUF3", "1.6", "release", "2026-06-13"),
            ],
        )]);

        let result = build_category_precheck(
            "resourcepack",
            &entries,
            &versions,
            &installed(&["olderrrr"]),
        );

        assert_eq!(
            result.updates,
            vec![ContentUpdateRow {
                category: "resourcepack".into(),
                entry_id: "enhanced-boss-bars".into(),
                project_id: "U5SedJ9S".into(),
                current_version_id: "olderrrr".into(),
                current_version_number: "1.5".into(),
                candidate_version_id: "fzlnlUF3".into(),
                candidate_version_number: "1.6".into(),
            }]
        );
    }

    /// D56 where it is visible: a beta published **after** a release does not
    /// win, and the row the popup shows is the release. Nothing filters the
    /// beta out of the list — it is still a candidate, it just ranks lower.
    #[test]
    fn a_newer_beta_does_not_outrank_a_release() {
        let entries = [entry("enhanced-boss-bars", "modrinth")];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let versions = HashMap::from([(
            "enhanced-boss-bars",
            vec![
                version("U5SedJ9S", "installed", "1.5", "release", "2025-11-02"),
                version("U5SedJ9S", "release16", "1.6", "release", "2026-06-13"),
                version("U5SedJ9S", "beta17", "1.7-beta", "beta", "2026-08-01"),
            ],
        )]);

        let result = build_category_precheck(
            "resourcepack",
            &entries,
            &versions,
            &installed(&["installed"]),
        );

        assert_eq!(
            result
                .updates
                .first()
                .map(|row| row.candidate_version_id.as_str()),
            Some("release16"),
            "the newest by date is the beta; the preferred one is the release"
        );
    }

    /// The other half of D56, and the reason a channel *filter* would be wrong:
    /// `fresh-animations` has published 17 betas and zero releases. Ranking
    /// keeps it; filtering would delete it from the user's list.
    #[test]
    fn a_project_that_only_publishes_betas_still_resolves() {
        let entries = [entry("fresh-animations", "modrinth")];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let versions = HashMap::from([(
            "fresh-animations",
            vec![
                version("50dA9Sha", "older", "1.10.3", "beta", "2025-12-01"),
                version("50dA9Sha", "xN57JJts", "1.10.4", "beta", "2026-02-24"),
            ],
        )]);

        let result =
            build_category_precheck("resourcepack", &entries, &versions, &installed(&["older"]));

        assert_eq!(
            resolved_pairs(&result),
            vec![("fresh-animations".to_string(), "xN57JJts".to_string())]
        );
        assert_eq!(
            result
                .updates
                .first()
                .map(|row| row.candidate_version_number.as_str()),
            Some("1.10.4")
        );
    }

    /// D60: `visual-effects-plus` is in `resourcepacks.json` and has no version
    /// for 1.20.1. It is not a row, it is not in the map — the launch would
    /// install nothing for it — and the pre-check now says so out loud.
    /// A local pack and an entry whose lookup failed are in the same list to
    /// prove they land nowhere at all.
    #[test]
    fn an_entry_without_versions_is_reported_and_the_silent_ones_stay_silent() {
        let entries = [
            entry("visual-effects-plus", "modrinth"),
            entry("Faithful 32x.zip", "local"),
            entry("boss-refreshed", "modrinth"),
        ];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        // `boss-refreshed` is absent from the map: its lookup failed.
        let versions = HashMap::from([("visual-effects-plus", Vec::new())]);

        let result = build_category_precheck("resourcepack", &entries, &versions, &installed(&[]));

        assert_eq!(
            result.without_versions,
            vec![ContentEntryWithoutVersions {
                category: "resourcepack".into(),
                entry_id: "visual-effects-plus".into(),
            }]
        );
        assert!(result.updates.is_empty());
        assert!(
            result.resolved.is_empty(),
            "a local pack and a failed lookup promise nothing"
        );
    }

    /// The measurement the popup's left-hand side depends on: from the manifest
    /// name to the link, from the link to the version id in the cache path.
    /// The three shapes that are not that path answer "unknown" instead of
    /// answering wrong.
    #[test]
    fn the_installed_version_id_is_read_from_the_instance_link() {
        let root = unique_test_root("installed-ids");
        let cache_dir = root.join("cache/content-packs");
        let instance_root = root.join("instances/1.20.1-forge");
        let instance_dir = instance_root.join("resourcepacks");
        fs::create_dir_all(&instance_dir).expect("instance dir");
        fs::create_dir_all(instance_root.join(".cubic")).expect("manifest dir");

        // The C5 layout: one directory per version id. Linked with the very
        // function the launch links with, so the chain under test is the real
        // one and not a symlink written by the test.
        let versioned = cache_dir.join("fzlnlUF3");
        fs::create_dir_all(&versioned).expect("cache entry");
        fs::write(versioned.join("[1.6] Enhanced Boss Bars.zip"), b"pack").expect("cached file");
        crate::instance_mods::create_file_link(
            &versioned.join("[1.6] Enhanced Boss Bars.zip"),
            &instance_dir.join("[1.6] Enhanced Boss Bars.zip"),
        )
        .expect("link the pack into the instance");

        // A pre-C5 flat file in the cache root: no version id to read.
        fs::write(cache_dir.join("Nature X.zip"), b"pack").expect("flat cached file");
        crate::instance_mods::create_file_link(
            &cache_dir.join("Nature X.zip"),
            &instance_dir.join("Nature X.zip"),
        )
        .expect("link the legacy pack");

        // A real file the user dropped in, listed in the manifest all the same.
        fs::write(instance_dir.join("Hand made.zip"), b"pack").expect("user file");

        fs::write(
            crate::instance_content::manifest_path(&instance_root, "resourcepacks"),
            r#"{"version":1,"category":"resourcepacks","files":["[1.6] Enhanced Boss Bars.zip","Nature X.zip","Hand made.zip","never linked.zip"]}"#,
        )
        .expect("manifest");

        let found = installed_content_versions(
            &cache_dir,
            std::slice::from_ref(&instance_root),
            "resourcepacks",
        );

        assert_eq!(
            found,
            installed(&["fzlnlUF3"]),
            "only the versioned cache path names a version"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// precheck-3: on Windows without Developer Mode the link is a hard link,
    /// with no target to read. The manifest's origin names the version all
    /// the same — here with `fs::hard_link`, the shape the fallback leaves.
    #[test]
    fn a_hard_linked_pack_is_recognised_through_the_manifest() {
        let root = unique_test_root("hard-link");
        let cache_dir = root.join("cache/content-packs");
        let instance_root = root.join("instances/1.20.1-forge");
        let instance_dir = instance_root.join("shaderpacks");
        fs::create_dir_all(&instance_dir).expect("instance dir");
        fs::create_dir_all(instance_root.join(".cubic")).expect("manifest dir");
        let versioned = cache_dir.join("olderrrr");
        fs::create_dir_all(&versioned).expect("cache entry");
        fs::write(versioned.join("BSL.zip"), b"pack").expect("cached file");
        fs::hard_link(versioned.join("BSL.zip"), instance_dir.join("BSL.zip"))
            .expect("the fallback's hard link");
        fs::write(
            crate::instance_content::manifest_path(&instance_root, "shaderpacks"),
            r#"{"version":1,"category":"shaderpacks","files":["BSL.zip"],"origins":{"BSL.zip":{"entryId":"bsl-shaders","versionId":"olderrrr"}}}"#,
        )
        .expect("manifest");

        let found = installed_content_versions(
            &cache_dir,
            std::slice::from_ref(&instance_root),
            "shaderpacks",
        );

        assert_eq!(found.version_ids, installed(&["olderrrr"]).version_ids);
        assert_eq!(
            found.by_entry.get("bsl-shaders").map(String::as_str),
            Some("olderrrr")
        );

        // And so the row exists, which it never did with read_link alone.
        let entries = [entry("bsl-shaders", "modrinth")];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let versions = HashMap::from([(
            "bsl-shaders",
            vec![
                version("Q1vvjJYV", "olderrrr", "8.2.08", "release", "2025-01-01"),
                version("Q1vvjJYV", "newerrrr", "8.2.09", "release", "2026-01-01"),
            ],
        )]);
        let result = build_category_precheck("shader", &entries, &versions, &found);
        assert_eq!(
            result
                .updates
                .iter()
                .map(|row| row.current_version_id.as_str())
                .collect::<Vec<_>>(),
            vec!["olderrrr"]
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// contentpack-4: «the launch keeps what you have». A failed lookup is no
    /// row, but the installed version goes into the map, so the launch
    /// installs that one instead of asking Modrinth again. An entry with
    /// nothing installed still promises nothing.
    #[test]
    fn a_failed_lookup_keeps_the_installed_version_in_the_map() {
        let entries = [
            entry("nature-x", "modrinth"),
            entry("never-installed", "modrinth"),
        ];
        let entries: Vec<&ContentEntry> = entries.iter().collect();
        let mut installed = installed(&["3c40Y4EH"]);
        installed
            .by_entry
            .insert("nature-x".to_string(), "3c40Y4EH".to_string());

        let result = build_category_precheck("resourcepack", &entries, &HashMap::new(), &installed);

        assert!(result.updates.is_empty());
        assert!(result.without_versions.is_empty());
        assert_eq!(
            resolved_pairs(&result),
            vec![("nature-x".to_string(), "3c40Y4EH".to_string())]
        );
    }

    /// D144: the data packs' manifests live in the worlds, and the pre-check
    /// reads them there.
    #[test]
    fn the_installed_data_packs_are_read_from_the_worlds() {
        let root = unique_test_root("world-datapacks");
        let instance_root = root.join("instances/1.20.1-fabric");
        let world = instance_root.join("saves/Survival");
        fs::create_dir_all(world.join(".cubic")).expect("world manifest dir");
        fs::write(world.join("level.dat"), b"nbt").expect("level.dat");
        fs::write(
            crate::instance_content::manifest_path(&world, "datapacks"),
            r#"{"version":1,"category":"datapacks","files":["Terralith.zip"],"origins":{"Terralith.zip":{"entryId":"terralith","versionId":"dataZip1"}}}"#,
        )
        .expect("world manifest");

        let roots = category_manifest_roots("datapack", &instance_root);
        let found = installed_content_versions(&root.join("cache"), &roots, "datapacks");

        assert_eq!(roots, vec![world.clone(), instance_root.clone()]);
        assert_eq!(
            found.by_entry.get("terralith").map(String::as_str),
            Some("dataZip1")
        );
        assert_eq!(
            category_manifest_roots("resourcepack", &instance_root),
            vec![instance_root.clone()]
        );

        let _ = fs::remove_dir_all(&root);
    }
}
