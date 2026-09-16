use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::State;
use tokio::sync::Semaphore;

use crate::launcher_paths::LauncherPaths;
use crate::mod_cache::SqliteModCacheRepository;
use crate::modrinth::{ModrinthClient, ModrinthVersion};
use crate::rules::{ModList, ModSource, Rule, VersionRule, VersionRuleKind, RULES_FILENAME};

/// Look up cached Modrinth availability from the database.
fn db_availability_get(
    db_path: &Path,
    mod_ids: &[String],
    mc_version: &str,
    loader: &str,
) -> Vec<(String, bool)> {
    let Ok(conn) = Connection::open(db_path) else {
        return vec![];
    };
    let mut results = Vec::new();
    for mod_id in mod_ids {
        let row: Option<bool> = conn
            .query_row(
                "SELECT available FROM modrinth_availability WHERE project_id = ?1 AND mc_version = ?2 AND mod_loader = ?3",
                params![mod_id, mc_version, loader],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if let Some(available) = row {
            results.push((mod_id.clone(), available));
        }
    }
    results
}

/// Persist Modrinth availability results to the database.
fn db_availability_set(db_path: &Path, entries: &[(String, String, String, bool)]) {
    let Ok(conn) = Connection::open(db_path) else {
        return;
    };
    let Ok(tx) = conn.unchecked_transaction() else {
        return;
    };
    for (mod_id, mc_version, loader, available) in entries {
        let _ = tx.execute(
            r#"INSERT INTO modrinth_availability (project_id, mc_version, mod_loader, available)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(project_id, mc_version, mod_loader) DO UPDATE SET available = excluded.available"#,
            params![mod_id, mc_version, loader, available],
        );
    }
    let _ = tx.commit();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionTarget {
    pub minecraft_version: String,
    pub mod_loader: ModLoader,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum ModLoader {
    Fabric,
    NeoForge,
    Forge,
    Quilt,
    Vanilla,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionResult {
    pub active_mods: HashSet<String>,
    pub resolved_rules: Vec<ResolvedRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRule {
    pub mod_id: String,
    pub outcome: RuleOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleOutcome {
    Resolved {
        /// The mod_id that actually resolved (primary or a nested alternative).
        resolved_id: String,
    },
    Unresolved {
        reason: FailureReason,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FailureReason {
    ExcludedByActiveMod,
    RequiredModMissing,
    IncompatibleVersion,
    NoOptionAvailable,
}

pub fn resolve_modlist(modlist: &ModList, target: &ResolutionTarget) -> Result<ResolutionResult> {
    // ── Pre-process: strip mutual requires ──────────────────────────────────
    // Mutual links (A requires B AND B requires A) create a deadlock — neither
    // can activate first.  We detect these pairs across the entire rule tree
    // and remove them so the normal resolution handles everything correctly.
    let mut rules = modlist.rules.clone();
    strip_mutual_requires(&mut rules);

    // ── Pass 1: sequential resolve to build the initial active set ────────────
    let mut active_mods = HashSet::new();
    let mut resolved_rules = Vec::with_capacity(rules.len());

    for rule in &rules {
        let outcome = try_resolve(rule, &active_mods, target);
        if let RuleOutcome::Resolved { ref resolved_id } = outcome {
            active_mods.insert(resolved_id.clone());
        }
        resolved_rules.push(ResolvedRule {
            mod_id: rule.mod_id.clone(),
            outcome,
        });
    }

    // ── Pass 2: re-check each rule against the FULL active set ───────────────
    let full_active = active_mods.clone();
    let mut final_active = HashSet::new();

    for (i, rule) in rules.iter().enumerate() {
        let outcome = try_resolve(rule, &full_active, target);
        if let RuleOutcome::Resolved { ref resolved_id } = outcome {
            final_active.insert(resolved_id.clone());
        }
        resolved_rules[i] = ResolvedRule {
            mod_id: rule.mod_id.clone(),
            outcome,
        };
    }

    Ok(ResolutionResult {
        active_mods: final_active,
        resolved_rules,
    })
}

/// Collect all mod IDs, their requires, and enabled states across the entire rule tree.
fn collect_all_requires(
    rules: &[Rule],
) -> (Vec<(String, Vec<String>)>, HashMap<String, bool>) {
    let mut requires = Vec::new();
    let mut enabled = HashMap::new();
    for rule in rules {
        requires.push((rule.mod_id.clone(), rule.requires.clone()));
        enabled.insert(rule.mod_id.clone(), rule.enabled);
        let (alternative_requires, alternative_enabled) =
            collect_all_requires(&rule.alternatives);
        requires.extend(alternative_requires);
        enabled.extend(alternative_enabled);
    }
    (requires, enabled)
}

/// Detect mutual requires (A requires B AND B requires A) and remove both
/// directions so the resolver doesn't deadlock.
fn strip_mutual_requires(rules: &mut Vec<Rule>) {
    // Build a set of all mutual pairs.
    let (all_reqs, enabled_map) = collect_all_requires(rules);
    let req_set: HashSet<(&str, &str)> = all_reqs
        .iter()
        .flat_map(|(from, tos)| tos.iter().map(move |to| (from.as_str(), to.as_str())))
        .collect();

    let mut mutual_pairs = HashSet::new();
    for &(a, b) in &req_set {
        if req_set.contains(&(b, a))
            && enabled_map.get(a) == Some(&true)
            && enabled_map.get(b) == Some(&true)
        {
            mutual_pairs.insert((a.to_string(), b.to_string()));
        }
    }

    if mutual_pairs.is_empty() {
        return;
    }

    // Recursively remove mutual requires from all rules.
    fn remove_mutual(rules: &mut Vec<Rule>, pairs: &HashSet<(String, String)>) {
        for rule in rules.iter_mut() {
            rule.requires
                .retain(|req| !pairs.contains(&(rule.mod_id.clone(), req.clone())));
            remove_mutual(&mut rule.alternatives, pairs);
        }
    }
    remove_mutual(rules, &mutual_pairs);
}

fn try_resolve(
    rule: &Rule,
    active_mods: &HashSet<String>,
    target: &ResolutionTarget,
) -> RuleOutcome {
    // 0. Disabled mods are treated as if they don't exist — skip to alternatives.
    if !rule.enabled {
        return try_alternatives(
            rule,
            active_mods,
            target,
            FailureReason::ExcludedByActiveMod,
        );
    }

    // 1. Check exclude_if
    if rule.exclude_if.iter().any(|id| active_mods.contains(id)) {
        return try_alternatives(
            rule,
            active_mods,
            target,
            FailureReason::ExcludedByActiveMod,
        );
    }

    // 2. Check requires
    if rule.requires.iter().any(|id| !active_mods.contains(id)) {
        return try_alternatives(rule, active_mods, target, FailureReason::RequiredModMissing);
    }

    // 3. Check version_rules
    if version_rules_conflict(&rule.version_rules, target) {
        return try_alternatives(
            rule,
            active_mods,
            target,
            FailureReason::IncompatibleVersion,
        );
    }

    RuleOutcome::Resolved {
        resolved_id: rule.mod_id.clone(),
    }
}

fn try_alternatives(
    rule: &Rule,
    active_mods: &HashSet<String>,
    target: &ResolutionTarget,
    reason: FailureReason,
) -> RuleOutcome {
    for alt in &rule.alternatives {
        let outcome = try_resolve(alt, active_mods, target);
        if matches!(outcome, RuleOutcome::Resolved { .. }) {
            return outcome;
        }
    }
    RuleOutcome::Unresolved { reason }
}

/// Returns true if the version rules exclude this mod for the given target.
pub(crate) fn version_rules_conflict(
    version_rules: &[VersionRule],
    target: &ResolutionTarget,
) -> bool {
    for vr in version_rules {
        let version_matches = vr.mc_versions.is_empty()
            || vr
                .mc_versions
                .iter()
                .any(|v| crate::modrinth::mc_version_matches(v, &target.minecraft_version));
        let vr_loader = vr.loader.to_ascii_lowercase();
        let loader_matches =
            vr_loader == "any" || vr_loader == target.mod_loader.as_modrinth_loader();

        match vr.kind {
            VersionRuleKind::Only => {
                // The mod is EXCLUDED unless version AND loader match.
                if !(version_matches && loader_matches) {
                    return true;
                }
            }
            VersionRuleKind::Exclude => {
                // The mod is EXCLUDED if version AND loader match.
                if version_matches && loader_matches {
                    return true;
                }
            }
        }
    }
    false
}

/// Look up which Rule was actually resolved for a top-level rule (follows alternatives).
pub fn find_resolved_rule<'a>(top_rule: &'a Rule, outcome: &RuleOutcome) -> Option<&'a Rule> {
    match outcome {
        RuleOutcome::Resolved { resolved_id } => find_rule_by_id(top_rule, resolved_id),
        RuleOutcome::Unresolved { .. } => None,
    }
}

fn find_rule_by_id<'a>(rule: &'a Rule, mod_id: &str) -> Option<&'a Rule> {
    if rule.mod_id == mod_id {
        return Some(rule);
    }
    for alt in &rule.alternatives {
        if let Some(found) = find_rule_by_id(alt, mod_id) {
            return Some(found);
        }
    }
    None
}

// ── Tauri commands ────────────────────────────────────────────────────────────

pub(crate) fn parse_mod_loader(value: &str) -> Result<ModLoader> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fabric" => Ok(ModLoader::Fabric),
        "forge" => Ok(ModLoader::Forge),
        "neoforge" => Ok(ModLoader::NeoForge),
        "vanilla" => Ok(ModLoader::Vanilla),
        "quilt" => bail!("Quilt is no longer supported by Cubic Launcher"),
        other => bail!("unsupported mod loader '{other}'"),
    }
}

/// Returns the set of active (resolved) mod IDs for a given modlist + version + loader.
#[tauri::command]
pub async fn resolve_modlist_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    mc_version: String,
    mod_loader: String,
) -> Result<Vec<String>, String> {
    let rules_path = launcher_paths
        .modlists_dir()
        .join(&modlist_name)
        .join(RULES_FILENAME);

    let modlist = ModList::read_from_file(&rules_path).map_err(|e| e.to_string())?;

    let target = ResolutionTarget {
        minecraft_version: mc_version,
        mod_loader: parse_mod_loader(&mod_loader).map_err(|e| e.to_string())?,
    };

    if target.mod_loader == ModLoader::Vanilla {
        return Ok(Vec::new());
    }

    let result = resolve_modlist(&modlist, &target).map_err(|e| e.to_string())?;

    let mut ids = result.active_mods;

    // ── Modrinth availability check ─────────────────────────────────────────
    // For each resolved mod sourced from Modrinth, verify that a compatible
    // version actually exists for the target MC version + loader.
    let modrinth_ids: Vec<String> = ids
        .iter()
        .filter(|id| {
            modlist
                .find_rule(id)
                .map_or(false, |rule| rule.source == ModSource::Modrinth)
        })
        .cloned()
        .collect();

    if !modrinth_ids.is_empty() {
        let db_path = launcher_paths.database_path();
        let loader_str = mod_loader.clone();

        // Check database cache first
        let cached = db_availability_get(
            &db_path,
            &modrinth_ids,
            &target.minecraft_version,
            &loader_str,
        );
        let cached_ids: HashSet<String> = cached.iter().map(|(id, _)| id.clone()).collect();
        for (mod_id, available) in &cached {
            if !available {
                ids.remove(mod_id);
            }
        }

        // Only call Modrinth API for mods not in the database
        let uncached_ids: Vec<String> = modrinth_ids
            .into_iter()
            .filter(|id| !cached_ids.contains(id))
            .collect();

        if !uncached_ids.is_empty() {
            let client = ModrinthClient::new();
            let mut tasks = tokio::task::JoinSet::new();
            let semaphore = Arc::new(Semaphore::new(10));

            for mod_id in uncached_ids {
                let client = client.clone();
                let target = target.clone();
                let permit_source = semaphore.clone();
                tasks.spawn(async move {
                    let _permit = permit_source.acquire_owned().await.ok()?;
                    let result = client.fetch_project_versions(&mod_id, &target).await;
                    match result {
                        Ok(v) => Some((mod_id, !v.is_empty())),
                        Err(_) => None, // on network error, skip — don't cache failures
                    }
                });
            }

            let mut to_persist = Vec::new();
            while let Some(join_result) = tasks.join_next().await {
                if let Ok(Some((mod_id, available))) = join_result {
                    if !available {
                        ids.remove(&mod_id);
                    }
                    to_persist.push((
                        mod_id,
                        target.minecraft_version.clone(),
                        loader_str.clone(),
                        available,
                    ));
                }
            }

            // Persist API results to database for future lookups
            if !to_persist.is_empty() {
                db_availability_set(&db_path, &to_persist);
            }
        }
    }

    // ── Cascade: remove mods whose requires are unsatisfied ────────────────
    // The Modrinth check (or version rules) may have removed a mod that
    // others depend on.  Iteratively remove any mod whose original requires
    // are no longer fully satisfied.
    let mut cascade_changed = true;
    while cascade_changed {
        cascade_changed = false;
        let snapshot: Vec<String> = ids.iter().cloned().collect();
        for mod_id in &snapshot {
            if let Some(rule) = modlist.find_rule(mod_id) {
                if rule.requires.iter().any(|req| !ids.contains(req)) {
                    ids.remove(mod_id);
                    cascade_changed = true;
                }
            }
        }
    }

    // ── Alt viability for UI ────────────────────────────────────────────────
    // After all checks (Modrinth availability, cascade), show which
    // alternatives are viable for rules whose primary is no longer in `ids`.
    // This runs last so it sees the final state of which mods survived.
    let before_alts: HashSet<String> = ids.clone();
    for rule in &modlist.rules {
        if !ids.contains(&rule.mod_id) {
            check_viable_alts(rule, &mut ids, &target);
        }
    }

    // check_viable_alts only tests user-defined constraints (version_rules,
    // requires, exclude_if).  Modrinth-sourced alternatives still need an
    // availability check — remove those the cache marks as unavailable.
    {
        let new_alt_modrinth_ids: Vec<String> = ids
            .iter()
            .filter(|id| !before_alts.contains(*id))
            .filter(|id| {
                modlist
                    .find_rule(id)
                    .map_or(false, |r| r.source == ModSource::Modrinth)
            })
            .cloned()
            .collect();

        if !new_alt_modrinth_ids.is_empty() {
            let db_path = launcher_paths.database_path();
            let cached = db_availability_get(
                &db_path,
                &new_alt_modrinth_ids,
                &target.minecraft_version,
                &mod_loader,
            );
            for (mod_id, available) in &cached {
                if !available {
                    ids.remove(mod_id);
                }
            }
        }
    }

    Ok(ids.into_iter().collect())
}

/// What one backfill pass decided. The command itself discards it: the pass
/// runs in the background and would otherwise leave no evidence of what it
/// asked, retried or wrote, which is what an off-line equivalence check needs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillOutcome {
    /// Exactly the rows handed to `db_availability_set`, in persist order:
    /// `(mod_id, available)`.
    pub persisted: Vec<(String, bool)>,
    /// Hashes sent to the bulk cascade; 0 means no bulk call was issued.
    pub bulk_hash_count: usize,
    /// `GET /project/{id}/version` requests issued by the per-project stage.
    pub per_project_request_count: usize,
    /// Mod ids whose cached hash the bulk response omitted, retried per
    /// project (D22).
    pub omitted_mod_ids: Vec<String>,
    /// Set when the bulk call itself failed and its whole set fell back.
    pub bulk_error: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct HashSplit {
    hashed: Vec<(String, String)>,
    unhashed: Vec<String>,
}

struct BulkStageOutcome {
    available_mod_ids: Vec<String>,
    per_project_mod_ids: Vec<String>,
    omitted_mod_ids: Vec<String>,
    bulk_error: Option<String>,
}

fn load_cached_hashes_for_backfill(
    launcher_paths: &LauncherPaths,
    mod_ids: &[String],
    target: &ResolutionTarget,
) -> Result<HashMap<String, String>> {
    let connection = Connection::open(launcher_paths.database_path())?;
    let repository = SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
    let mut hash_by_mod_id = HashMap::new();

    for mod_id in mod_ids {
        if let Some(hash) = repository
            .find_cached_file_hash_by_project_or_alias(mod_id, target)?
            .filter(|hash| !hash.is_empty())
        {
            hash_by_mod_id.insert(mod_id.clone(), hash);
        }
    }

    Ok(hash_by_mod_id)
}

fn split_ids_by_cached_hash(
    ids: &[String],
    hash_by_mod_id: &HashMap<String, String>,
) -> HashSplit {
    let mut split = HashSplit::default();
    let mut seen = HashSet::new();

    for mod_id in ids {
        if !seen.insert(mod_id.as_str()) {
            continue;
        }

        match hash_by_mod_id.get(mod_id) {
            Some(hash) => split.hashed.push((mod_id.clone(), hash.clone())),
            None => split.unhashed.push(mod_id.clone()),
        }
    }

    split
}

async fn resolve_bulk_availability<Fetch, Fut>(
    split: &HashSplit,
    fetch_bulk: Fetch,
) -> BulkStageOutcome
where
    Fetch: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = anyhow::Result<HashMap<String, ModrinthVersion>>>,
{
    if split.hashed.is_empty() {
        return BulkStageOutcome {
            available_mod_ids: Vec::new(),
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
            let mut available_mod_ids = Vec::new();
            let mut omitted_mod_ids = Vec::new();
            for (mod_id, hash) in &split.hashed {
                if versions_by_hash.contains_key(hash) {
                    available_mod_ids.push(mod_id.clone());
                } else {
                    // An omission can also mean an unknown hash, so retry it per project.
                    omitted_mod_ids.push(mod_id.clone());
                }
            }

            let mut per_project_mod_ids = omitted_mod_ids.clone();
            per_project_mod_ids.extend(split.unhashed.iter().cloned());
            BulkStageOutcome {
                available_mod_ids,
                per_project_mod_ids,
                omitted_mod_ids,
                bulk_error: None,
            }
        }
        Err(error) => {
            // A bulk failure must not make the entire hashed set unavailable.
            let mut per_project_mod_ids = split
                .hashed
                .iter()
                .map(|(mod_id, _)| mod_id.clone())
                .collect::<Vec<_>>();
            per_project_mod_ids.extend(split.unhashed.iter().cloned());
            BulkStageOutcome {
                available_mod_ids: Vec::new(),
                per_project_mod_ids,
                omitted_mod_ids: Vec::new(),
                bulk_error: Some(format!("{error:#}")),
            }
        }
    }
}

async fn check_availability_concurrently<Check, Fut>(
    mod_ids: Vec<String>,
    permits: usize,
    check: Check,
) -> Vec<(String, bool)>
where
    Check: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
{
    let mut tasks = tokio::task::JoinSet::new();
    let semaphore = Arc::new(Semaphore::new(permits));

    for mod_id in mod_ids {
        let check = check.clone();
        let permit_source = semaphore.clone();
        tasks.spawn(async move {
            let _permit = permit_source.acquire_owned().await.ok()?;
            match check(mod_id.clone()).await {
                Ok(available) => Some((mod_id, available)),
                Err(_) => None,
            }
        });
    }

    let mut results = Vec::new();
    while let Some(join_result) = tasks.join_next().await {
        if let Ok(Some(result)) = join_result {
            results.push(result);
        }
    }
    results
}

fn merge_backfill_entries(
    bulk_available: &[String],
    per_project: &[(String, bool)],
) -> Vec<(String, bool)> {
    let mut merged = Vec::with_capacity(bulk_available.len() + per_project.len());
    let mut seen = HashSet::new();

    for mod_id in bulk_available {
        if seen.insert(mod_id.as_str()) {
            merged.push((mod_id.clone(), true));
        }
    }
    for (mod_id, available) in per_project {
        if seen.insert(mod_id.as_str()) {
            merged.push((mod_id.clone(), *available));
        }
    }

    merged
}

pub async fn backfill_availability(
    launcher_paths: &LauncherPaths,
    client: &ModrinthClient,
    modlist_name: &str,
    mc_version: &str,
    mod_loader: &str,
) -> Result<BackfillOutcome, String> {
    let rules_path = launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(RULES_FILENAME);
    let modlist = ModList::read_from_file(&rules_path).map_err(|e| e.to_string())?;
    let parsed_loader = parse_mod_loader(mod_loader).map_err(|e| e.to_string())?;

    if parsed_loader == ModLoader::Vanilla {
        return Ok(BackfillOutcome::default());
    }

    let mut all_modrinth_ids = Vec::new();
    fn collect_modrinth_ids(rules: &[Rule], out: &mut Vec<String>) {
        for rule in rules {
            if rule.source == ModSource::Modrinth {
                out.push(rule.mod_id.clone());
            }
            collect_modrinth_ids(&rule.alternatives, out);
        }
    }
    collect_modrinth_ids(&modlist.rules, &mut all_modrinth_ids);

    if all_modrinth_ids.is_empty() {
        return Ok(BackfillOutcome::default());
    }

    let db_path = launcher_paths.database_path();
    let cached = db_availability_get(&db_path, &all_modrinth_ids, mc_version, mod_loader);
    // Only skip mods that are cached as *available*. Mods cached as
    // unavailable are re-checked because mod authors frequently add
    // version support after initial release.
    let skip_ids: HashSet<String> = cached
        .iter()
        .filter(|(_, available)| *available)
        .map(|(id, _)| id.clone())
        .collect();
    let ids_to_check = all_modrinth_ids
        .into_iter()
        .filter(|id| !skip_ids.contains(id))
        .collect::<Vec<_>>();

    if ids_to_check.is_empty() {
        return Ok(BackfillOutcome::default());
    }

    let target = ResolutionTarget {
        minecraft_version: mc_version.to_string(),
        mod_loader: parsed_loader,
    };
    // Opening or querying mod_cache may fail; fall back to per-project requests.
    let hash_by_mod_id =
        load_cached_hashes_for_backfill(launcher_paths, &ids_to_check, &target)
            .unwrap_or_default();
    let split = split_ids_by_cached_hash(&ids_to_check, &hash_by_mod_id);
    let bulk_hash_count = split.hashed.len();
    // Availability only asks whether a compatible version exists, so the
    // release → beta → alpha installation preference does not apply here.
    let bulk_outcome = resolve_bulk_availability(&split, |hashes| {
        let target = &target;
        async move {
            client
                .fetch_versions_by_hash_any_channel(&hashes, target)
                .await
        }
    })
    .await;

    // The endpoint reports neither "unknown hash" nor "no version for this
    // target": both are omissions. Naming them is the only diagnostic it
    // offers (D23).
    if let Some(error) = &bulk_outcome.bulk_error {
        eprintln!(
            "[Backfill] bulk availability lookup failed for {bulk_hash_count} cached hashes; falling back to one request per mod ({error})"
        );
    } else if !bulk_outcome.omitted_mod_ids.is_empty() {
        eprintln!(
            "[Backfill] bulk availability lookup omitted {}; retrying one request per mod",
            bulk_outcome.omitted_mod_ids.join(", ")
        );
    }

    let per_project_request_count = bulk_outcome.per_project_mod_ids.len();
    let check_client = client.clone();
    let check_target = target.clone();
    // Match resolve_modlist_command's limit so both paths share the same API pressure.
    let per_project = check_availability_concurrently(
        bulk_outcome.per_project_mod_ids,
        10,
        move |mod_id| {
            let client = check_client.clone();
            let target = check_target.clone();
            async move {
                client
                    .fetch_project_versions(&mod_id, &target)
                    .await
                    .map(|versions| !versions.is_empty())
            }
        },
    )
    .await;
    let persisted = merge_backfill_entries(&bulk_outcome.available_mod_ids, &per_project);
    let entries = persisted
        .iter()
        .map(|(mod_id, available)| {
            (
                mod_id.clone(),
                mc_version.to_string(),
                mod_loader.to_string(),
                *available,
            )
        })
        .collect::<Vec<_>>();
    if !entries.is_empty() {
        db_availability_set(&db_path, &entries);
    }

    Ok(BackfillOutcome {
        persisted,
        bulk_hash_count,
        per_project_request_count,
        omitted_mod_ids: bulk_outcome.omitted_mod_ids,
        bulk_error: bulk_outcome.bulk_error,
    })
}

/// Pre-populates the modrinth_availability table for all Modrinth-sourced mods
/// in a modlist that don't already have a cached result for the given version+loader.
/// Runs in the background so it doesn't block the UI.
#[tauri::command]
pub async fn backfill_availability_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    mc_version: String,
    mod_loader: String,
) -> Result<(), String> {
    let client = ModrinthClient::new();
    backfill_availability(
        &launcher_paths,
        &client,
        &modlist_name,
        &mc_version,
        &mod_loader,
    )
    .await?;
    Ok(())
}

/// Recursively check every alternative in the tree and add viable ones to `ids`.
/// Only recurses into an alt's children when the alt itself is NOT viable
/// (its sub-alternatives are only relevant as fallbacks when it fails).
fn check_viable_alts(rule: &Rule, ids: &mut HashSet<String>, target: &ResolutionTarget) {
    for alt in &rule.alternatives {
        if ids.contains(&alt.mod_id) {
            continue;
        }
        if alt_itself_viable(alt, ids, target) {
            ids.insert(alt.mod_id.clone());
        } else {
            check_viable_alts(alt, ids, target);
        }
    }
}

/// Returns true only if this specific rule passes all checks (exclude_if, requires,
/// version_rules) — ignores its own alternatives.
fn alt_itself_viable(
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::rules::{ModSource, Rule, VersionRule, VersionRuleKind};

    use super::*;

    fn target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    fn simple_rule(mod_id: &str) -> Rule {
        Rule {
            mod_id: mod_id.into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        }
    }

    fn modlist(rules: Vec<Rule>) -> ModList {
        ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: "Test".into(),
            rules,
        }
    }

    fn cached_hashes(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(mod_id, hash)| (mod_id.to_string(), hash.to_string()))
            .collect()
    }

    fn modrinth_version(id: &str) -> ModrinthVersion {
        ModrinthVersion {
            id: id.into(),
            project_id: format!("{id}-project"),
            version_number: "1.0.0".into(),
            name: id.into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            version_type: "release".into(),
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn all_hashed_mods_are_persisted_available_without_per_project_requests() {
        let ids = vec!["sodium".to_string(), "lithium".to_string()];
        let split = split_ids_by_cached_hash(
            &ids,
            &cached_hashes(&[("sodium", "hash-a"), ("lithium", "hash-b")]),
        );
        let bulk_calls = Arc::new(AtomicUsize::new(0));
        let recorded_bulk_calls = bulk_calls.clone();
        let bulk = resolve_bulk_availability(&split, move |hashes| {
            recorded_bulk_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(hashes, vec!["hash-a".to_string(), "hash-b".to_string()]);
                Ok(HashMap::from([
                    ("hash-a".to_string(), modrinth_version("sodium-version")),
                    ("hash-b".to_string(), modrinth_version("lithium-version")),
                ]))
            }
        })
        .await;

        let per_project_calls = Arc::new(AtomicUsize::new(0));
        let recorded_per_project_calls = per_project_calls.clone();
        let per_project = check_availability_concurrently(
            bulk.per_project_mod_ids.clone(),
            10,
            move |_mod_id| {
                let calls = recorded_per_project_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                }
            },
        )
        .await;
        let persisted = merge_backfill_entries(&bulk.available_mod_ids, &per_project);

        assert_eq!(bulk_calls.load(Ordering::SeqCst), 1);
        assert_eq!(per_project_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            persisted,
            vec![("sodium".to_string(), true), ("lithium".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn unhashed_mods_skip_bulk_and_are_persisted_from_per_project_checks() {
        let ids = vec!["sodium".to_string(), "lithium".to_string()];
        let split = split_ids_by_cached_hash(&ids, &HashMap::new());
        let bulk_calls = Arc::new(AtomicUsize::new(0));
        let recorded_bulk_calls = bulk_calls.clone();
        let bulk = resolve_bulk_availability(&split, move |_hashes| {
            recorded_bulk_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(HashMap::new()) }
        })
        .await;

        assert_eq!(bulk_calls.load(Ordering::SeqCst), 0);
        assert_eq!(bulk.per_project_mod_ids, ids);

        let per_project_calls = Arc::new(AtomicUsize::new(0));
        let recorded_per_project_calls = per_project_calls.clone();
        let per_project = check_availability_concurrently(
            bulk.per_project_mod_ids.clone(),
            10,
            move |_mod_id| {
                let calls = recorded_per_project_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                }
            },
        )
        .await;
        let mut persisted = merge_backfill_entries(&bulk.available_mod_ids, &per_project);
        persisted.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(per_project_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            persisted,
            vec![("lithium".to_string(), true), ("sodium".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn mixed_hash_coverage_persists_each_mod_once_through_the_right_path() {
        let ids = vec![
            "sodium".to_string(),
            "polytone".to_string(),
            "lithium".to_string(),
            "sodium".to_string(),
        ];
        let split = split_ids_by_cached_hash(
            &ids,
            &cached_hashes(&[("sodium", "hash-a"), ("lithium", "hash-b")]),
        );
        let bulk_calls = Arc::new(AtomicUsize::new(0));
        let recorded_bulk_calls = bulk_calls.clone();
        let bulk = resolve_bulk_availability(&split, move |hashes| {
            recorded_bulk_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(hashes, vec!["hash-a".to_string(), "hash-b".to_string()]);
                Ok(HashMap::from([
                    ("hash-a".to_string(), modrinth_version("sodium-version")),
                    ("hash-b".to_string(), modrinth_version("lithium-version")),
                ]))
            }
        })
        .await;

        assert_eq!(bulk.per_project_mod_ids, vec!["polytone"]);
        let per_project = check_availability_concurrently(
            bulk.per_project_mod_ids.clone(),
            10,
            |mod_id| async move {
                assert_eq!(mod_id, "polytone");
                Ok(false)
            },
        )
        .await;
        let persisted = merge_backfill_entries(&bulk.available_mod_ids, &per_project);

        assert_eq!(bulk_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            persisted,
            vec![
                ("sodium".to_string(), true),
                ("lithium".to_string(), true),
                ("polytone".to_string(), false),
            ]
        );
    }

    #[tokio::test]
    async fn omitted_bulk_hash_is_retried_before_availability_is_persisted() {
        let ids = vec!["sodium".to_string(), "lithium".to_string()];
        let split = split_ids_by_cached_hash(
            &ids,
            &cached_hashes(&[("sodium", "hash-a"), ("lithium", "hash-b")]),
        );
        let bulk = resolve_bulk_availability(&split, |_hashes| async {
            Ok(HashMap::from([(
                "hash-a".to_string(),
                modrinth_version("sodium-version"),
            )]))
        })
        .await;

        assert_eq!(bulk.omitted_mod_ids, vec!["lithium"]);
        assert_eq!(bulk.per_project_mod_ids, vec!["lithium"]);
        let without_retry = merge_backfill_entries(&bulk.available_mod_ids, &[]);
        assert!(!without_retry
            .iter()
            .any(|(mod_id, _available)| mod_id == "lithium"));

        let per_project = check_availability_concurrently(
            bulk.per_project_mod_ids.clone(),
            10,
            |mod_id| async move {
                assert_eq!(mod_id, "lithium");
                Ok(true)
            },
        )
        .await;
        let persisted = merge_backfill_entries(&bulk.available_mod_ids, &per_project);

        assert_eq!(
            persisted,
            vec![("sodium".to_string(), true), ("lithium".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn network_errors_are_not_persisted_and_bulk_errors_fall_back_per_project() {
        let ids = vec!["sodium".to_string(), "broken-mod".to_string()];
        let split =
            split_ids_by_cached_hash(&ids, &cached_hashes(&[("sodium", "hash-a")]));
        let bulk = resolve_bulk_availability(&split, |_hashes| async {
            Err::<HashMap<String, ModrinthVersion>, _>(anyhow::anyhow!("connection reset"))
        })
        .await;

        assert_eq!(bulk.bulk_error.as_deref(), Some("connection reset"));
        assert_eq!(
            bulk.per_project_mod_ids,
            vec!["sodium".to_string(), "broken-mod".to_string()]
        );
        let per_project = check_availability_concurrently(
            bulk.per_project_mod_ids.clone(),
            10,
            |mod_id| async move {
                if mod_id == "broken-mod" {
                    Err(anyhow::anyhow!("request failed"))
                } else {
                    Ok(true)
                }
            },
        )
        .await;
        let persisted = merge_backfill_entries(&bulk.available_mod_ids, &per_project);

        assert_eq!(persisted, vec![("sodium".to_string(), true)]);
        assert!(!persisted
            .iter()
            .any(|(mod_id, _available)| mod_id == "broken-mod"));
    }

    /// The bug this pass exists to close: the per-project stage used to spawn
    /// one unbounded task per mod, so a 300-mod modlist could put 300 requests
    /// in flight against a 300-per-minute limit.
    #[tokio::test]
    async fn per_project_checks_never_exceed_the_permitted_concurrency() {
        let mod_ids = (0..24).map(|index| format!("mod-{index}")).collect::<Vec<_>>();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let observed_in_flight = in_flight.clone();
        let observed_peak = peak_in_flight.clone();

        let checked = check_availability_concurrently(mod_ids.clone(), 3, move |_mod_id| {
            let in_flight = observed_in_flight.clone();
            let peak = observed_peak.clone();
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(true)
            }
        })
        .await;

        assert_eq!(checked.len(), mod_ids.len());
        let peak = peak_in_flight.load(Ordering::SeqCst);
        assert!(
            peak <= 3,
            "the per-project stage put {peak} requests in flight with 3 permits"
        );
        assert!(
            peak > 1,
            "the stage serialized every request, so the permit count buys nothing"
        );
    }

    #[test]
    fn basic_resolution() {
        let ml = modlist(vec![simple_rule("sodium"), simple_rule("lithium")]);
        let result = resolve_modlist(&ml, &target()).unwrap();

        assert!(result.active_mods.contains("sodium"));
        assert!(result.active_mods.contains("lithium"));
        assert_eq!(result.resolved_rules.len(), 2);
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Resolved {
                resolved_id: "sodium".into()
            }
        );
    }

    #[test]
    fn exclude_if_with_fallback() {
        let ml = modlist(vec![
            simple_rule("sodium"),
            Rule {
                mod_id: "embeddium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec!["sodium".into()],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![simple_rule("iris")],
            },
        ]);

        let result = resolve_modlist(&ml, &target()).unwrap();

        assert!(result.active_mods.contains("sodium"));
        assert!(!result.active_mods.contains("embeddium"));
        assert!(result.active_mods.contains("iris"));
        assert_eq!(
            result.resolved_rules[1].outcome,
            RuleOutcome::Resolved {
                resolved_id: "iris".into()
            }
        );
    }

    #[test]
    fn requires_satisfied() {
        let ml = modlist(vec![
            simple_rule("fabric-api"),
            Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec!["fabric-api".into()],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            },
        ]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(result.active_mods.contains("sodium"));
    }

    #[test]
    fn requires_unsatisfied() {
        let ml = modlist(vec![Rule {
            mod_id: "sodium".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec!["fabric-api".into()],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(!result.active_mods.contains("sodium"));
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::RequiredModMissing,
            }
        );
    }

    #[test]
    fn version_rule_exclude() {
        let ml = modlist(vec![Rule {
            mod_id: "sodium".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![VersionRule {
                kind: VersionRuleKind::Exclude,
                mc_versions: vec!["1.21.1".into()],
                loader: "fabric".into(),
            }],
            custom_configs: vec![],
            alternatives: vec![],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(!result.active_mods.contains("sodium"));
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::IncompatibleVersion,
            }
        );
    }

    #[test]
    fn version_rule_only() {
        // "only" on 1.20.1/forge — should fail on 1.21.1/fabric target
        let ml = modlist(vec![Rule {
            mod_id: "optifine".into(),
            source: ModSource::Local,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![VersionRule {
                kind: VersionRuleKind::Only,
                mc_versions: vec!["1.20.1".into()],
                loader: "forge".into(),
            }],
            custom_configs: vec![],
            alternatives: vec![],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(!result.active_mods.contains("optifine"));
    }

    #[test]
    fn version_rule_only_matches() {
        let ml = modlist(vec![Rule {
            mod_id: "optifine".into(),
            source: ModSource::Local,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![VersionRule {
                kind: VersionRuleKind::Only,
                mc_versions: vec!["1.21.1".into()],
                loader: "fabric".into(),
            }],
            custom_configs: vec![],
            alternatives: vec![],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(result.active_mods.contains("optifine"));
    }

    #[test]
    fn version_rule_exclude_with_any_loader() {
        let ml = modlist(vec![Rule {
            mod_id: "sodium".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec![],
            version_rules: vec![VersionRule {
                kind: VersionRuleKind::Exclude,
                mc_versions: vec!["1.21.1".into()],
                loader: "any".into(),
            }],
            custom_configs: vec![],
            alternatives: vec![],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(!result.active_mods.contains("sodium"));
    }

    #[test]
    fn version_rule_exclude_with_any_version() {
        let mut rule = simple_rule("sodium");
        rule.version_rules = vec![VersionRule {
            kind: VersionRuleKind::Exclude,
            mc_versions: vec![],
            loader: "fabric".into(),
        }];

        let result = resolve_modlist(&modlist(vec![rule]), &target()).unwrap();
        assert!(!result.active_mods.contains("sodium"));
    }

    #[test]
    fn version_rule_only_with_any_version_matches_loader() {
        let mut rule = simple_rule("sodium");
        rule.version_rules = vec![VersionRule {
            kind: VersionRuleKind::Only,
            mc_versions: vec![],
            loader: "fabric".into(),
        }];

        let result = resolve_modlist(&modlist(vec![rule]), &target()).unwrap();
        assert!(result.active_mods.contains("sodium"));
    }

    #[test]
    fn deep_alternative_recursion() {
        let ml = modlist(vec![Rule {
            mod_id: "a".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec!["missing".into()], // fails
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![
                Rule {
                    mod_id: "b".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec!["also-missing".into()], // also fails
                    version_rules: vec![],
                    custom_configs: vec![],
                    alternatives: vec![],
                },
                simple_rule("c"), // should succeed
            ],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(result.active_mods.contains("c"));
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Resolved {
                resolved_id: "c".into()
            }
        );
    }

    #[test]
    fn all_alternatives_fail() {
        let ml = modlist(vec![Rule {
            mod_id: "a".into(),
            source: ModSource::Modrinth,
            enabled: true,
            exclude_if: vec![],
            requires: vec!["missing".into()],
            version_rules: vec![],
            custom_configs: vec![],
            alternatives: vec![Rule {
                mod_id: "b".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec!["also-missing".into()],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            }],
        }]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(result.active_mods.is_empty());
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::RequiredModMissing,
            }
        );
    }

    #[test]
    fn exclude_if_no_alternatives_stays_unresolved() {
        let ml = modlist(vec![
            simple_rule("sodium"),
            Rule {
                mod_id: "mod-b".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec!["sodium".into()],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            },
        ]);

        let result = resolve_modlist(&ml, &target()).unwrap();
        assert!(!result.active_mods.contains("mod-b"));
        assert_eq!(
            result.resolved_rules[1].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::ExcludedByActiveMod,
            }
        );
    }
    #[test]
    fn mutual_requires_with_disabled_peer_does_not_resolve() {
        let mut a = simple_rule("A");
        a.requires = vec!["B".into()];
        let mut disabled_b = simple_rule("B");
        disabled_b.enabled = false;
        disabled_b.requires = vec!["A".into()];

        let result = resolve_modlist(&modlist(vec![a, disabled_b]), &target()).unwrap();
        assert!(!result.active_mods.contains("A"));

        let mut a = simple_rule("A");
        a.requires = vec!["B".into()];
        let mut b = simple_rule("B");
        b.requires = vec!["A".into()];

        let result = resolve_modlist(&modlist(vec![a, b]), &target()).unwrap();
        assert!(result.active_mods.contains("A"));
        assert!(result.active_mods.contains("B"));
    }
}
