// The update pre-check: what a launch would change, computed without launching.
//
// It is a separate command because the launch cannot ask a question.
// `start_launch_command` (`launch_preview.rs:80-96`) spawns the pipeline and
// returns `Ok(())` immediately, so there is no suspended call to hold while a
// popup waits for an answer. The shape is the app updater's: `check_for_updates`
// returns a payload, the user decides, `install_update` applies
// (`updater.rs:58-74`, `:76-95`) — except that here the decision has to travel
// back into the launch, because a launch that re-picks its versions can change
// its mind between the popup and Play (D16).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::launcher_paths::LauncherPaths;
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{ModLoader, ResolutionTarget};
use crate::rules::ModSource;

use super::{
    emit_log, load_cached_version_ids_for_selected, load_modlist, parse_mod_loader,
    resolve_compatible_versions_hybrid, resolve_online_selection, run_content_precheck,
    ContentEntryWithoutVersions, ContentUpdateRow, ResolvedContentVersions, SelectedMod,
};

/// Same input as a launch: the mod-list and the target it is launched on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePrecheckRequest {
    pub modlist_name: String,
    pub minecraft_version: String,
    pub mod_loader: String,
}

/// One popup row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModUpdateRow {
    /// The mod actually loaded for this target. When a group of alternatives
    /// resolved through an alternative, this is the alternative and not its
    /// parent (D9) — it comes from the selection after the re-resolution, so
    /// there is nothing extra to compute here.
    pub mod_id: String,
    /// Canonical Modrinth project id, from the candidate version. The frontend
    /// already resolves icon and name from it through `fetchModMetadata`.
    pub project_id: String,
    pub current_version_id: String,
    /// `None` when `GET /versions?ids=` did not return the registered version
    /// — Modrinth no longer has it — or when that lookup failed altogether,
    /// which `version_number_lookup_error` distinguishes. The row is still an
    /// update either way: the two ids differ, and that is true whatever the
    /// label ends up saying.
    pub current_version_number: Option<String>,
    pub candidate_version_id: String,
    pub candidate_version_number: String,
}

/// The sets the popup and the launch need, and the distinction between them
/// is the whole point: `updates` is what to show, `resolved` is what to
/// install.
///
/// The content packs travel here and not in a second command (D57): a second
/// call would widen the double-click window the frontend guards, and the
/// launch starts once. They get their own fields rather than a `kind` on the
/// mod rows because the two halves are keyed differently — a mod by `mod_id`,
/// a pack by category *and* entry id — so one list would force a composite key
/// on both sides and a category that is meaningless for half the rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePrecheckResult {
    /// Only the mods whose candidate version differs from the one registered in
    /// `mod_cache`.
    pub updates: Vec<ModUpdateRow>,
    /// `mod_id → version_id` for **every** selected Modrinth mod, the unchanged
    /// ones included. The launch needs all of them to install exactly the
    /// versions that were shown without asking Modrinth again (D16), and a mod
    /// that was never downloaded is in here even though it is not an update
    /// (D17).
    pub resolved: HashMap<String, String>,
    /// Set when `GET /versions?ids=` failed, which costs the "from" labels and
    /// nothing else.
    ///
    /// The version numbers are a **separate failure domain** on purpose:
    /// `resolved` is what the launch needs (D16) and `mod_cache` alone answers
    /// which version is registered, so losing a cosmetic label must not abort
    /// the pre-check. One malformed version id would otherwise `400` the whole
    /// request and take the launch's version map down with it.
    pub version_number_lookup_error: Option<String>,
    /// The pack rows, all three categories in one list, each carrying its
    /// category so the popup can group them.
    pub content_updates: Vec<ContentUpdateRow>,
    /// `category → (entry id → version id)` for **every** selected Modrinth
    /// pack, same rule as `resolved` above: the unchanged and the
    /// never-installed included (D16, D17). Without it the launch re-resolves
    /// and the answer to the popup is lost in silence (D58).
    pub resolved_content: ResolvedContentVersions,
    /// Entries Modrinth has no version of for this target (D60). Not an error
    /// and not a row: today only `launcher.log` knows, and only during a
    /// launch.
    pub content_without_versions: Vec<ContentEntryWithoutVersions>,
}

/// A vanilla target loads no mods: an empty payload, not an error, and no
/// mod-list read at all. `None` means the target needs the full selection.
///
/// Empty covers the packs too, and that is not an omission: the vanilla
/// pipeline installs none (`launch_preview_vanilla.rs:31-35`, "skipping mod
/// resolution, mod downloads and content packs"), so a pack row would promise
/// something no launch would do.
pub(super) fn empty_precheck_for_loaderless_target(
    target: &ResolutionTarget,
) -> Option<UpdatePrecheckResult> {
    matches!(target.mod_loader, ModLoader::Vanilla).then(UpdatePrecheckResult::default)
}

/// The registered version ids a row would have to name, and whose version
/// *number* nothing local knows: `mod_cache` stores `modrinth_version_id` but
/// no version number (`database.rs:31-41`).
///
/// Ids equal to their candidate are left out because those mods produce no row,
/// and the result is sorted so the request is reproducible.
pub(super) fn version_ids_needing_a_number(
    cached_version_ids: &HashMap<String, String>,
    candidates: &HashMap<String, ModrinthVersion>,
) -> Vec<String> {
    let mut ids = cached_version_ids
        .iter()
        .filter(|(mod_id, cached_version_id)| {
            candidates
                .get(mod_id.as_str())
                .is_some_and(|candidate| &candidate.id != *cached_version_id)
        })
        .map(|(_, cached_version_id)| cached_version_id.clone())
        .collect::<Vec<_>>();

    ids.sort();
    ids.dedup();
    ids
}

/// The display side of the payload: the version numbers that were resolved,
/// and the reason there are none if the lookup failed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct VersionNumbers {
    pub(super) by_version_id: HashMap<String, String>,
    pub(super) lookup_error: Option<String>,
}

impl VersionNumbers {
    /// A failed lookup degrades to "no labels", never to "no updates": the
    /// version map the launch needs comes from `mod_cache` and the candidate
    /// lookups, both of which have already answered by the time this runs.
    pub(super) fn from_lookup(lookup: Result<HashMap<String, ModrinthVersion>>) -> Self {
        match lookup {
            Ok(versions) => Self {
                by_version_id: versions
                    .into_iter()
                    .map(|(version_id, version)| (version_id, version.version_number))
                    .collect(),
                lookup_error: None,
            },
            Err(error) => Self {
                by_version_id: HashMap::new(),
                lookup_error: Some(format!("{error:#}")),
            },
        }
    }
}

/// Build the payload from the maps that were already collected.
///
/// Pure on purpose: D9, D16 and D17 all live here, and this is what the tests
/// can exercise without a single request.
pub(super) fn build_precheck_result(
    selected_mods: &[SelectedMod],
    candidates: &HashMap<String, ModrinthVersion>,
    cached_version_ids: &HashMap<String, String>,
    cached_version_numbers: &VersionNumbers,
) -> UpdatePrecheckResult {
    let mut result = UpdatePrecheckResult {
        version_number_lookup_error: cached_version_numbers.lookup_error.clone(),
        ..UpdatePrecheckResult::default()
    };
    let mut seen = HashSet::new();

    for selected in selected_mods {
        if !matches!(selected.source, ModSource::Modrinth) {
            continue;
        }
        if !seen.insert(selected.mod_id.as_str()) {
            continue;
        }

        // No candidate means no compatible version on Modrinth at all. Such a
        // mod is not dropped any more — a jar in the cache for this target
        // keeps it (D29, both selection passes) — but the pre-check has no
        // version to promise for it: the cache is what decides, and the launch
        // acquires it from the row without a map entry.
        let Some(candidate) = candidates.get(&selected.mod_id) else {
            continue;
        };

        result
            .resolved
            .insert(selected.mod_id.clone(), candidate.id.clone());

        // D17: nothing registered for this target means a first install, not an
        // update. It stays in `resolved`, or it would never be installed.
        let Some(current_version_id) = cached_version_ids.get(&selected.mod_id) else {
            continue;
        };
        if current_version_id == &candidate.id {
            continue;
        }

        result.updates.push(ModUpdateRow {
            mod_id: selected.mod_id.clone(),
            project_id: candidate.project_id.clone(),
            current_version_id: current_version_id.clone(),
            current_version_number: cached_version_numbers
                .by_version_id
                .get(current_version_id)
                .cloned(),
            candidate_version_id: candidate.id.clone(),
            candidate_version_number: candidate.version_number.clone(),
        });
    }

    result
}

/// What a launch of this mod-list on this target would change.
///
/// Reads the database and Modrinth, writes nothing: no launch log session, no
/// download, no cache row. The update notification setting is deliberately not
/// consulted — it decides whether the popup appears, not whether the check
/// runs, and a mod that was never downloaded has to be resolved even with the
/// popup off or the launch has no version to install it from.
pub(super) async fn run_update_precheck(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    request: UpdatePrecheckRequest,
) -> Result<UpdatePrecheckResult> {
    let target = ResolutionTarget {
        minecraft_version: request.minecraft_version.trim().to_string(),
        mod_loader: parse_mod_loader(&request.mod_loader)?,
    };
    let modlist_name = request.modlist_name.trim().to_string();
    anyhow::ensure!(!modlist_name.is_empty(), "modlist_name cannot be empty");

    if let Some(empty) = empty_precheck_for_loaderless_target(&target) {
        return Ok(empty);
    }

    let modlist = load_modlist(launcher_paths, &modlist_name)?;
    let modrinth_client = ModrinthClient::new();
    let http_client = reqwest::Client::new();

    let selection = resolve_online_selection(
        app_handle,
        launcher_paths,
        &http_client,
        &modlist,
        &modrinth_client,
        &target,
        // The pre-check is the thing that decides: it has nothing handed down.
        None,
    )
    .await?;

    // The second pass, for the same reason the launch runs one: the
    // authoritative set is the one after the re-resolution, and it can contain
    // alternatives the first pass never saw.
    let candidates = resolve_compatible_versions_hybrid(
        app_handle,
        launcher_paths,
        &http_client,
        &selection.selected_mods,
        &modrinth_client,
        &target,
    )
    .await?;

    let cached_version_ids =
        load_cached_version_ids_for_selected(launcher_paths, &selection.selected_mods, &target)?;
    let wanted_numbers = version_ids_needing_a_number(&cached_version_ids, &candidates);
    // Degraded, not propagated: `resolved` is the part the launch cannot do
    // without (D16), and it is already complete here. One malformed version id
    // `400`s this whole request, so letting it fail the command would lose the
    // version map for a label.
    let cached_version_numbers = VersionNumbers::from_lookup(
        modrinth_client.fetch_versions_by_ids(&wanted_numbers).await,
    );
    if let Some(error) = &cached_version_numbers.lookup_error {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Precheck] version metadata lookup failed for {} cached versions; rows keep their ids and lose the current version number ({error})",
                wanted_numbers.len()
            ),
        );
    }

    let mut result = build_precheck_result(
        &selection.selected_mods,
        &candidates,
        &cached_version_ids,
        &cached_version_numbers,
    );

    // The packs last, and in the same payload: they need no mod-list read and
    // no database, so nothing above depends on them, and a pack lookup that
    // fails costs its own entry only.
    let content = run_content_precheck(
        app_handle,
        launcher_paths,
        &modrinth_client,
        &modlist_name,
        &target,
    )
    .await?;
    result.content_updates = content.updates;
    result.resolved_content = content.resolved;
    result.content_without_versions = content.without_versions;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::rules::{ModList, Rule, VersionRule, VersionRuleKind};

    use super::super::collect_selected_mods;
    use super::*;

    fn target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    fn modrinth_mod(mod_id: &str) -> SelectedMod {
        SelectedMod {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
        }
    }

    fn candidate(mod_id: &str, version_id: &str, version_number: &str) -> ModrinthVersion {
        ModrinthVersion {
            id: version_id.into(),
            project_id: format!("{mod_id}-project"),
            version_number: version_number.into(),
            name: version_number.into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            version_type: "release".into(),
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn candidates(entries: &[(&str, &str, &str)]) -> HashMap<String, ModrinthVersion> {
        entries
            .iter()
            .map(|(mod_id, version_id, version_number)| {
                (
                    (*mod_id).to_string(),
                    candidate(mod_id, version_id, version_number),
                )
            })
            .collect()
    }

    fn numbers(entries: &[(&str, &str)]) -> VersionNumbers {
        VersionNumbers {
            by_version_id: string_map(entries),
            lookup_error: None,
        }
    }

    fn string_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn resolved_pairs(result: &UpdatePrecheckResult) -> Vec<(String, String)> {
        let mut pairs = result
            .resolved
            .iter()
            .map(|(mod_id, version_id)| (mod_id.clone(), version_id.clone()))
            .collect::<Vec<_>>();
        pairs.sort();
        pairs
    }

    #[test]
    fn a_cache_that_already_holds_every_candidate_produces_no_row() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("lithium", "lithium-v7", "0.11.2"),
        ]);
        let cached = string_map(&[("sodium", "sodium-v2"), ("lithium", "lithium-v7")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

        assert!(result.updates.is_empty());
        assert_eq!(
            resolved_pairs(&result),
            vec![
                ("lithium".to_string(), "lithium-v7".to_string()),
                ("sodium".to_string(), "sodium-v2".to_string()),
            ]
        );
    }

    #[test]
    fn every_mod_whose_candidate_moved_becomes_a_row() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("lithium", "lithium-v7", "0.11.2"),
        ]);
        let cached = string_map(&[("sodium", "sodium-v1"), ("lithium", "lithium-v6")]);
        let numbers = numbers(&[("sodium-v1", "0.5.8"), ("lithium-v6", "0.11.1")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &numbers);

        assert_eq!(
            result
                .updates
                .iter()
                .map(|row| (
                    row.mod_id.as_str(),
                    row.current_version_number.as_deref(),
                    row.candidate_version_number.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("sodium", Some("0.5.8"), "0.6.0"),
                ("lithium", Some("0.11.1"), "0.11.2"),
            ]
        );
        assert_eq!(result.resolved.len(), 2);
    }

    #[test]
    fn a_row_carries_both_ids_and_the_modrinth_project_id() {
        let selected = vec![modrinth_mod("sodium")];
        let candidates = candidates(&[("sodium", "sodium-v2", "0.6.0")]);
        let cached = string_map(&[("sodium", "sodium-v1")]);
        let numbers = numbers(&[("sodium-v1", "0.5.8")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &numbers);

        assert_eq!(
            result.updates,
            vec![ModUpdateRow {
                mod_id: "sodium".into(),
                project_id: "sodium-project".into(),
                current_version_id: "sodium-v1".into(),
                current_version_number: Some("0.5.8".into()),
                candidate_version_id: "sodium-v2".into(),
                candidate_version_number: "0.6.0".into(),
            }]
        );
    }

    #[test]
    fn a_mixed_modlist_reports_only_the_mods_that_moved() {
        let selected = vec![
            modrinth_mod("sodium"),
            modrinth_mod("lithium"),
            modrinth_mod("iris"),
        ];
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("lithium", "lithium-v7", "0.11.2"),
            ("iris", "iris-v3", "1.7.3"),
        ]);
        let cached = string_map(&[
            ("sodium", "sodium-v1"),
            ("lithium", "lithium-v7"),
            ("iris", "iris-v2"),
        ]);
        let numbers = numbers(&[("sodium-v1", "0.5.8"), ("iris-v2", "1.7.0")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &numbers);

        assert_eq!(
            result
                .updates
                .iter()
                .map(|row| row.mod_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sodium", "iris"]
        );
        assert_eq!(result.resolved.len(), 3);
    }

    #[test]
    fn a_mod_without_a_cache_row_is_resolved_but_never_an_update() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("polytone")];
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("polytone", "polytone-v1", "2.0.0"),
        ]);
        let cached = string_map(&[("sodium", "sodium-v2")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

        assert!(result.updates.is_empty());
        assert_eq!(
            result.resolved.get("polytone").map(String::as_str),
            Some("polytone-v1")
        );
    }

    #[test]
    fn a_mod_with_no_candidate_is_in_neither_set() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("epic-fight")];
        let candidates = candidates(&[("sodium", "sodium-v2", "0.6.0")]);
        let cached = string_map(&[("sodium", "sodium-v1"), ("epic-fight", "epic-fight-v1")]);
        let numbers = numbers(&[("sodium-v1", "0.5.8")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &numbers);

        assert_eq!(result.updates.len(), 1);
        assert_eq!(result.updates[0].mod_id, "sodium");
        assert!(!result.resolved.contains_key("epic-fight"));
    }

    #[test]
    fn a_row_survives_a_cached_version_modrinth_no_longer_returns() {
        let selected = vec![modrinth_mod("sodium")];
        let candidates = candidates(&[("sodium", "sodium-v2", "0.6.0")]);
        let cached = string_map(&[("sodium", "sodium-v1")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

        assert_eq!(result.updates.len(), 1);
        assert_eq!(result.updates[0].current_version_id, "sodium-v1");
        assert_eq!(result.updates[0].current_version_number, None);
    }

    #[test]
    fn local_mods_stay_out_of_the_precheck() {
        let selected = vec![SelectedMod {
            mod_id: "optifine".into(),
            source: ModSource::Local,
        }];
        let candidates = candidates(&[("optifine", "optifine-v2", "1.0.1")]);
        let cached = string_map(&[("optifine", "optifine-v1")]);

        let result = build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

        assert!(result.updates.is_empty());
        assert!(result.resolved.is_empty());
    }

    #[test]
    fn a_mod_resolved_through_an_alternative_is_named_by_the_alternative() {
        // Embeddium only exists on forge, so on fabric/1.21.1 the primary loses
        // and the alternative is what the launch actually loads (D9).
        let modlist = ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: "Test".into(),
            rules: vec![Rule {
                mod_id: "embeddium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![VersionRule {
                    kind: VersionRuleKind::Only,
                    mc_versions: vec!["1.20.1".into()],
                    loader: "forge".into(),
                }],
                custom_configs: vec![],
                alternatives: vec![Rule {
                    mod_id: "sodium".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec![],
                    version_rules: vec![],
                    custom_configs: vec![],
                    alternatives: vec![],
                }],
            }],
        };
        let resolution = crate::resolver::resolve_modlist(&modlist, &target())
            .expect("resolution should succeed");
        let selected = collect_selected_mods(&modlist, &resolution, &target());

        let result = build_precheck_result(
            &selected,
            &candidates(&[("sodium", "sodium-v2", "0.6.0")]),
            &string_map(&[("sodium", "sodium-v1"), ("embeddium", "embeddium-v1")]),
            &numbers(&[("sodium-v1", "0.5.8")]),
        );

        assert_eq!(
            result
                .updates
                .iter()
                .map(|row| row.mod_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sodium"]
        );
        assert_eq!(resolved_pairs(&result), vec![("sodium".to_string(), "sodium-v2".to_string())]);
    }

    #[test]
    fn a_vanilla_target_has_nothing_to_check() {
        // The command path parses the loader string first, so the short-circuit
        // is only reachable if "vanilla" parses instead of erroring.
        let vanilla = ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: parse_mod_loader("vanilla").expect("vanilla must parse"),
        };

        assert_eq!(vanilla.mod_loader, ModLoader::Vanilla);
        assert_eq!(
            empty_precheck_for_loaderless_target(&vanilla),
            Some(UpdatePrecheckResult::default())
        );
        assert_eq!(empty_precheck_for_loaderless_target(&target()), None);
    }

    #[test]
    fn a_failed_number_lookup_keeps_every_row_and_names_itself() {
        let selected = vec![modrinth_mod("sodium"), modrinth_mod("lithium")];
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("lithium", "lithium-v7", "0.11.2"),
        ]);
        let cached = string_map(&[("sodium", "sodium-v1"), ("lithium", "lithium-v6")]);
        let failed = VersionNumbers::from_lookup(Err(anyhow::anyhow!(
            "Modrinth returned an error for a 2-version metadata lookup"
        )));

        let result = build_precheck_result(&selected, &candidates, &cached, &failed);

        // The labels are gone, the updates and the launch's version map are not.
        assert_eq!(result.updates.len(), 2);
        assert!(result
            .updates
            .iter()
            .all(|row| row.current_version_number.is_none()));
        assert_eq!(result.resolved.len(), 2);
        assert_eq!(
            result.version_number_lookup_error.as_deref(),
            Some("Modrinth returned an error for a 2-version metadata lookup")
        );
    }

    #[test]
    fn only_the_cached_versions_that_differ_are_asked_for_a_number() {
        let candidates = candidates(&[
            ("sodium", "sodium-v2", "0.6.0"),
            ("lithium", "lithium-v7", "0.11.2"),
        ]);
        let cached = string_map(&[
            ("sodium", "sodium-v1"),
            ("lithium", "lithium-v7"),
            ("polytone", "polytone-v1"),
        ]);

        assert_eq!(
            version_ids_needing_a_number(&cached, &candidates),
            vec!["sodium-v1".to_string()]
        );
    }

    #[test]
    fn serializes_the_exact_payload_contract_the_frontend_reads() {
        // A2 builds the popup against these names. Nothing else in this crate
        // reads them back, so a rename would otherwise only show up as an
        // empty popup at runtime.
        let result = UpdatePrecheckResult {
            updates: vec![ModUpdateRow {
                mod_id: "sodium".into(),
                project_id: "AANobbMI".into(),
                current_version_id: "sodium-v1".into(),
                current_version_number: None,
                candidate_version_id: "sodium-v2".into(),
                candidate_version_number: "0.6.0".into(),
            }],
            resolved: string_map(&[("sodium", "sodium-v2")]),
            version_number_lookup_error: Some("boom".into()),
            content_updates: vec![ContentUpdateRow {
                category: "resourcepack".into(),
                entry_id: "enhanced-boss-bars".into(),
                project_id: "U5SedJ9S".into(),
                current_version_id: "older".into(),
                current_version_number: "1.5".into(),
                candidate_version_id: "fzlnlUF3".into(),
                candidate_version_number: "1.6".into(),
            }],
            resolved_content: ResolvedContentVersions::from([(
                "resourcepack".to_string(),
                string_map(&[("enhanced-boss-bars", "fzlnlUF3")]),
            )]),
            content_without_versions: vec![ContentEntryWithoutVersions {
                category: "resourcepack".into(),
                entry_id: "visual-effects-plus".into(),
            }],
        };

        assert_eq!(
            serde_json::to_value(&result).expect("payload should serialize"),
            serde_json::json!({
                "updates": [{
                    "modId": "sodium",
                    "projectId": "AANobbMI",
                    "currentVersionId": "sodium-v1",
                    "currentVersionNumber": null,
                    "candidateVersionId": "sodium-v2",
                    "candidateVersionNumber": "0.6.0",
                }],
                "resolved": { "sodium": "sodium-v2" },
                "versionNumberLookupError": "boom",
                "contentUpdates": [{
                    "category": "resourcepack",
                    "entryId": "enhanced-boss-bars",
                    "projectId": "U5SedJ9S",
                    "currentVersionId": "older",
                    "currentVersionNumber": "1.5",
                    "candidateVersionId": "fzlnlUF3",
                    "candidateVersionNumber": "1.6",
                }],
                "resolvedContent": {
                    "resourcepack": { "enhanced-boss-bars": "fzlnlUF3" },
                },
                "contentWithoutVersions": [{
                    "category": "resourcepack",
                    "entryId": "visual-effects-plus",
                }],
            })
        );
    }
}
