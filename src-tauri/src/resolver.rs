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
use crate::mod_cache::{CacheProbe, SqliteModCacheRepository};
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
///
/// Una scrittura fallita non ferma la risoluzione — la cache serve solo a
/// risparmiare richieste — ma si dice nel log: prima finiva in `let _`, e una
/// cache che non si riempie rifaceva tutte le richieste in silenzio (audit 101,
/// «Cache delle disponibilità»).
fn db_availability_set(db_path: &Path, entries: &[(String, String, String, bool)]) {
    if let Err(error) = try_db_availability_set(db_path, entries) {
        eprintln!(
            "[Availability] could not save {} availability result(s) to {}: {error:#}",
            entries.len(),
            db_path.display()
        );
    }
}

fn try_db_availability_set(
    db_path: &Path,
    entries: &[(String, String, String, bool)],
) -> Result<()> {
    let conn = Connection::open(db_path)?;
    let tx = conn.unchecked_transaction()?;
    for (mod_id, mc_version, loader, available) in entries {
        tx.execute(
            r#"INSERT INTO modrinth_availability (project_id, mc_version, mod_loader, available)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(project_id, mc_version, mod_loader) DO UPDATE SET available = excluded.available"#,
            params![mod_id, mc_version, loader, available],
        )?;
    }
    tx.commit()?;
    Ok(())
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
    /// The rule itself is disabled: by the user in the editor, or by the
    /// availability pass because nothing installable exists for it. Before
    /// this value it came back as `ExcludedByActiveMod`, and the launch log
    /// said "excluded by already-selected mods" with nothing excluding it.
    Disabled,
    ExcludedByActiveMod,
    RequiredModMissing,
    IncompatibleVersion,
    NoOptionAvailable,
}

/// Voce pinnata → voce dinamica che ha preso il posto (audit 101, modlist-1).
///
/// Il pin è una voce locale con un altro mod_id (lo stem del jar), quindi un
/// `requires` o un `exclude_if` che nomina la voce dinamica non la vede mai
/// attiva sul target del pin: «iris requires sodium» lasciava fuori iris su
/// 1.20.1/fabric appena si pinnava sodium. Con questa mappa, quando la voce
/// pinnata è attiva conta come attiva anche la dinamica, per i soli controlli
/// dei vincoli: `active_mods` resta l'insieme dei mod caricati davvero. La
/// mappa la legge chi ha la cartella della mod list
/// (`mod_version_pin::pin_aliases`); vuota, la risoluzione è quella di sempre.
pub type PinAliases = HashMap<String, String>;

/// L'insieme con cui si controllano `requires` ed `exclude_if`: i mod attivi
/// più la voce dinamica di ogni pin attivo.
fn constraint_view<'a>(
    active_mods: &'a HashSet<String>,
    pins: &PinAliases,
) -> std::borrow::Cow<'a, HashSet<String>> {
    let aliases = active_mods
        .iter()
        .filter_map(|mod_id| pins.get(mod_id))
        .filter(|dynamic| !active_mods.contains(dynamic.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if aliases.is_empty() {
        return std::borrow::Cow::Borrowed(active_mods);
    }
    let mut view = active_mods.clone();
    view.extend(aliases);
    std::borrow::Cow::Owned(view)
}

pub fn resolve_modlist(modlist: &ModList, target: &ResolutionTarget) -> Result<ResolutionResult> {
    resolve_modlist_pinned(modlist, &PinAliases::new(), target)
}

/// `resolve_modlist` con i pin della mod list (`PinAliases`).
pub fn resolve_modlist_pinned(
    modlist: &ModList,
    pins: &PinAliases,
    target: &ResolutionTarget,
) -> Result<ResolutionResult> {
    // ── Pre-process: strip mutual requires ──────────────────────────────────
    // Mutual links (A requires B AND B requires A) create a deadlock — neither
    // can activate first.  We detect these pairs across the entire rule tree
    // and remove them so the normal resolution handles everything correctly.
    let mut rules = modlist.rules.clone();
    strip_mutual_requires(&mut rules);

    // ── Pass 1: sequential resolve to build the initial active set ────────────
    let mut active_mods = HashSet::new();
    let mut resolved_rules: Vec<ResolvedRule> = Vec::with_capacity(rules.len());

    for rule in &rules {
        let outcome = try_resolve(rule, &constraint_view(&active_mods, pins), target);
        if let RuleOutcome::Resolved { ref resolved_id } = outcome {
            active_mods.insert(resolved_id.clone());
        }
        resolved_rules.push(ResolvedRule {
            mod_id: rule.mod_id.clone(),
            outcome,
        });
    }

    // ── Pass 2..n: re-check each rule against the previous pass's set ────────
    // Una passata sola non basta (audit 101, risoluzione-4): la passata 2
    // confronta con l'insieme della passata 1, e un mod che entra solo nella
    // passata 2 può far cadere l'`exclude_if` o il `requires` di un altro.
    // Si ripete finché l'esito non cambia più: a quel punto ogni regola
    // risolta è valutata contro l'insieme finale, che quindi è coerente. Se
    // l'esito torna su uno già visto (un ciclo, es. A requires B e B
    // exclude_if A) o le passate finiscono, `repair_inconsistent` toglie in
    // ordine di regola i mod che violano i vincoli, in modo deterministico.
    let mut history = vec![resolved_rules.clone()];
    let mut consistent = false;
    for _ in 0..MAX_RESOLUTION_PASSES {
        let mut next_active = HashSet::new();
        let mut next_rules = Vec::with_capacity(rules.len());
        let previous = constraint_view(&active_mods, pins);
        for rule in &rules {
            let outcome = try_resolve(rule, &previous, target);
            if let RuleOutcome::Resolved { ref resolved_id } = outcome {
                next_active.insert(resolved_id.clone());
            }
            next_rules.push(ResolvedRule {
                mod_id: rule.mod_id.clone(),
                outcome,
            });
        }

        drop(previous);
        let fixed_point = next_rules == resolved_rules;
        let cycle = !fixed_point && history.contains(&next_rules);
        resolved_rules = next_rules;
        active_mods = next_active;
        if fixed_point {
            consistent = true;
            break;
        }
        if cycle {
            break;
        }
        history.push(resolved_rules.clone());
    }

    if !consistent {
        active_mods = repair_inconsistent(&rules, pins, &mut resolved_rules);
    }

    Ok(ResolutionResult {
        active_mods,
        resolved_rules,
    })
}

/// Bound on the re-check passes of `resolve_modlist`. Real mod-lists reach the
/// fixed point in two or three; the bound only matters for a cycle that the
/// history check has not caught yet.
const MAX_RESOLUTION_PASSES: usize = 32;

/// Ultima rete per un esito che non si è stabilizzato (un ciclo fra le
/// regole): toglie, in ordine di regola e finché ce n'è uno, ogni mod risolto
/// che sull'insieme corrente viola il proprio `exclude_if` o `requires`.
/// L'insieme può solo restringersi, quindi termina, e alla fine ogni mod
/// rimasto ha i suoi vincoli soddisfatti dall'insieme finale.
fn repair_inconsistent(
    rules: &[Rule],
    pins: &PinAliases,
    resolved_rules: &mut [ResolvedRule],
) -> HashSet<String> {
    let active_of = |resolved_rules: &[ResolvedRule]| {
        resolved_rules
            .iter()
            .filter_map(|rule| match &rule.outcome {
                RuleOutcome::Resolved { resolved_id } => Some(resolved_id.clone()),
                RuleOutcome::Unresolved { .. } => None,
            })
            .collect::<HashSet<_>>()
    };

    let mut active = active_of(resolved_rules);
    loop {
        let view = constraint_view(&active, pins);
        let violation = resolved_rules
            .iter()
            .enumerate()
            .find_map(|(index, resolved)| {
                let RuleOutcome::Resolved { resolved_id } = &resolved.outcome else {
                    return None;
                };
                let rule = find_rule_by_id(&rules[index], resolved_id)?;
                if rule.exclude_if.iter().any(|id| view.contains(id)) {
                    Some((index, FailureReason::ExcludedByActiveMod))
                } else if rule.requires.iter().any(|id| !view.contains(id)) {
                    Some((index, FailureReason::RequiredModMissing))
                } else {
                    None
                }
            });
        drop(view);
        let Some((index, reason)) = violation else {
            return active;
        };
        resolved_rules[index].outcome = RuleOutcome::Unresolved { reason };
        active = active_of(resolved_rules);
    }
}

/// `resolve_modlist` with the given mods disabled wherever they appear in the
/// tree: the re-resolution that gives alternatives their chance once a mod
/// turns out to have nothing installable for the target.
pub fn resolve_modlist_without(
    modlist: &ModList,
    unavailable: &[String],
    target: &ResolutionTarget,
) -> Result<ResolutionResult> {
    resolve_modlist_without_pinned(modlist, &PinAliases::new(), unavailable, target)
}

/// `resolve_modlist_without` con i pin della mod list.
pub fn resolve_modlist_without_pinned(
    modlist: &ModList,
    pins: &PinAliases,
    unavailable: &[String],
    target: &ResolutionTarget,
) -> Result<ResolutionResult> {
    fn disable(rules: &mut [Rule], unavailable: &HashSet<&str>) {
        for rule in rules {
            if unavailable.contains(rule.mod_id.as_str()) {
                rule.enabled = false;
            }
            disable(&mut rule.alternatives, unavailable);
        }
    }

    let unavailable = unavailable
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut patched = modlist.clone();
    disable(&mut patched.rules, &unavailable);
    resolve_modlist_pinned(&patched, pins, target)
}

/// La disponibilità e la ri-risoluzione iterate fino al punto fisso (audit
/// 101, risoluzione-2).
///
/// Interrogare una volta sola la prima selezione lasciava fuori la catena di
/// fallback: con «sodium» e le alternative [embeddium, rubidium], se sodium
/// non ha versione la ri-risoluzione sceglie embeddium senza chiedere se
/// esiste, e rubidium non viene mai interrogato. Qui ogni mod che una
/// risoluzione seleziona viene chiesto prima che la selezione valga: chi non è
/// disponibile si disabilita, si risolve di nuovo, e si chiedono i mod nuovi
/// che la ri-risoluzione ha portato dentro, finché non ne arrivano più.
///
/// Termina: ogni giro chiede almeno un mod mai chiesto, e i mod dell'albero
/// sono finiti. È deterministico: l'ordine è quello della risoluzione, e un mod
/// già chiesto non si richiede (la risposta non cambia dentro un lancio).
///
/// La domanda — rete, cache, o tutte e due — è del chiamante: questo tipo
/// sa solo chi chiedere (`pending`) e cosa fare della risposta (`record`), così
/// il lancio online, quello offline, il pre-check e l'editor fanno la stessa
/// sequenza (D146).
#[derive(Debug, Clone)]
pub struct AvailabilityFixpoint<'a> {
    modlist: &'a ModList,
    pins: PinAliases,
    target: &'a ResolutionTarget,
    first: ResolutionResult,
    current: ResolutionResult,
    unavailable: Vec<String>,
    asked: HashSet<String>,
}

impl<'a> AvailabilityFixpoint<'a> {
    pub fn new(modlist: &'a ModList, target: &'a ResolutionTarget) -> Result<Self> {
        Self::pinned(modlist, PinAliases::new(), target)
    }

    /// `new` con i pin della mod list (`PinAliases`), per ogni giro.
    pub fn pinned(
        modlist: &'a ModList,
        pins: PinAliases,
        target: &'a ResolutionTarget,
    ) -> Result<Self> {
        let first = resolve_modlist_pinned(modlist, &pins, target)?;
        Ok(Self {
            modlist,
            pins,
            target,
            current: first.clone(),
            first,
            unavailable: Vec::new(),
            asked: HashSet::new(),
        })
    }

    /// The Modrinth mods the current resolution selects and whose availability
    /// nobody asked yet, once each, in resolution order. Empty means the fixed
    /// point: every selected mod has been asked and none was unavailable.
    pub fn pending(&self) -> Vec<String> {
        let mut pending = Vec::new();
        for (index, resolved) in self.current.resolved_rules.iter().enumerate() {
            let RuleOutcome::Resolved { resolved_id } = &resolved.outcome else {
                continue;
            };
            let is_modrinth = self
                .modlist
                .find_rule(resolved_id)
                .or_else(|| self.modlist.rules.get(index))
                .is_some_and(|rule| rule.source == ModSource::Modrinth);
            if is_modrinth && !self.asked.contains(resolved_id) && !pending.contains(resolved_id) {
                pending.push(resolved_id.clone());
            }
        }
        pending
    }

    /// The answer for `asked`: `unavailable` are the ones among them with
    /// nothing installable. Anything new re-resolves with every unavailable mod
    /// so far disabled.
    pub fn record(&mut self, asked: &[String], unavailable: &[String]) -> Result<()> {
        self.asked.extend(asked.iter().cloned());
        let mut changed = false;
        for mod_id in unavailable {
            if !self.unavailable.contains(mod_id) {
                self.unavailable.push(mod_id.clone());
                changed = true;
            }
        }
        if changed {
            self.current = resolve_modlist_without_pinned(
                self.modlist,
                &self.pins,
                &self.unavailable,
                self.target,
            )?;
        }
        Ok(())
    }

    /// The resolution before any mod was found unavailable.
    pub fn first(&self) -> &ResolutionResult {
        &self.first
    }

    /// The resolution the target actually loads.
    pub fn current(&self) -> &ResolutionResult {
        &self.current
    }

    /// Every mod found unavailable, in the order it was found.
    pub fn unavailable(&self) -> &[String] {
        &self.unavailable
    }

    /// `(first, final, unavailable)`.
    pub fn into_parts(self) -> (ResolutionResult, ResolutionResult, Vec<String>) {
        (self.first, self.current, self.unavailable)
    }
}

/// Every `(mod_id, requires)` pair of the rule tree, plus each mod's enabled state.
type CollectedRequires = (Vec<(String, Vec<String>)>, HashMap<String, bool>);

/// Collect all mod IDs, their requires, and enabled states across the entire rule tree.
fn collect_all_requires(rules: &[Rule]) -> CollectedRequires {
    let mut requires = Vec::new();
    let mut enabled = HashMap::new();
    for rule in rules {
        requires.push((rule.mod_id.clone(), rule.requires.clone()));
        enabled.insert(rule.mod_id.clone(), rule.enabled);
        let (alternative_requires, alternative_enabled) = collect_all_requires(&rule.alternatives);
        requires.extend(alternative_requires);
        enabled.extend(alternative_enabled);
    }
    (requires, enabled)
}

/// Detect mutual requires (A requires B AND B requires A) and remove both
/// directions so the resolver doesn't deadlock.
fn strip_mutual_requires(rules: &mut [Rule]) {
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
    fn remove_mutual(rules: &mut [Rule], pairs: &HashSet<(String, String)>) {
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
        return try_alternatives(rule, active_mods, target, FailureReason::Disabled);
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
///
/// Il verde e il rosso dell'editor sono quello che il lancio carica (D146,
/// audit 101 editor-logica-4): la stessa sequenza del lancio online —
/// `AvailabilityFixpoint`, cioè disponibilità, ri-risoluzione, e di nuovo
/// finché la selezione non cambia — e quindi solo la prima alternativa
/// viabile di ogni catena, mai tutte. Prima l'editor toglieva gli
/// indisponibili senza ri-risolvere e poi coloriva verde ogni alternativa
/// viabile, mentre il lancio ne carica una.
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

    // I pin contano come la voce dinamica che sostituiscono, come al lancio
    // (audit 101, modlist-1).
    let pins = crate::mod_version_pin::pin_aliases(
        &launcher_paths.modlists_dir().join(&modlist_name),
        &modlist,
    );
    // D140, D146: with the active account playing offline only the cache
    // answers, and with the launch's own rule: a mod is available when its jar
    // for this target is on disk (review-risoluzione-7). The availability rows
    // describe Modrinth, not what an offline launch can install.
    let plays_offline = crate::app_shell::active_account_plays_offline(&launcher_paths)
        .map_err(|e| e.to_string())?;
    if plays_offline {
        return crate::launch_preview::offline_active_mods(
            &launcher_paths,
            &modlist,
            &pins,
            &target,
        )
        .map_err(|e| e.to_string());
    }
    let client = ModrinthClient::new();
    let mut reach = availability_check_guard(&modlist_name, &target).await;
    let mut pass =
        AvailabilityFixpoint::pinned(&modlist, pins, &target).map_err(|e| e.to_string())?;
    loop {
        let pending = pass.pending();
        if pending.is_empty() {
            break;
        }
        let check_client = client.clone();
        let check_target = target.clone();
        // review-risoluzione-9: once a round met no connection, the rest of
        // this fixed point (and the checks queued behind it, for a minute)
        // answer from the cache.
        let (unavailable, unreachable) = editor_unavailable_mods(
            &launcher_paths,
            &pending,
            &target,
            &mod_loader,
            reach.may_ask(),
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
        if unreachable {
            reach.mark_unreachable();
        }
        pass.record(&pending, &unavailable)
            .map_err(|e| e.to_string())?;
    }

    Ok(pass.current().active_mods.iter().cloned().collect())
}

/// The editor's availability verdict for `mod_ids`, the mods one round of the
/// fixed point selected: the ones with nothing installable, in input order.
///
/// Same meaning as the online launch: a compatible version on Modrinth, or a
/// jar for this exact target already on disk (D29, `CacheBacking::JarOnDisk`).
/// The Modrinth half reads `modrinth_availability` first and asks `check` only
/// for the mods with no row, persisting the answers. A failed question is not
/// an answer: the mod stays available and nothing is cached, as before — an
/// editor that turns red because the network blinked would say something the
/// next launch may well not do. Without `ask_modrinth` (Modrinth was just
/// found out of reach, review-risoluzione-9) only the cached rows answer, and
/// a mod with no row stays available. The second value says whether this call
/// found Modrinth out of reach.
async fn editor_unavailable_mods<Check, Fut>(
    launcher_paths: &LauncherPaths,
    mod_ids: &[String],
    target: &ResolutionTarget,
    loader_key: &str,
    ask_modrinth: bool,
    check: Check,
) -> (Vec<String>, bool)
where
    Check: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
{
    let db_path = launcher_paths.database_path();
    let cached = db_availability_get(db_path, mod_ids, &target.minecraft_version, loader_key);
    let cached_ids = cached
        .iter()
        .map(|(mod_id, _)| mod_id.as_str())
        .collect::<HashSet<_>>();
    let uncached = mod_ids
        .iter()
        .filter(|mod_id| !cached_ids.contains(mod_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    let mut answers = cached.clone();
    let mut unreachable = false;
    if !uncached.is_empty() && ask_modrinth {
        let checked = check_availability_persisting(uncached, 10, check, |mod_id, available| {
            persist_availability_answer(
                db_path,
                mod_id,
                &target.minecraft_version,
                loader_key,
                available,
            )
        })
        .await;
        unreachable = checked.unreachable;
        answers.extend(checked.answers);
    }

    let no_version = answers
        .into_iter()
        .filter(|(_, available)| !available)
        .map(|(mod_id, _)| mod_id)
        .collect::<HashSet<_>>();
    if no_version.is_empty() {
        return (Vec::new(), unreachable);
    }

    // D29: un jar per questo target sul disco tiene il mod nel lancio, qualunque
    // cosa dica Modrinth. Una cache illeggibile non tiene niente.
    let connection = Connection::open(db_path).ok();
    let jar_on_disk = |mod_id: &str| {
        connection.as_ref().is_some_and(|connection| {
            SqliteModCacheRepository::new(connection, launcher_paths.mods_cache_dir())
                .probe_project(mod_id, target)
                .is_ok_and(|probe| matches!(probe, CacheProbe::Ready(_)))
        })
    };

    let unavailable = mod_ids
        .iter()
        .filter(|mod_id| no_version.contains(mod_id.as_str()) && !jar_on_disk(mod_id))
        .cloned()
        .collect();
    (unavailable, unreachable)
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

fn split_ids_by_cached_hash(ids: &[String], hash_by_mod_id: &HashMap<String, String>) -> HashSplit {
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

/// The answers without a sink: what the tests of the concurrency bound use.
#[cfg(test)]
async fn check_availability_concurrently<Check, Fut>(
    mod_ids: Vec<String>,
    permits: usize,
    check: Check,
) -> Vec<(String, bool)>
where
    Check: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
{
    check_availability_persisting(mod_ids, permits, check, |_, _| {})
        .await
        .answers
}

/// What one round of availability questions answered.
#[derive(Debug, Default)]
struct AvailabilityAnswers {
    answers: Vec<(String, bool)>,
    /// A question met no connection at all, and the round stopped there
    /// (review-risoluzione-9).
    unreachable: bool,
}

/// `check_availability_concurrently`, handing each answer to `on_answer` as
/// soon as it arrives (audit 101, r2-scala-mod-list-grandi-1). Writing the
/// whole batch at the end left a second check that started meanwhile — a
/// toggle during the first resolution — with an empty cache, and it asked
/// every mod again.
///
/// The first question that finds no connection — refused, unresolved, silent —
/// stops the round, as precheck-5 does for the launch (review-risoluzione-9):
/// the next three hundred would each pay the same ~14 s of retries.
async fn check_availability_persisting<Check, Fut, OnAnswer>(
    mod_ids: Vec<String>,
    permits: usize,
    check: Check,
    mut on_answer: OnAnswer,
) -> AvailabilityAnswers
where
    Check: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
    OnAnswer: FnMut(&str, bool),
{
    let mut tasks = tokio::task::JoinSet::new();
    let semaphore = Arc::new(Semaphore::new(permits));

    for mod_id in mod_ids {
        let check = check.clone();
        let permit_source = semaphore.clone();
        tasks.spawn(async move {
            let _permit = permit_source.acquire_owned().await.ok()?;
            Some(match check(mod_id.clone()).await {
                Ok(available) => Ok((mod_id, available)),
                Err(error) => Err(crate::modrinth::is_connection_failure(&error)),
            })
        });
    }

    let mut outcome = AvailabilityAnswers::default();
    while let Some(join_result) = tasks.join_next().await {
        match join_result {
            Ok(Some(Ok(result))) => {
                on_answer(&result.0, result.1);
                outcome.answers.push(result);
            }
            Ok(Some(Err(true))) => {
                outcome.unreachable = true;
                tasks.abort_all();
            }
            _ => {}
        }
    }
    outcome
}

/// One availability check per (mod list, target) at a time (audit 101,
/// r2-scala-mod-list-grandi-1). The editor starts two — the background
/// backfill and the resolution — on the same ids, and a toggle starts a third;
/// each read the cache before the others had written it and asked Modrinth
/// for everything, 2×N requests and more against a limit of 300 a minute.
/// Serialised, the second one finds the first one's answers in the cache.
///
/// The lock also remembers when Modrinth was last found out of reach for this
/// (mod list, target) (review-risoluzione-9). The checks queued behind one
/// that met no connection then answer from the cache for a short while,
/// instead of each paying the same storm of retries.
async fn availability_check_guard(
    modlist_name: &str,
    target: &ResolutionTarget,
) -> tokio::sync::OwnedMutexGuard<ModrinthReach> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<ModrinthReach>>>>,
    > = std::sync::OnceLock::new();

    let key = format!(
        "{modlist_name}\u{0}{}\u{0}{}",
        target.minecraft_version,
        target.mod_loader.as_modrinth_loader()
    );
    let lock = {
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(key).or_default().clone()
    };
    lock.lock_owned().await
}

/// How long the availability checks of one (mod list, target) stop asking
/// after one found no connection (review-risoluzione-9).
const UNREACHABLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether the availability checks may ask Modrinth, kept under the lock of
/// `availability_check_guard`.
#[derive(Debug, Default)]
struct ModrinthReach {
    unreachable_until: Option<std::time::Instant>,
}

impl ModrinthReach {
    fn may_ask(&self) -> bool {
        self.unreachable_until
            .is_none_or(|until| std::time::Instant::now() >= until)
    }

    fn mark_unreachable(&mut self) {
        self.unreachable_until = Some(std::time::Instant::now() + UNREACHABLE_PAUSE);
    }
}

/// The (mod, target) pairs Modrinth was asked about since the app started.
/// The backfill re-checks a mod cached as unavailable — authors add versions
/// later — but once per session, not at every edit that reloads the editor
/// (audit 101, r2-scala-mod-list-grandi-1).
fn asked_this_session() -> &'static std::sync::Mutex<HashSet<(String, String, String)>> {
    static ASKED: std::sync::OnceLock<std::sync::Mutex<HashSet<(String, String, String)>>> =
        std::sync::OnceLock::new();
    ASKED.get_or_init(Default::default)
}

fn mark_asked_this_session(mod_id: &str, mc_version: &str, loader: &str) {
    asked_this_session()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((
            mod_id.to_string(),
            mc_version.to_string(),
            loader.to_string(),
        ));
}

fn was_asked_this_session(mod_id: &str, mc_version: &str, loader: &str) -> bool {
    asked_this_session()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&(
            mod_id.to_string(),
            mc_version.to_string(),
            loader.to_string(),
        ))
}

/// Persist one answer as it arrives, and remember it was asked.
fn persist_availability_answer(
    db_path: &Path,
    mod_id: &str,
    mc_version: &str,
    loader: &str,
    available: bool,
) {
    mark_asked_this_session(mod_id, mc_version, loader);
    db_availability_set(
        db_path,
        &[(
            mod_id.to_string(),
            mc_version.to_string(),
            loader.to_string(),
            available,
        )],
    );
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

    let target = ResolutionTarget {
        minecraft_version: mc_version.to_string(),
        mod_loader: parsed_loader,
    };
    let mut reach = availability_check_guard(modlist_name, &target).await;
    // review-risoluzione-9: a check that just met no connection; the cache is
    // what there is until the pause ends.
    if !reach.may_ask() {
        return Ok(BackfillOutcome::default());
    }

    let db_path = launcher_paths.database_path();
    let cached = db_availability_get(db_path, &all_modrinth_ids, mc_version, mod_loader);
    // Only skip mods that are cached as *available*. Mods cached as
    // unavailable are re-checked because mod authors frequently add
    // version support after initial release — once per session, not at every
    // reload of the editor (audit 101, r2-scala-mod-list-grandi-1).
    let skip_ids: HashSet<String> = cached
        .iter()
        .filter(|(id, available)| *available || was_asked_this_session(id, mc_version, mod_loader))
        .map(|(id, _)| id.clone())
        .collect();
    let mut seen_ids = HashSet::new();
    let ids_to_check = all_modrinth_ids
        .into_iter()
        .filter(|id| !skip_ids.contains(id) && seen_ids.insert(id.clone()))
        .collect::<Vec<_>>();

    if ids_to_check.is_empty() {
        return Ok(BackfillOutcome::default());
    }

    // Opening or querying mod_cache may fail; fall back to per-project requests.
    let hash_by_mod_id =
        load_cached_hashes_for_backfill(launcher_paths, &ids_to_check, &target).unwrap_or_default();
    let split = split_ids_by_cached_hash(&ids_to_check, &hash_by_mod_id);
    let bulk_hash_count = split.hashed.len();
    // Availability only asks whether a compatible version exists, so the
    // release → beta → alpha installation preference does not apply here.
    let bulk_unreachable = std::sync::atomic::AtomicBool::new(false);
    let bulk_outcome = resolve_bulk_availability(&split, |hashes| {
        let target = &target;
        let bulk_unreachable = &bulk_unreachable;
        async move {
            let answer = client
                .fetch_versions_by_hash_any_channel(&hashes, target)
                .await;
            if answer
                .as_ref()
                .is_err_and(crate::modrinth::is_connection_failure)
            {
                bulk_unreachable.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            answer
        }
    })
    .await;
    if bulk_unreachable.load(std::sync::atomic::Ordering::Relaxed) {
        // No connection: the per-project fallback would pay the same retries
        // once per mod (review-risoluzione-9).
        reach.mark_unreachable();
        eprintln!(
            "[Backfill] Modrinth could not be reached; the editor uses the cached availability"
        );
        return Ok(BackfillOutcome {
            bulk_hash_count,
            bulk_error: bulk_outcome.bulk_error,
            ..BackfillOutcome::default()
        });
    }

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

    // Each answer reaches the cache as soon as it is known (audit 101,
    // r2-scala-mod-list-grandi-1): the bulk ones now, the per-project ones one
    // by one, so a check that runs next finds them.
    let bulk_entries = bulk_outcome
        .available_mod_ids
        .iter()
        .map(|mod_id| {
            mark_asked_this_session(mod_id, mc_version, mod_loader);
            (
                mod_id.clone(),
                mc_version.to_string(),
                mod_loader.to_string(),
                true,
            )
        })
        .collect::<Vec<_>>();
    if !bulk_entries.is_empty() {
        db_availability_set(db_path, &bulk_entries);
    }

    let per_project_request_count = bulk_outcome.per_project_mod_ids.len();
    let check_client = client.clone();
    let check_target = target.clone();
    // Match resolve_modlist_command's limit so both paths share the same API pressure.
    let per_project = check_availability_persisting(
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
        |mod_id, available| {
            persist_availability_answer(db_path, mod_id, mc_version, mod_loader, available)
        },
    )
    .await;
    if per_project.unreachable {
        reach.mark_unreachable();
    }
    let per_project = per_project.answers;
    let persisted = merge_backfill_entries(&bulk_outcome.available_mod_ids, &per_project);

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
    // D140: nothing to fill from Modrinth while the active account plays
    // offline; the editor reads the availability already cached.
    if crate::app_shell::active_account_plays_offline(&launcher_paths).map_err(|e| e.to_string())? {
        return Ok(());
    }
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
        let per_project =
            check_availability_concurrently(bulk.per_project_mod_ids.clone(), 10, move |_mod_id| {
                let calls = recorded_per_project_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                }
            })
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
        let per_project =
            check_availability_concurrently(bulk.per_project_mod_ids.clone(), 10, move |_mod_id| {
                let calls = recorded_per_project_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                }
            })
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
        let split = split_ids_by_cached_hash(&ids, &cached_hashes(&[("sodium", "hash-a")]));
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

    /// Audit 101, r2-scala-mod-list-grandi-1: each answer reaches its sink as
    /// it arrives, and a failed question is no answer.
    #[tokio::test]
    async fn every_answer_is_handed_over_as_it_arrives() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let answers = check_availability_persisting(
            vec!["sodium".into(), "broken".into(), "iris".into()],
            2,
            |mod_id| async move {
                match mod_id.as_str() {
                    "broken" => Err(anyhow::anyhow!("request failed")),
                    "iris" => Ok(false),
                    _ => Ok(true),
                }
            },
            move |mod_id, available| sink.lock().unwrap().push((mod_id.to_string(), available)),
        )
        .await;

        let mut handed = seen.lock().unwrap().clone();
        handed.sort();
        assert!(
            !answers.unreachable,
            "a request error is not a dead network"
        );
        let mut returned = answers.answers;
        returned.sort();
        assert_eq!(handed, returned);
        assert_eq!(
            handed,
            vec![("iris".to_string(), false), ("sodium".to_string(), true)]
        );
    }

    /// A real connection failure: a request to a port nobody listens on.
    async fn refused_connection() -> anyhow::Error {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        let error = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("nothing listens there");
        anyhow::Error::new(error).context("failed to query Modrinth")
    }

    /// review-risoluzione-9: the first question that finds no connection stops
    /// the round; the rest are never asked.
    #[tokio::test]
    async fn a_connection_failure_stops_the_round() {
        let refused = Arc::new(tokio::sync::Mutex::new(Some(refused_connection().await)));
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let mod_ids = (0..50).map(|n| format!("mod-{n}")).collect::<Vec<_>>();
        let answers = check_availability_persisting(
            mod_ids,
            1,
            move |_mod_id| {
                let refused = refused.clone();
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    match refused.lock().await.take() {
                        Some(error) => Err(error),
                        None => {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            Ok(true)
                        }
                    }
                }
            },
            |_, _| {},
        )
        .await;

        assert!(answers.unreachable);
        assert!(
            asked.load(Ordering::SeqCst) < 5,
            "asked {} mods after the network was found down",
            asked.load(Ordering::SeqCst)
        );
    }

    /// review-risoluzione-9: after a round that found no connection, the
    /// checks of that (mod list, target) answer from the cache for a while.
    #[test]
    fn a_dead_network_pauses_the_questions() {
        let mut reach = ModrinthReach::default();
        assert!(reach.may_ask());
        reach.mark_unreachable();
        assert!(!reach.may_ask());
        reach.unreachable_until = Some(std::time::Instant::now());
        assert!(reach.may_ask());
    }

    /// Two checks of one mod list on one target run one after the other; on
    /// another target they do not wait for each other.
    #[tokio::test]
    async fn one_availability_check_per_modlist_and_target_at_a_time() {
        let fabric = target();
        let first = availability_check_guard("Scale Pack", &fabric).await;

        let waiting = tokio::spawn({
            let fabric = fabric.clone();
            async move {
                let _second = availability_check_guard("Scale Pack", &fabric).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiting.is_finished(),
            "the second check must wait for the first"
        );

        let other_target = ResolutionTarget {
            minecraft_version: format!("{}-other", fabric.minecraft_version),
            ..fabric.clone()
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            availability_check_guard("Scale Pack", &other_target),
        )
        .await
        .expect("another target does not wait");

        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the second check runs once the first is done")
            .expect("task");
    }

    #[test]
    fn a_mod_asked_once_is_remembered_for_the_session() {
        assert!(!was_asked_this_session(
            "session-mod",
            "1.20.1",
            "fabric-session-test"
        ));
        mark_asked_this_session("session-mod", "1.20.1", "fabric-session-test");
        assert!(was_asked_this_session(
            "session-mod",
            "1.20.1",
            "fabric-session-test"
        ));
        assert!(!was_asked_this_session(
            "session-mod",
            "1.21.1",
            "fabric-session-test"
        ));
    }

    /// The bug this pass exists to close: the per-project stage used to spawn
    /// one unbounded task per mod, so a 300-mod modlist could put 300 requests
    /// in flight against a 300-per-minute limit.
    #[tokio::test]
    async fn per_project_checks_never_exceed_the_permitted_concurrency() {
        let mod_ids = (0..24)
            .map(|index| format!("mod-{index}"))
            .collect::<Vec<_>>();
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

    fn temp_db_path(label: &str) -> std::path::PathBuf {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cubic-resolver-{label}-{}-{timestamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("launcher.db")
    }

    #[test]
    fn availability_rows_are_saved_and_a_failed_save_is_an_error_not_a_silence() {
        let entries = vec![
            ("sodium".to_string(), "1.20.1".into(), "fabric".into(), true),
            ("iris".to_string(), "1.20.1".into(), "fabric".into(), false),
        ];

        let db_path = temp_db_path("availability");
        crate::database::initialize_database(&db_path).expect("initialize database");
        try_db_availability_set(&db_path, &entries).expect("the rows are saved");
        let mut saved = db_availability_get(
            &db_path,
            &["sodium".to_string(), "iris".to_string()],
            "1.20.1",
            "fabric",
        );
        saved.sort();
        assert_eq!(
            saved,
            vec![("iris".to_string(), false), ("sodium".to_string(), true)]
        );

        // Un database senza la tabella: prima `let _` lo ingoiava.
        let bare = temp_db_path("availability-bare");
        assert!(try_db_availability_set(&bare, &entries).is_err());

        for path in [db_path, bare] {
            std::fs::remove_dir_all(path.parent().unwrap()).expect("remove temp dir");
        }
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
    /// I vincoli di ogni mod caricato valgono sull'insieme **finale**: è la
    /// proprietà che la passata unica violava (audit 101, risoluzione-4).
    fn assert_consistent(ml: &ModList, result: &ResolutionResult) {
        for id in &result.active_mods {
            let rule = ml.find_rule(id).expect("an active mod comes from the tree");
            for excluded_by in &rule.exclude_if {
                assert!(
                    !result.active_mods.contains(excluded_by),
                    "'{id}' is loaded next to '{excluded_by}', which its exclude_if forbids"
                );
            }
            for required in &rule.requires {
                assert!(
                    result.active_mods.contains(required),
                    "'{id}' is loaded without '{required}', which it requires"
                );
            }
        }
    }

    fn fabric_1_20_1() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.20.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    #[test]
    fn an_excluded_mod_does_not_stay_next_to_a_mod_that_entered_in_the_second_pass() {
        // Report 101, risoluzione-4, primo caso: [optifabric exclude_if [iris],
        // iris requires [sodium], sodium]. La passata unica dava {optifabric,
        // iris, sodium}: due mod incompatibili caricati insieme.
        let mut optifabric = simple_rule("optifabric");
        optifabric.exclude_if = vec!["iris".into()];
        let mut iris = simple_rule("iris");
        iris.requires = vec!["sodium".into()];
        let ml = modlist(vec![optifabric, iris, simple_rule("sodium")]);

        let result = resolve_modlist(&ml, &fabric_1_20_1()).unwrap();

        assert_consistent(&ml, &result);
        assert_eq!(
            result.active_mods,
            HashSet::from(["iris".to_string(), "sodium".to_string()])
        );
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::ExcludedByActiveMod,
            }
        );
    }

    #[test]
    fn a_mod_does_not_stay_without_a_requirement_that_left_in_the_second_pass() {
        // Report 101, risoluzione-4, secondo caso: [iris requires sodium, sodium
        // exclude_if [optifabric], optifabric]. La passata unica dava {iris,
        // optifabric}: Iris senza Sodium, e il client crasha.
        let mut iris = simple_rule("iris");
        iris.requires = vec!["sodium".into()];
        let mut sodium = simple_rule("sodium");
        sodium.exclude_if = vec!["optifabric".into()];
        let ml = modlist(vec![iris, sodium, simple_rule("optifabric")]);

        let result = resolve_modlist(&ml, &fabric_1_20_1()).unwrap();

        assert_consistent(&ml, &result);
        assert_eq!(
            result.active_mods,
            HashSet::from(["optifabric".to_string()])
        );
        assert_eq!(
            result.resolved_rules[0].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::RequiredModMissing,
            }
        );
    }

    #[test]
    fn a_cycle_between_rules_ends_in_a_consistent_set_and_always_the_same_one() {
        // A requires B, B exclude_if A: le passate girano {B} → {A,B} → {A} →
        // {} → {B}. Il ciclo si ferma e la riparazione dà un insieme coerente,
        // lo stesso a ogni chiamata.
        let mut a = simple_rule("A");
        a.requires = vec!["B".into()];
        let mut b = simple_rule("B");
        b.exclude_if = vec!["A".into()];
        let ml = modlist(vec![a, b]);

        let first = resolve_modlist(&ml, &target()).unwrap();
        let second = resolve_modlist(&ml, &target()).unwrap();

        assert_consistent(&ml, &first);
        assert_eq!(first, second);
        assert_eq!(first.active_mods, HashSet::from(["B".to_string()]));
    }

    #[test]
    fn a_mod_that_excludes_itself_is_left_out_instead_of_oscillating() {
        let mut a = simple_rule("A");
        a.exclude_if = vec!["A".into()];
        let ml = modlist(vec![a, simple_rule("B")]);

        let result = resolve_modlist(&ml, &target()).unwrap();

        assert_consistent(&ml, &result);
        assert_eq!(result.active_mods, HashSet::from(["B".to_string()]));
    }

    /// Il punto fisso con una disponibilità scritta a mano: restituisce i mod
    /// caricati e quante volte è stata fatta la domanda per ciascun mod.
    fn run_fixpoint(
        ml: &ModList,
        target: &ResolutionTarget,
        unavailable: &[&str],
    ) -> (HashSet<String>, HashMap<String, usize>, Vec<String>) {
        let mut pass = AvailabilityFixpoint::new(ml, target).unwrap();
        let mut asked = HashMap::<String, usize>::new();
        loop {
            let pending = pass.pending();
            if pending.is_empty() {
                break;
            }
            for mod_id in &pending {
                *asked.entry(mod_id.clone()).or_default() += 1;
            }
            let missing = pending
                .iter()
                .filter(|mod_id| unavailable.contains(&mod_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            pass.record(&pending, &missing).unwrap();
        }
        let (_, last, unavailable) = pass.into_parts();
        (last.active_mods, asked, unavailable)
    }

    #[test]
    fn the_fallback_chain_goes_past_an_unavailable_first_alternative() {
        // Report 101, risoluzione-2: «sodium» con alternative [embeddium,
        // rubidium] su forge 1.16.5, sodium ed embeddium senza versione. Prima
        // embeddium entrava senza verifica e il gioco partiva senza nessun mod
        // della catena; rubidium non veniva mai chiesto.
        let mut sodium = simple_rule("sodium");
        sodium.alternatives = vec![simple_rule("embeddium"), simple_rule("rubidium")];
        let ml = modlist(vec![sodium, simple_rule("lithium")]);
        let target = ResolutionTarget {
            minecraft_version: "1.16.5".into(),
            mod_loader: ModLoader::Forge,
        };

        let (loaded, asked, unavailable) = run_fixpoint(&ml, &target, &["sodium", "embeddium"]);

        assert_eq!(
            loaded,
            HashSet::from(["rubidium".to_string(), "lithium".to_string()])
        );
        assert_eq!(unavailable, vec!["sodium", "embeddium"]);
        // Ogni mod si chiede una volta sola, anche se la risoluzione gira tre volte.
        assert!(asked.values().all(|count| *count == 1), "{asked:?}");
        assert_eq!(asked.len(), 4);
    }

    #[test]
    fn a_chain_with_nothing_available_loads_nothing_of_it() {
        let mut sodium = simple_rule("sodium");
        sodium.alternatives = vec![simple_rule("embeddium"), simple_rule("rubidium")];
        let ml = modlist(vec![sodium]);

        let (loaded, _, _) = run_fixpoint(&ml, &target(), &["sodium", "embeddium", "rubidium"]);

        assert!(loaded.is_empty());
    }

    #[test]
    fn a_fixpoint_with_everything_available_asks_once_and_keeps_the_first_resolution() {
        let mut sodium = simple_rule("sodium");
        sodium.alternatives = vec![simple_rule("embeddium")];
        let mut local = simple_rule("my-jar");
        local.source = ModSource::Local;
        let ml = modlist(vec![sodium, local]);

        let (loaded, asked, unavailable) = run_fixpoint(&ml, &target(), &[]);

        assert_eq!(
            loaded,
            HashSet::from(["sodium".to_string(), "my-jar".to_string()])
        );
        // Un mod locale non si chiede a Modrinth; un'alternativa non selezionata nemmeno.
        assert_eq!(asked, HashMap::from([("sodium".to_string(), 1)]));
        assert!(unavailable.is_empty());
    }

    #[test]
    fn an_unavailable_mod_is_disabled_everywhere_it_appears_in_the_tree() {
        // Lo stesso id come alternativa di due regole. `rules.json` lo rifiuta
        // in lettura, ma un `ModList` in memoria no: disabilitare solo la prima
        // occorrenza (`find_rule_mut`) lo lascerebbe rientrare dalla seconda
        // senza domanda.
        let mut first = simple_rule("sodium");
        first.alternatives = vec![simple_rule("embeddium")];
        let mut second = simple_rule("rubidium");
        second.alternatives = vec![simple_rule("embeddium")];
        let ml = modlist(vec![first, second]);

        let without = resolve_modlist_without(
            &ml,
            &[
                "sodium".to_string(),
                "rubidium".to_string(),
                "embeddium".to_string(),
            ],
            &target(),
        )
        .unwrap();

        assert!(without.active_mods.is_empty());
        assert_eq!(
            without.resolved_rules[1].outcome,
            RuleOutcome::Unresolved {
                reason: FailureReason::Disabled,
            }
        );
    }

    #[test]
    fn the_editor_colours_the_loser_of_an_incompatibility_the_launch_loads() {
        // Report 101, editor-logica-4, primo caso: «W vince su L» (L exclude_if
        // [W]) con W senza release per il target. L'editor toglieva W senza
        // ri-risolvere e coloriva verde L2, l'alternativa di L; il lancio
        // disabilita W e installa L. Con il punto fisso l'editor carica L.
        let mut loser = simple_rule("L");
        loser.exclude_if = vec!["W".into()];
        loser.alternatives = vec![simple_rule("L2")];
        let ml = modlist(vec![simple_rule("W"), loser]);

        let (loaded, _, _) = run_fixpoint(&ml, &target(), &["W"]);

        assert_eq!(loaded, HashSet::from(["L".to_string()]));
    }

    #[test]
    fn the_editor_colours_only_the_first_viable_fallback() {
        // Secondo caso: P non disponibile, A1 e A2 tutte e due valide.
        // L'editor le coloriva verdi tutte e due, il lancio installa solo A1.
        let mut primary = simple_rule("P");
        primary.alternatives = vec![simple_rule("A1"), simple_rule("A2")];
        let ml = modlist(vec![primary]);

        let (loaded, asked, _) = run_fixpoint(&ml, &target(), &["P"]);

        assert_eq!(loaded, HashSet::from(["A1".to_string()]));
        assert!(!asked.contains_key("A2"));
    }

    #[tokio::test]
    async fn the_editor_availability_reads_the_cache_and_keeps_a_jar_on_disk() {
        let db_path = temp_db_path("editor-availability");
        let launcher_paths = LauncherPaths::new(db_path.parent().unwrap());
        std::fs::create_dir_all(launcher_paths.mods_cache_dir()).expect("mods cache");
        crate::database::initialize_database(launcher_paths.database_path())
            .expect("initialize database");
        let target = target();
        db_availability_set(
            launcher_paths.database_path(),
            &[
                ("sodium".into(), "1.21.1".into(), "fabric".into(), true),
                ("iris".into(), "1.21.1".into(), "fabric".into(), false),
                ("lithium".into(), "1.21.1".into(), "fabric".into(), false),
            ],
        );
        // `lithium` non ha versione per Modrinth, ma il suo jar per questo
        // target è sul disco: il lancio lo carica (D29), quindi è verde.
        {
            let connection =
                Connection::open(launcher_paths.database_path()).expect("open database");
            let repository =
                SqliteModCacheRepository::new(&connection, launcher_paths.mods_cache_dir());
            let mut version = modrinth_version("lithium-v1");
            version.project_id = "lithium".into();
            version.files = vec![crate::modrinth::ModrinthFile {
                hashes: HashMap::from([("sha1".to_string(), "lithium-sha1".to_string())]),
                url: "https://cdn.modrinth.com/data/lithium/lithium.jar".into(),
                filename: "lithium.jar".into(),
                primary: true,
                size: 3,
            }];
            repository
                .upsert_modrinth_version(&version, &target)
                .expect("register lithium");
            let record = repository
                .find_registered_by_project_or_alias("lithium", &target)
                .expect("read the row")
                .expect("the row exists");
            let path = crate::mod_cache::cached_artifact_path_for_record(
                launcher_paths.mods_cache_dir(),
                &record,
            );
            std::fs::create_dir_all(path.parent().unwrap()).expect("jar directory");
            std::fs::write(&path, crate::mod_cache::test_jar_bytes("jar")).expect("write the jar");
        }

        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = asked.clone();
        let unavailable = editor_unavailable_mods(
            &launcher_paths,
            &[
                "sodium".to_string(),
                "iris".to_string(),
                "lithium".to_string(),
                "polytone".to_string(),
                "broken".to_string(),
            ],
            &target,
            "fabric",
            true,
            move |mod_id| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(mod_id.clone());
                    match mod_id.as_str() {
                        "polytone" => Ok(false),
                        _ => Err(anyhow::anyhow!("connection reset")),
                    }
                }
            },
        )
        .await;

        assert_eq!(
            unavailable,
            (vec!["iris".to_string(), "polytone".to_string()], false)
        );
        // Solo i mod senza riga vanno a Modrinth; la risposta si salva, l'errore no.
        let mut asked = asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, vec!["broken".to_string(), "polytone".to_string()]);
        assert_eq!(
            db_availability_get(
                launcher_paths.database_path(),
                &["polytone".to_string(), "broken".to_string()],
                "1.21.1",
                "fabric"
            ),
            vec![("polytone".to_string(), false)]
        );

        std::fs::remove_dir_all(launcher_paths.root_dir()).expect("remove temp dir");
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
