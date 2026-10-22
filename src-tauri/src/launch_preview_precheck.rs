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
    active_account_plays_offline, dropped_reason_clause, dropped_reason_detail, emit_log,
    load_cached_version_ids_for_selected, load_modlist, load_modlist_pins, parse_mod_loader,
    resolve_compatible_versions_hybrid, resolve_offline_selection, resolve_online_selection,
    run_content_precheck, ContentEntryWithoutVersions, ContentLookupFailure, ContentUpdateRow,
    DroppedMod, ResolvedContentVersions, SelectedMod,
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

/// The mods a launch will leave out for one reason, said the way the launch
/// banner says it (`dropped_reason_clause`, `dropped_reason_detail`), so that
/// the question before the launch and the record after it cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedModGroup {
    /// The ids as the mod-list rules write them. Nothing local knows a
    /// readable name for a mod that was never downloaded (D116): the frontend
    /// names what its in-memory metadata already holds and shows the id for
    /// the rest.
    pub mod_ids: Vec<String>,
    /// Why they are out, as a clause: "this launch had no version chosen for
    /// them and …".
    pub reason: String,
    /// The cause, then what it costs.
    pub detail: String,
}

/// One group per reason, in the order the reasons first appear in `dropped`.
///
/// Shared by every branch that has a selection: the offline branch fills the
/// field today, and the online one would be one assignment from
/// `selection.dropped`.
pub(super) fn dropped_mod_groups(
    dropped: &[DroppedMod],
    target: &ResolutionTarget,
) -> Vec<DroppedModGroup> {
    let mut reasons = Vec::new();
    for lost in dropped {
        if !reasons.contains(&lost.reason) {
            reasons.push(lost.reason);
        }
    }

    reasons
        .into_iter()
        .map(|reason| {
            let mod_ids = dropped
                .iter()
                .filter(|lost| lost.reason == reason)
                .map(|lost| lost.mod_id.clone())
                .collect::<Vec<_>>();
            let many = mod_ids.len() > 1;
            DroppedModGroup {
                reason: dropped_reason_clause(reason, target, many),
                detail: dropped_reason_detail(reason, many),
                mod_ids,
            }
        })
        .collect()
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
    /// Entries whose version lookup failed (D63). Not the same list as the one
    /// above and not the same sentence: the launch will resolve these the
    /// usual way, and until it does nothing is known about them.
    pub content_lookup_failures: Vec<ContentLookupFailure>,
    /// The mods the launch will leave out, with nothing in their place (D116):
    /// said here so the user hears it **before** the game starts and chooses
    /// whether to start it. Filled on the offline branch only; the launch
    /// still reports them on its banner (`report_dropped_mods`), which is the
    /// only channel left when this pre-check fails.
    pub dropped_mods: Vec<DroppedModGroup>,
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

/// The pre-check of an account that plays offline, or `None` when the active
/// account plays online.
///
/// D115, D116: the same zero-request selection the offline launch makes
/// (`resolve_offline_selection` with no map), read for what it leaves out.
/// Everything else stays empty, `resolved` above all: the version map comes
/// from the network by construction, and an offline launch does not follow
/// one (`LaunchResolutionPath::for_active_account`). Synchronous and handed
/// no client, on purpose: the shape is half of the zero-request guarantee.
pub(super) fn offline_precheck_for_active_account(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    target: &ResolutionTarget,
) -> Result<Option<UpdatePrecheckResult>> {
    if !active_account_plays_offline(launcher_paths)? {
        return Ok(None);
    }

    let modlist = load_modlist(launcher_paths, modlist_name)?;
    let pins = load_modlist_pins(launcher_paths, modlist_name, &modlist);
    let selection = resolve_offline_selection(launcher_paths, &modlist, &pins, target, None)?;

    Ok(Some(UpdatePrecheckResult {
        dropped_mods: dropped_mod_groups(&selection.dropped, target),
        ..UpdatePrecheckResult::default()
    }))
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

    // D115, D116: an account that plays offline launches with zero requests,
    // so its pre-check makes none either. It reads the cache for the mods the
    // launch will leave out, and that is all it returns: no updates and no
    // map, and the launch reads the same row and takes the offline path by
    // itself.
    if let Some(offline) =
        offline_precheck_for_active_account(launcher_paths, &modlist_name, &target)?
    {
        let _ = emit_log(
            app_handle,
            ProcessLogStream::Stdout,
            "[Launcher] The active account is set to play offline; the pre-check reads the local cache only: no requests, no update check"
                .to_string(),
        );
        return Ok(offline);
    }

    let modlist = load_modlist(launcher_paths, &modlist_name)?;
    let pins = load_modlist_pins(launcher_paths, &modlist_name, &modlist);
    let modrinth_client = ModrinthClient::new();
    let http_client = crate::modrinth::build_download_client();

    let selection = resolve_online_selection(
        app_handle,
        launcher_paths,
        &http_client,
        &modlist,
        &pins,
        &modrinth_client,
        &target,
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
    let cached_version_numbers =
        VersionNumbers::from_lookup(modrinth_client.fetch_versions_by_ids(&wanted_numbers).await);
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
    result.content_lookup_failures = content.lookup_failures;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::resolver::PinAliases;
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

        let result =
            build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

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

        let result =
            build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

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

        let result =
            build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

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

        let result =
            build_precheck_result(&selected, &candidates, &cached, &VersionNumbers::default());

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
        assert_eq!(
            resolved_pairs(&result),
            vec![("sodium".to_string(), "sodium-v2".to_string())]
        );
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
            content_lookup_failures: vec![ContentLookupFailure {
                category: "shader".into(),
                entry_id: "complementary-reimagined".into(),
                error: "Modrinth returned an error".into(),
            }],
            dropped_mods: vec![DroppedModGroup {
                mod_ids: vec!["lithium".into()],
                reason: "your cache has no jar".into(),
                detail: "Nothing took its place.".into(),
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
                "contentLookupFailures": [{
                    "category": "shader",
                    "entryId": "complementary-reimagined",
                    "error": "Modrinth returned an error",
                }],
                "droppedMods": [{
                    "modIds": ["lithium"],
                    "reason": "your cache has no jar",
                    "detail": "Nothing took its place.",
                }],
            })
        );
    }

    /// A launcher root with an active account, a two-mod list and a cache that
    /// holds a row for `sodium` only: `lithium` has no row and no jar for the
    /// target, which is exactly what the offline selection leaves out.
    fn offline_root(label: &str, plays_offline: bool) -> LauncherPaths {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let launcher_paths = LauncherPaths::new(std::env::temp_dir().join(format!(
            "cubic-offline-precheck-{label}-{}-{timestamp}",
            std::process::id()
        )));
        std::fs::create_dir_all(launcher_paths.mods_cache_dir()).expect("mods cache directory");
        crate::database::initialize_database(launcher_paths.database_path())
            .expect("initialize database");

        let connection =
            rusqlite::Connection::open(launcher_paths.database_path()).expect("open database");
        let accounts = crate::microsoft_auth::AccountsRepository::new(&connection);
        accounts
            .upsert_account(&crate::microsoft_auth::AccountRecord {
                microsoft_id: "microsoft-id".into(),
                xbox_gamertag: Some("StoredPlayer".into()),
                minecraft_uuid: Some("123456781234123412341234567890ab".into()),
                access_token_enc: None,
                refresh_token_enc: None,
                profile_data: None,
                is_active: true,
                plays_offline: false,
            })
            .expect("insert active account");
        accounts
            .set_plays_offline("microsoft-id", plays_offline)
            .expect("save the choice");

        let mut cached = candidate("sodium", "sodium-v2", "0.6.0");
        cached.project_id = "sodium".into();
        cached.files = vec![crate::modrinth::ModrinthFile {
            hashes: HashMap::from([("sha1".to_string(), "sodium-v2-sha1".to_string())]),
            url: "https://cdn.modrinth.com/data/sodium/sodium.jar".into(),
            filename: "sodium.jar".into(),
            primary: true,
            size: 100,
        }];
        crate::mod_cache::SqliteModCacheRepository::new(
            &connection,
            launcher_paths.mods_cache_dir(),
        )
        .upsert_modrinth_version(&cached, &target())
        .expect("register sodium for the target");

        let rule = |mod_id: &str| Rule {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        };
        let modlist_dir = launcher_paths.modlists_dir().join("Offline Pack");
        std::fs::create_dir_all(&modlist_dir).expect("mod-list directory");
        ModList {
            modlist_name: "Offline Pack".into(),
            author: "Author".into(),
            description: "Test".into(),
            rules: vec![rule("sodium"), rule("lithium")],
        }
        .write_to_file(&modlist_dir.join(crate::rules::RULES_FILENAME))
        .expect("write the mod-list");

        launcher_paths
    }

    /// D116: with the active account offline, the pre-check names the mods
    /// the launch will leave out — and carries no version map, because the
    /// map comes from the network and an offline launch does not follow one.
    ///
    /// `sodium` ha la riga e non il jar: dalla D-b (onda 8a) esce anche lui, con
    /// il suo motivo, perché il popup e il lancio chiamano la stessa funzione.
    #[test]
    fn an_offline_precheck_names_the_mods_left_out_and_carries_no_version_map() {
        let launcher_paths = offline_root("dropped", true);

        let result =
            offline_precheck_for_active_account(&launcher_paths, "Offline Pack", &target())
                .expect("the offline pre-check should run")
                .expect("an offline account takes the offline pre-check");

        assert_eq!(
            result
                .dropped_mods
                .iter()
                .map(|group| group.mod_ids.clone())
                .collect::<Vec<_>>(),
            vec![vec!["sodium".to_string()], vec!["lithium".to_string()]]
        );
        assert!(result.resolved.is_empty());
        assert!(result.updates.is_empty());
        assert!(result.resolved_content.is_empty());

        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove root");
    }

    /// D-b (onda 8a): senza mappa un jar registrato ma sparito dal disco non è
    /// «disponibile», perché ripristinarlo è un download e l'account gioca
    /// offline. Il mod esce con un motivo suo, distinto da «niente in cache»; con
    /// la mappa del pre-check la riga basta ancora e il lancio lo ripristina.
    #[test]
    fn offline_without_a_map_a_registered_jar_gone_from_disk_leaves_the_launch() {
        use super::super::DropReason;

        // `sodium` è registrato per il target, ma il suo jar non è sul disco.
        let launcher_paths = offline_root("jar-gone", true);
        let modlist = load_modlist(&launcher_paths, "Offline Pack").expect("mod-list");

        let offline = resolve_offline_selection(
            &launcher_paths,
            &modlist,
            &PinAliases::new(),
            &target(),
            None,
        )
        .expect("the offline selection should run");
        assert_eq!(
            offline
                .dropped
                .iter()
                .map(|lost| (lost.mod_id.as_str(), lost.reason))
                .collect::<Vec<_>>(),
            vec![
                ("sodium", DropReason::RegisteredJarGoneOffline),
                ("lithium", DropReason::NothingCachedOffline),
            ]
        );
        assert!(!offline
            .selected_mods
            .iter()
            .any(|selected| selected.mod_id == "sodium"));

        let map = HashMap::from([("sodium".to_string(), "sodium-v2".to_string())]);
        let preresolved = resolve_offline_selection(
            &launcher_paths,
            &modlist,
            &PinAliases::new(),
            &target(),
            Some(&map),
        )
        .expect("the pre-resolved selection should run");
        assert!(preresolved
            .selected_mods
            .iter()
            .any(|selected| selected.mod_id == "sodium"));
        assert!(!preresolved
            .dropped
            .iter()
            .any(|lost| lost.mod_id == "sodium"));

        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove root");
    }

    /// Audit 101, risoluzione-3 (D138 con le alternative): il primario e la
    /// sua prima alternativa hanno la riga registrata e il jar sparito. Prima
    /// le sonde si facevano solo sulla prima selezione, l'alternativa entrava
    /// senza essere sondata e il lancio offline la riscaricava dalla CDN. Ora
    /// ogni giro sonda i mod nuovi: la catena va avanti fino a un'alternativa
    /// col jar sul disco, e senza di essa il mod esce col suo motivo.
    #[test]
    fn offline_an_alternative_whose_jar_is_gone_is_probed_and_never_selected() {
        use super::super::DropReason;

        let launcher_paths = offline_root("alternatives", true);
        let connection =
            rusqlite::Connection::open(launcher_paths.database_path()).expect("open database");
        let repository = crate::mod_cache::SqliteModCacheRepository::new(
            &connection,
            launcher_paths.mods_cache_dir(),
        );
        let register = |mod_id: &str, jar_on_disk: bool| {
            let mut version = candidate(mod_id, &format!("{mod_id}-v1"), "1.0.0");
            version.project_id = mod_id.into();
            version.files = vec![crate::modrinth::ModrinthFile {
                hashes: HashMap::from([("sha1".to_string(), format!("{mod_id}-sha1"))]),
                url: format!("https://cdn.modrinth.com/data/{mod_id}/{mod_id}.jar"),
                filename: format!("{mod_id}.jar"),
                primary: true,
                size: 100,
            }];
            repository
                .upsert_modrinth_version(&version, &target())
                .expect("register the mod for the target");
            if jar_on_disk {
                let record = repository
                    .find_registered_by_project_or_alias(mod_id, &target())
                    .expect("read the row back")
                    .expect("the row was just written");
                let path = crate::mod_cache::cached_artifact_path_for_record(
                    launcher_paths.mods_cache_dir(),
                    &record,
                );
                std::fs::create_dir_all(path.parent().unwrap()).expect("jar directory");
                std::fs::write(&path, crate::mod_cache::test_jar_bytes("jar"))
                    .expect("write the jar");
            }
        };
        // `sodium` è già registrato da `offline_root`, col jar sparito.
        register("embeddium", false);
        register("rubidium", true);

        let alternative = |mod_id: &str| Rule {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        };
        let chain = |alternatives: Vec<Rule>| ModList {
            modlist_name: "Offline Pack".into(),
            author: "Author".into(),
            description: "Test".into(),
            rules: vec![Rule {
                alternatives,
                ..alternative("sodium")
            }],
        };
        let selected = |selection: &super::super::TargetSelection| {
            selection
                .selected_mods
                .iter()
                .map(|selected| selected.mod_id.clone())
                .collect::<Vec<_>>()
        };

        // Con rubidium sul disco la catena arriva fin lì, e nessuno è perso.
        let reaches_rubidium = resolve_offline_selection(
            &launcher_paths,
            &chain(vec![alternative("embeddium"), alternative("rubidium")]),
            &PinAliases::new(),
            &target(),
            None,
        )
        .expect("the offline selection should run");
        assert_eq!(selected(&reaches_rubidium), vec!["rubidium".to_string()]);
        assert!(reaches_rubidium.dropped.is_empty());
        assert_eq!(
            reaches_rubidium.unavailable,
            vec!["sodium".to_string(), "embeddium".to_string()]
        );

        // Senza rubidium embeddium non entra (niente download offline), e il
        // mod esce col motivo del jar sparito.
        let nothing_on_disk = resolve_offline_selection(
            &launcher_paths,
            &chain(vec![alternative("embeddium")]),
            &PinAliases::new(),
            &target(),
            None,
        )
        .expect("the offline selection should run");
        assert!(selected(&nothing_on_disk).is_empty());
        assert_eq!(
            nothing_on_disk
                .dropped
                .iter()
                .map(|lost| (lost.mod_id.as_str(), lost.reason))
                .collect::<Vec<_>>(),
            vec![("sodium", DropReason::RegisteredJarGoneOffline)]
        );

        drop(connection);
        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove root");
    }

    /// An account that plays online is not this branch's business: the
    /// online pre-check runs as it did before.
    #[test]
    fn an_online_account_does_not_take_the_offline_precheck() {
        let launcher_paths = offline_root("online", false);

        assert_eq!(
            offline_precheck_for_active_account(&launcher_paths, "Offline Pack", &target())
                .expect("reading the choice should succeed"),
            None
        );

        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove root");
    }

    /// D115, and what this test does **not** prove. It pins the shape that
    /// makes the offline branch request-free in this crate: a synchronous
    /// function that receives no HTTP or Modrinth client. Making it `async` or
    /// handing it a client stops this from compiling. It does not observe the
    /// socket layer: a synchronous body could still build a runtime and a
    /// client of its own, and only a trace of the running process sees that.
    #[test]
    fn the_offline_precheck_is_synchronous_and_receives_no_client() {
        let offline_branch: fn(
            &LauncherPaths,
            &str,
            &ResolutionTarget,
        ) -> Result<Option<UpdatePrecheckResult>> = offline_precheck_for_active_account;
        let launcher_paths = offline_root("shape", true);

        assert!(offline_branch(&launcher_paths, "Offline Pack", &target())
            .expect("the offline pre-check should run")
            .is_some());

        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove root");
    }
}
