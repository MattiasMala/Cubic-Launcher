//! Pin one mod to one Modrinth release.
//!
//! Pinning adds no field to `Rule` and does not touch the `rules.json` schema
//! (decision D14): the chosen jar is downloaded and registered as a **local**
//! mod, and a local mod is never re-resolved against Modrinth, so the pin is
//! stable by construction instead of by a rule that protects it.
//!
//! A local mod is also always available for every target, which would leave
//! the dynamic fallback of D5 unreachable. The pinned entry therefore carries
//! a `version_rules` of kind `only` on the chosen Minecraft version and
//! loader: outside that target the pin is excluded and the dynamic entry —
//! kept as its alternative — takes over.
//!
//! The writes live in one command because they are not independent: a local
//! jar registered without its version rule is a mod pinned *everywhere*. They
//! run in a fixed order behind a snapshot of `rules.json`, and any failure
//! restores it byte for byte — the discipline of
//! `local_content_packs::remove_partial_import`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::editor_data::{
    add_alternative_from_root, delete_rules_from_root, remove_alternative_from_root,
    reorder_rules_from_root, save_rule_advanced_from_root, AddAlternativeInput, DeleteRulesInput,
    RemoveAlternativeInput, ReorderRulesInput, SaveRuleAdvancedInput, SaveVersionRuleInput,
};
use crate::launcher_paths::LauncherPaths;
use crate::minecraft_downloader::download_file_verified;
use crate::mod_cache::SqliteModCacheRepository;
use crate::modlist_manager::{
    copy_local_jar_from_root, local_mod_id_from_jar_filename, CopyLocalJarInput,
};
use crate::modrinth::{build_http_client, is_version_compatible, ModrinthClient, ModrinthVersion};
use crate::path_safety::validate_path_component;
use crate::resolver::{parse_mod_loader, ResolutionTarget};
use crate::rules::{ModList, ModSource, Rule, VersionRuleKind, RULES_FILENAME};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinModVersionInput {
    pub modlist_name: String,
    /// The dynamic entry being pinned: a top-level Modrinth rule.
    pub mod_id: String,
    /// The Modrinth version id to freeze on.
    pub version_id: String,
    pub minecraft_version: String,
    pub mod_loader: String,
    /// D5: `false` (the default) keeps the dynamic entry as the alternative of
    /// the pinned one; `true` deletes it.
    #[serde(default)]
    pub remove_dynamic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PinnedVersion {
    pub pinned_mod_id: String,
    pub jar_file_name: String,
    pub dynamic_removed: bool,
    /// D48: the pin this one replaced, if the mod was already pinned.
    pub replaced_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePinInput {
    pub modlist_name: String,
    /// The pinned (local) entry to drop.
    pub pinned_mod_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedPin {
    /// The dynamic entry put back at the top level, when the pin had one.
    pub restored_mod_id: Option<String>,
}

#[tauri::command]
pub fn remove_pin_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: RemovePinInput,
) -> Result<RemovedPin, String> {
    remove_pin_from_root(launcher_paths.root_dir(), &input).map_err(|error| error.to_string())
}

/// Undo a pin: the local entry and its jar go, the dynamic entry parked under
/// it comes back to the top level, in the pin's place.
///
/// Same discipline as the pin: everything decided first, one snapshot, and a
/// rollback that restores `rules.json` byte for byte and brings the jar back.
pub fn remove_pin_from_root(root_dir: &Path, input: &RemovePinInput) -> Result<RemovedPin> {
    validate_path_component(&input.modlist_name)?;
    let rules_path = rules_path(root_dir, &input.modlist_name);
    let modlist = ModList::read_from_file(&rules_path)?;

    let pin = modlist
        .rules
        .iter()
        .find(|rule| rule.mod_id == input.pinned_mod_id)
        .with_context(|| {
            format!(
                "'{}' is not a top-level entry of modlist '{}'",
                input.pinned_mod_id, input.modlist_name
            )
        })?;

    // The `only` rule is what tells a pin from a jar the user uploaded
    // himself, and removing a pin deletes the jar.
    if !is_pinned_entry(pin) {
        bail!(
            "'{}' is not a pinned version: a local entry without an 'only' version rule is a jar you added by hand, and its jar is the only copy",
            input.pinned_mod_id
        );
    }

    if pin.alternatives.len() > 1 {
        bail!(
            "the pin '{}' carries {} alternatives: remove them by hand first, so nothing is deleted by surprise",
            input.pinned_mod_id,
            pin.alternatives.len()
        );
    }
    let restored_mod_id = pin.alternatives.first().map(|alt| alt.mod_id.clone());

    let mut snapshot = PinSnapshot::capture(
        &rules_path,
        &local_jars_dir(root_dir, &input.modlist_name).join(format!("{}.jar", pin.mod_id)),
    )?;
    snapshot.stash_jar(
        &local_jars_dir(root_dir, &input.modlist_name).join(format!("{}.jar", pin.mod_id)),
    )?;

    let applied = (|| -> Result<()> {
        // `remove_alternative_from_root` re-inserts the extracted rule right
        // after its old parent (`editor_data.rs:421-426`), so deleting the pin
        // afterwards leaves the dynamic entry exactly where the pin was — no
        // reordering step needed.
        if let Some(restored) = &restored_mod_id {
            remove_alternative_from_root(
                root_dir,
                &RemoveAlternativeInput {
                    modlist_name: input.modlist_name.clone(),
                    parent_mod_id: input.pinned_mod_id.clone(),
                    alt_mod_id: restored.clone(),
                },
            )?;
        }

        delete_rules_from_root(
            root_dir,
            &DeleteRulesInput {
                modlist_name: input.modlist_name.clone(),
                mod_ids: vec![input.pinned_mod_id.clone()],
            },
        )
    })();

    match applied {
        Ok(()) => {
            snapshot.commit();
            Ok(RemovedPin { restored_mod_id })
        }
        Err(error) => {
            snapshot.restore();
            Err(error.context(format!(
                "removing the pin '{}' failed; the mod list was rolled back",
                input.pinned_mod_id
            )))
        }
    }
}

#[tauri::command]
pub async fn pin_mod_version_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: PinModVersionInput,
) -> Result<PinnedVersion, String> {
    pin_mod_version_from_root(launcher_paths.root_dir(), &input)
        .await
        .map_err(|error| error.to_string())
}

/// Download the chosen release and register it as the pinned entry.
///
/// Nothing on disk is touched before the download succeeds, so a Modrinth or
/// network failure cannot leave a half-pinned mod list: the rollback below
/// only has to cover the writes.
pub async fn pin_mod_version_from_root(
    root_dir: &Path,
    input: &PinModVersionInput,
) -> Result<PinnedVersion> {
    validate_path_component(&input.modlist_name)?;

    let minecraft_version = input.minecraft_version.trim().to_string();
    if minecraft_version.is_empty() {
        bail!("minecraft_version cannot be empty");
    }
    let target = ResolutionTarget {
        minecraft_version,
        mod_loader: parse_mod_loader(&input.mod_loader)?,
    };

    let version_id = input.version_id.trim().to_string();
    if version_id.is_empty() {
        bail!("version_id cannot be empty");
    }

    let client = ModrinthClient::new();
    let mut versions = client
        .fetch_versions_by_ids(std::slice::from_ref(&version_id))
        .await?;
    let version = versions
        .remove(&version_id)
        .with_context(|| format!("Modrinth does not know a version with id '{version_id}'"))?;

    // The pin is scoped to this target by its `only` rule, so a release that
    // does not support the target would produce an entry that never resolves.
    if !is_version_compatible(&version, &target) {
        bail!(
            "version '{}' does not support {} / {}",
            version.version_number,
            target.minecraft_version,
            target.mod_loader.as_modrinth_loader()
        );
    }

    verify_version_belongs_to_mod(root_dir, &client, &version, &input.mod_id, &target).await?;

    let file = version
        .primary_file()
        .with_context(|| format!("version '{version_id}' has no downloadable file"))?;
    let sha1 = file
        .hashes
        .get("sha1")
        .map(|hash| hash.trim().to_string())
        .filter(|hash| !hash.is_empty())
        .with_context(|| {
            format!(
                "version '{version_id}' publishes no sha1 for '{}', so the download cannot be verified",
                file.filename
            )
        })?;
    validate_path_component(&file.filename)?;

    let temp_dir = unique_temp_dir();
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;
    let temp_jar = temp_dir.join(&file.filename);

    let pinned = match download_file_verified(&build_http_client(), &file.url, &temp_jar, &sha1)
        .await
        .with_context(|| format!("failed to download '{}'", file.filename))
    {
        Ok(()) => apply_pin_from_root(
            root_dir,
            &ApplyPinInput {
                modlist_name: input.modlist_name.clone(),
                dynamic_mod_id: input.mod_id.clone(),
                source_jar_path: temp_jar,
                minecraft_version: target.minecraft_version.clone(),
                loader: target.mod_loader.as_modrinth_loader().to_string(),
                remove_dynamic: input.remove_dynamic,
            },
        ),
        Err(error) => Err(error),
    };

    // The jar now lives inside the mod list; the staging copy is the price of
    // `copy_local_jar_from_root` taking a filesystem path.
    std::fs::remove_dir_all(&temp_dir).ok();

    pinned
}

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();

    std::env::temp_dir().join(format!("cubic-pin-{}-{nanos}", std::process::id()))
}

/// Refuse a version id that belongs to another project.
///
/// Without this a wrong id downloads someone else's jar and registers it as a
/// brand-new local entry — a mod list corrupted by one typo. The rule names a
/// slug and the metadata carry the canonical base62 id, so the comparison goes
/// through the three bridges, cheapest first:
///
/// 1. the rule already names the canonical id;
/// 2. `modrinth_project_aliases` knows the slug (`mod_cache.rs:300`) — it is
///    only populated by the online branch, so an absent row proves nothing;
/// 3. the project's own version list for this target contains the id. One extra
///    request, and the only bridge that works on a cold database.
async fn verify_version_belongs_to_mod(
    root_dir: &Path,
    client: &ModrinthClient,
    version: &ModrinthVersion,
    mod_id: &str,
    target: &ResolutionTarget,
) -> Result<()> {
    let mod_id = mod_id.trim();
    if version.project_id.trim() == mod_id {
        return Ok(());
    }

    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let database_path = launcher_paths.database_path();
    if database_path.exists() {
        let connection = Connection::open(&database_path).with_context(|| {
            format!("failed to open the database at {}", database_path.display())
        })?;
        let repository = SqliteModCacheRepository::new(
            &connection,
            LauncherPaths::new(root_dir.to_path_buf()).mods_cache_dir(),
        );
        if let Some(canonical) = repository.find_canonical_project_id(mod_id)? {
            if canonical.trim() == version.project_id.trim() {
                return Ok(());
            }
        }
    }

    let belongs = client
        .fetch_project_versions(mod_id, target)
        .await
        .with_context(|| format!("failed to check that version '{}' belongs to '{mod_id}'", version.id))?
        .iter()
        .any(|candidate| candidate.id == version.id);

    if belongs {
        return Ok(());
    }

    bail!(
        "version '{}' belongs to Modrinth project '{}', not to '{mod_id}'",
        version.id,
        version.project_id
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPinInput {
    pub modlist_name: String,
    pub dynamic_mod_id: String,
    /// A jar already on disk: the downloaded release, or a local file in tests.
    pub source_jar_path: PathBuf,
    pub minecraft_version: String,
    pub loader: String,
    pub remove_dynamic: bool,
}

/// Every decision the pin needs, taken before a byte is written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinPlan {
    pinned_mod_id: String,
    jar_file_name: String,
    dynamic_mod_id: String,
    remove_dynamic: bool,
    version_rules: Vec<SaveVersionRuleInput>,
    /// Top-level order after the pin: the pinned entry takes the place the
    /// dynamic one had instead of landing at the bottom of the list.
    top_level_order: Vec<String>,
    /// D48: the pin this one replaces, when the dynamic entry is already
    /// parked under an older pin.
    replaced_pin: Option<String>,
}

/// A pinned entry, told apart from a hand-uploaded local jar by the `only`
/// rule the pin always writes.
///
/// It matters because "remove pin" deletes the jar: doing that to a jar the
/// user uploaded himself would destroy the only copy.
fn is_pinned_entry(rule: &Rule) -> bool {
    rule.source == ModSource::Local
        && rule
            .version_rules
            .iter()
            .any(|version_rule| version_rule.kind == VersionRuleKind::Only)
}

pub fn apply_pin_from_root(root_dir: &Path, input: &ApplyPinInput) -> Result<PinnedVersion> {
    validate_path_component(&input.modlist_name)?;
    let rules_path = rules_path(root_dir, &input.modlist_name);
    let modlist = ModList::read_from_file(&rules_path)?;
    let plan = plan_pin(&modlist, input)?;

    apply_pin_plan(root_dir, &input.modlist_name, &input.source_jar_path, &plan)
}

fn plan_pin(modlist: &ModList, input: &ApplyPinInput) -> Result<PinPlan> {
    if input.minecraft_version.trim().is_empty() {
        // An `only` rule with no Minecraft version matches *every* version
        // (`resolver::version_rules_conflict`), which is the opposite of a pin.
        bail!("a pin needs a Minecraft version");
    }
    if input.loader.trim().is_empty() {
        bail!("a pin needs a loader");
    }

    let jar_file_name = input
        .source_jar_path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| {
            format!(
                "source path '{}' has no valid filename",
                input.source_jar_path.display()
            )
        })?
        .to_string();
    let pinned_mod_id = local_mod_id_from_jar_filename(&jar_file_name)?;

    let (position, replaced_pin) = match modlist
        .rules
        .iter()
        .position(|rule| rule.mod_id == input.dynamic_mod_id)
    {
        Some(position) => {
            if modlist.rules[position].source != ModSource::Modrinth {
                bail!(
                    "'{}' is a local mod: it has no Modrinth release to pin",
                    input.dynamic_mod_id
                );
            }
            (position, None)
        }
        // D48: a dynamic entry that sits **directly** under a pin is the
        // "change the pin" case — the old pin goes, the new one takes its
        // place, the dynamic entry stays where it is.
        None => {
            let parent = modlist
                .rules
                .iter()
                .position(|rule| {
                    is_pinned_entry(rule)
                        && rule
                            .alternatives
                            .iter()
                            .any(|alt| alt.mod_id == input.dynamic_mod_id)
                })
                .with_context(|| {
                    if modlist.contains_mod_id(&input.dynamic_mod_id) {
                        format!(
                            "'{}' is an alternative of another mod; only a top-level entry, or the dynamic entry of a pin, can be pinned",
                            input.dynamic_mod_id
                        )
                    } else {
                        format!(
                            "rule '{}' not found in modlist '{}'",
                            input.dynamic_mod_id, input.modlist_name
                        )
                    }
                })?;

            // Deleting the old pin takes its whole subtree with it
            // (`delete_rules_from_root`), and the new pin only pulls the
            // dynamic entry out. Anything else parked under the old pin would
            // be lost, so that case is refused instead of guessed.
            let siblings = modlist.rules[parent].alternatives.len();
            if siblings != 1 {
                bail!(
                    "the pin '{}' carries {siblings} alternatives: replace it by hand, or leave only the dynamic entry '{}' under it",
                    modlist.rules[parent].mod_id,
                    input.dynamic_mod_id
                );
            }

            (parent, Some(modlist.rules[parent].mod_id.clone()))
        }
    };

    if modlist.contains_mod_id(&pinned_mod_id) {
        bail!(
            "'{}' is already in modlist '{}'",
            pinned_mod_id,
            input.modlist_name
        );
    }

    // `delete_rules_from_root` drops the rule with `retain` (`editor_data.rs:248`),
    // and a rule carries its alternatives: removing a dynamic entry that is the
    // head of a fallback chain would delete mods nobody named. D5 says the
    // dynamic entry disappears, not the chain under it, so this is refused
    // instead of decided here.
    let dynamic_rule = match &replaced_pin {
        Some(_) => &modlist.rules[position].alternatives[0],
        None => &modlist.rules[position],
    };
    if input.remove_dynamic && !dynamic_rule.alternatives.is_empty() {
        bail!(
            "'{}' has {} alternative(s): removing it would delete them too — keep the dynamic entry, or remove its alternatives first",
            input.dynamic_mod_id,
            dynamic_rule.alternatives.len()
        );
    }

    let mut top_level_order: Vec<String> = modlist
        .rules
        .iter()
        .map(|rule| rule.mod_id.clone())
        .collect();
    // The dynamic entry leaves the top level either way — deleted, or moved
    // under the pinned entry — so the pinned id replaces it in place.
    top_level_order[position] = pinned_mod_id.clone();

    Ok(PinPlan {
        pinned_mod_id,
        jar_file_name,
        dynamic_mod_id: input.dynamic_mod_id.clone(),
        remove_dynamic: input.remove_dynamic,
        version_rules: vec![SaveVersionRuleInput {
            kind: "only".into(),
            mc_versions: vec![input.minecraft_version.trim().to_string()],
            loader: input.loader.trim().to_string(),
        }],
        top_level_order,
        replaced_pin,
    })
}

/// The writes, in the only order that works, behind a rollback.
///
/// The pinned entry is created **first**: adding an alternative *moves* the
/// existing rule instead of duplicating it (`add_alternative_from_root` in
/// `editor_data.rs`), so the parent has to exist before the dynamic entry can
/// be moved under it. The other order would make the pin an alternative of
/// itself.
///
/// Each step is an existing editor command, which is why `rules.json` is read
/// and written more than once: the cost of reusing the rule-tree logic instead
/// of reimplementing it.
fn apply_pin_plan(
    root_dir: &Path,
    modlist_name: &str,
    source_jar_path: &Path,
    plan: &PinPlan,
) -> Result<PinnedVersion> {
    let rules_path = rules_path(root_dir, modlist_name);
    let jar_destination =
        local_jars_dir(root_dir, modlist_name).join(format!("{}.jar", plan.pinned_mod_id));
    let mut snapshot = PinSnapshot::capture(&rules_path, &jar_destination)?;

    // The jar of the pin being replaced moves aside before anything else: a
    // rename is undoable, a delete is not.
    if let Some(replaced) = &plan.replaced_pin {
        snapshot.stash_jar(&local_jars_dir(root_dir, modlist_name).join(format!("{replaced}.jar")))?;
    }

    let applied = (|| -> Result<()> {
        copy_local_jar_from_root(
            root_dir,
            &CopyLocalJarInput {
                source_path: source_jar_path.to_string_lossy().into_owned(),
                modlist_name: modlist_name.to_string(),
            },
        )?;

        save_rule_advanced_from_root(
            root_dir,
            &SaveRuleAdvancedInput {
                modlist_name: modlist_name.to_string(),
                mod_id: plan.pinned_mod_id.clone(),
                exclude_if: vec![],
                requires: vec![],
                version_rules: plan.version_rules.clone(),
                custom_configs: vec![],
            },
        )?;

        if plan.remove_dynamic {
            delete_rules_from_root(
                root_dir,
                &DeleteRulesInput {
                    modlist_name: modlist_name.to_string(),
                    mod_ids: vec![plan.dynamic_mod_id.clone()],
                },
            )?;
        } else {
            add_alternative_from_root(
                root_dir,
                &AddAlternativeInput {
                    modlist_name: modlist_name.to_string(),
                    parent_mod_id: plan.pinned_mod_id.clone(),
                    mod_id: plan.dynamic_mod_id.clone(),
                    source: "modrinth".into(),
                },
            )?;
        }

        // Only now is the old pin childless: the dynamic entry has already
        // been moved out (or deleted), so deleting it cannot take anything
        // else with it.
        if let Some(replaced) = &plan.replaced_pin {
            delete_rules_from_root(
                root_dir,
                &DeleteRulesInput {
                    modlist_name: modlist_name.to_string(),
                    mod_ids: vec![replaced.clone()],
                },
            )?;
        }

        reorder_rules_from_root(
            root_dir,
            &ReorderRulesInput {
                modlist_name: modlist_name.to_string(),
                ordered_mod_ids: plan.top_level_order.clone(),
            },
        )
    })();

    match applied {
        Ok(()) => {
            snapshot.commit();
            Ok(PinnedVersion {
                pinned_mod_id: plan.pinned_mod_id.clone(),
                jar_file_name: plan.jar_file_name.clone(),
                dynamic_removed: plan.remove_dynamic,
                replaced_pin: plan.replaced_pin.clone(),
            })
        }
        Err(error) => {
            snapshot.restore();
            Err(error.context(format!(
                "pinning '{}' failed; the mod list was rolled back",
                plan.dynamic_mod_id
            )))
        }
    }
}

fn rules_path(root_dir: &Path, modlist_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join(RULES_FILENAME)
}

fn local_jars_dir(root_dir: &Path, modlist_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join("local-jars")
}

/// `rules.json` as it was, whether the pinned jar was already there, and the
/// jar of a replaced pin moved aside instead of deleted.
struct PinSnapshot {
    rules_path: PathBuf,
    rules_contents: Vec<u8>,
    jar_destination: PathBuf,
    jar_existed: bool,
    stashed_jar: Option<(PathBuf, PathBuf)>,
}

impl PinSnapshot {
    fn capture(rules_path: &Path, jar_destination: &Path) -> Result<Self> {
        Ok(Self {
            rules_path: rules_path.to_path_buf(),
            rules_contents: std::fs::read(rules_path)
                .with_context(|| format!("failed to read {}", rules_path.display()))?,
            jar_destination: jar_destination.to_path_buf(),
            jar_existed: jar_destination.exists(),
            stashed_jar: None,
        })
    }

    /// Move a jar out of the way, keeping it restorable. A rename inside the
    /// same directory cannot half-succeed and cannot cross a filesystem.
    fn stash_jar(&mut self, jar: &Path) -> Result<()> {
        if !jar.exists() {
            return Ok(());
        }

        let stash = jar.with_extension("jar.replaced");
        std::fs::rename(jar, &stash).with_context(|| {
            format!(
                "failed to move {} aside before replacing the pin",
                jar.display()
            )
        })?;
        self.stashed_jar = Some((jar.to_path_buf(), stash));

        Ok(())
    }

    /// Nothing left to undo: drop the stashed jar for good.
    fn commit(&mut self) {
        if let Some((_, stash)) = self.stashed_jar.take() {
            std::fs::remove_file(stash).ok();
        }
    }

    /// Put `rules.json` back byte for byte, remove the jar the pin copied, and
    /// bring a stashed jar back.
    ///
    /// A jar that was already there keeps its bytes: same filename means the
    /// same Modrinth file, and the copy was sha1-verified before it got here.
    ///
    /// Failures are ignored on purpose — an error is already on its way out,
    /// and replacing it would hide why the pin failed (same reasoning as
    /// `local_content_packs::remove_partial_import`).
    fn restore(&self) {
        std::fs::write(&self.rules_path, &self.rules_contents).ok();
        if !self.jar_existed {
            std::fs::remove_file(&self.jar_destination).ok();
        }
        if let Some((original, stash)) = &self.stashed_jar {
            std::fs::rename(stash, original).ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::resolver::{resolve_modlist, ModLoader, ResolutionTarget, RuleOutcome};
    use crate::rules::{ModList, ModSource, Rule, VersionRuleKind};

    use super::{
        apply_pin_from_root, apply_pin_plan, remove_pin_from_root, ApplyPinInput, PinPlan,
        RemovePinInput,
    };
    use crate::editor_data::SaveVersionRuleInput;

    const PINNED_JAR: &str = "modernfix-forge-5.27.72+mc1.20.1.jar";
    const PINNED_ID: &str = "modernfix-forge-5.27.72+mc1.20.1";

    fn unique_test_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        std::env::temp_dir().join(format!("cubic-pin-test-{nanos}"))
    }

    fn modrinth_rule(mod_id: &str) -> Rule {
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

    /// A mod list with the pin target in the middle, so a lost position shows.
    fn seed_modlist(root: &PathBuf, rules: Vec<Rule>) -> PathBuf {
        let modlist_dir = root.join("mod-lists").join("Test Pack");
        fs::create_dir_all(modlist_dir.join("local-jars")).unwrap();

        ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules,
        }
        .write_to_file(&modlist_dir.join("rules.json"))
        .unwrap();

        modlist_dir
    }

    fn source_jar(root: &PathBuf) -> PathBuf {
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let jar = staging.join(PINNED_JAR);
        fs::write(&jar, b"pinned jar bytes").unwrap();
        jar
    }

    fn pin_input(root: &PathBuf, remove_dynamic: bool) -> ApplyPinInput {
        ApplyPinInput {
            modlist_name: "Test Pack".into(),
            dynamic_mod_id: "modernfix".into(),
            source_jar_path: source_jar(root),
            minecraft_version: "1.20.1".into(),
            loader: "forge".into(),
            remove_dynamic,
        }
    }

    fn target(minecraft_version: &str, mod_loader: ModLoader) -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: minecraft_version.into(),
            mod_loader,
        }
    }

    fn resolved_id(modlist: &ModList, target: &ResolutionTarget, mod_id: &str) -> Option<String> {
        resolve_modlist(modlist, target)
            .unwrap()
            .resolved_rules
            .into_iter()
            .find(|resolved| resolved.mod_id == mod_id)
            .and_then(|resolved| match resolved.outcome {
                RuleOutcome::Resolved { resolved_id } => Some(resolved_id),
                RuleOutcome::Unresolved { .. } => None,
            })
    }

    #[test]
    fn the_pin_becomes_the_parent_and_the_dynamic_entry_its_alternative() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(
            &root,
            vec![
                modrinth_rule("cull-leaves"),
                modrinth_rule("modernfix"),
                modrinth_rule("embeddium"),
            ],
        );

        let outcome = apply_pin_from_root(&root, &pin_input(&root, false)).unwrap();
        assert_eq!(outcome.pinned_mod_id, PINNED_ID);
        assert!(!outcome.dynamic_removed);

        let modlist = ModList::read_from_file(&modlist_dir.join("rules.json")).unwrap();
        let ids: Vec<&str> = modlist.rules.iter().map(|r| r.mod_id.as_str()).collect();
        assert_eq!(ids, vec!["cull-leaves", PINNED_ID, "embeddium"]);

        let pinned = &modlist.rules[1];
        assert_eq!(pinned.source, ModSource::Local);
        assert_eq!(pinned.version_rules.len(), 1);
        assert_eq!(pinned.version_rules[0].kind, VersionRuleKind::Only);
        assert_eq!(pinned.version_rules[0].mc_versions, vec!["1.20.1"]);
        assert_eq!(pinned.version_rules[0].loader, "forge");

        let alternatives: Vec<&str> = pinned
            .alternatives
            .iter()
            .map(|alt| alt.mod_id.as_str())
            .collect();
        assert_eq!(alternatives, vec!["modernfix"]);
        assert_eq!(pinned.alternatives[0].source, ModSource::Modrinth);

        assert!(modlist_dir
            .join("local-jars")
            .join(PINNED_JAR)
            .exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_the_dynamic_entry_leaves_only_the_pin() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(
            &root,
            vec![modrinth_rule("modernfix"), modrinth_rule("embeddium")],
        );

        let outcome = apply_pin_from_root(&root, &pin_input(&root, true)).unwrap();
        assert!(outcome.dynamic_removed);

        let modlist = ModList::read_from_file(&modlist_dir.join("rules.json")).unwrap();
        let ids: Vec<&str> = modlist.rules.iter().map(|r| r.mod_id.as_str()).collect();
        assert_eq!(ids, vec![PINNED_ID, "embeddium"]);
        assert!(!modlist.contains_mod_id("modernfix"));
        assert!(modlist.rules[0].alternatives.is_empty());
        assert_eq!(modlist.rules[0].version_rules.len(), 1);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_target_the_pin_does_not_cover_resolves_the_dynamic_entry() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(&root, vec![modrinth_rule("modernfix")]);

        apply_pin_from_root(&root, &pin_input(&root, false)).unwrap();
        let modlist = ModList::read_from_file(&modlist_dir.join("rules.json")).unwrap();

        assert_eq!(
            resolved_id(&modlist, &target("1.20.1", ModLoader::Forge), PINNED_ID),
            Some(PINNED_ID.to_string()),
            "on the pinned target the pin itself must win"
        );
        assert_eq!(
            resolved_id(&modlist, &target("1.20.1", ModLoader::Fabric), PINNED_ID),
            Some("modernfix".to_string()),
            "another loader must fall back to the dynamic entry"
        );
        assert_eq!(
            resolved_id(&modlist, &target("1.21.1", ModLoader::Forge), PINNED_ID),
            Some("modernfix".to_string()),
            "another Minecraft version must fall back to the dynamic entry"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_dynamic_entry_that_carries_alternatives_is_refused() {
        let root = unique_test_root();
        let mut dynamic = modrinth_rule("modernfix");
        dynamic.alternatives = vec![modrinth_rule("embeddium")];
        let modlist_dir = seed_modlist(&root, vec![dynamic]);
        let before = fs::read(modlist_dir.join("rules.json")).unwrap();

        let error = apply_pin_from_root(&root, &pin_input(&root, true))
            .expect_err("deleting the dynamic entry would delete its fallback chain");
        assert!(
            error.to_string().contains("would delete them too"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(modlist_dir.join("rules.json")).unwrap(), before);

        // Keeping the dynamic entry preserves the chain under it.
        apply_pin_from_root(&root, &pin_input(&root, false)).unwrap();
        let modlist = ModList::read_from_file(&modlist_dir.join("rules.json")).unwrap();
        assert_eq!(modlist.rules[0].mod_id, PINNED_ID);
        assert_eq!(modlist.rules[0].alternatives[0].mod_id, "modernfix");
        assert_eq!(
            modlist.rules[0].alternatives[0].alternatives[0].mod_id,
            "embeddium"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_step_leaves_the_mod_list_exactly_as_it_was() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(
            &root,
            vec![modrinth_rule("modernfix"), modrinth_rule("embeddium")],
        );
        let rules_path = modlist_dir.join("rules.json");
        let before = fs::read(&rules_path).unwrap();

        // The jar, the version rule and the move all land; the last step —
        // putting the pinned entry back where the dynamic one was — fails
        // because the order names a rule this mod list does not have. Only a
        // stale plan produces this (another window editing `rules.json` in
        // between), so the plan is built by hand: it is the rollback under
        // test, not the validation that normally prevents it.
        let plan = PinPlan {
            pinned_mod_id: PINNED_ID.into(),
            jar_file_name: PINNED_JAR.into(),
            dynamic_mod_id: "modernfix".into(),
            remove_dynamic: false,
            version_rules: vec![SaveVersionRuleInput {
                kind: "only".into(),
                mc_versions: vec!["1.20.1".into()],
                loader: "forge".into(),
            }],
            top_level_order: vec!["deleted-meanwhile".into(), PINNED_ID.into()],
            replaced_pin: None,
        };

        let error = apply_pin_plan(&root, "Test Pack", &source_jar(&root), &plan)
            .expect_err("the last step must fail");
        assert!(
            error.to_string().contains("rolled back"),
            "the error should say the mod list was restored, got: {error}"
        );

        assert_eq!(
            fs::read(&rules_path).unwrap(),
            before,
            "rules.json must be byte-identical to the state before the pin"
        );
        assert!(
            !modlist_dir.join("local-jars").join(PINNED_JAR).exists(),
            "the copied jar must be gone"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_nested_entry_cannot_be_pinned() {
        let root = unique_test_root();
        let mut parent = modrinth_rule("embeddium");
        parent.alternatives = vec![modrinth_rule("modernfix")];
        let modlist_dir = seed_modlist(&root, vec![parent]);
        let before = fs::read(modlist_dir.join("rules.json")).unwrap();

        let error = apply_pin_from_root(&root, &pin_input(&root, false))
            .expect_err("an entry that is already a fallback cannot be pinned");
        assert!(
            error.to_string().contains("alternative of another mod"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(modlist_dir.join("rules.json")).unwrap(), before);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_local_entry_cannot_be_pinned() {
        let root = unique_test_root();
        let mut local = modrinth_rule("modernfix");
        local.source = ModSource::Local;
        seed_modlist(&root, vec![local]);

        let error = apply_pin_from_root(&root, &pin_input(&root, false))
            .expect_err("a local entry has no Modrinth release to pin");
        assert!(
            error.to_string().contains("local mod"),
            "unexpected error: {error}"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    /// A second jar, so re-pinning has a different version to land on.
    const OTHER_JAR: &str = "modernfix-forge-5.27.66+mc1.20.1.jar";
    const OTHER_ID: &str = "modernfix-forge-5.27.66+mc1.20.1";

    fn other_source_jar(root: &PathBuf) -> PathBuf {
        let staging = root.join("staging-2");
        fs::create_dir_all(&staging).unwrap();
        let jar = staging.join(OTHER_JAR);
        fs::write(&jar, b"other pinned jar bytes").unwrap();
        jar
    }

    #[test]
    fn re_pinning_replaces_the_previous_pin_instead_of_adding_a_third_entry() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(
            &root,
            vec![
                modrinth_rule("cull-leaves"),
                modrinth_rule("modernfix"),
                modrinth_rule("embeddium"),
            ],
        );

        apply_pin_from_root(&root, &pin_input(&root, false)).unwrap();
        let outcome = apply_pin_from_root(
            &root,
            &ApplyPinInput {
                source_jar_path: other_source_jar(&root),
                ..pin_input(&root, false)
            },
        )
        .unwrap();

        assert_eq!(outcome.pinned_mod_id, OTHER_ID);
        assert_eq!(outcome.replaced_pin.as_deref(), Some(PINNED_ID));

        let modlist = ModList::read_from_file(&modlist_dir.join("rules.json")).unwrap();
        let ids: Vec<&str> = modlist.rules.iter().map(|r| r.mod_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["cull-leaves", OTHER_ID, "embeddium"],
            "the new pin takes the old pin's place, and there is no third entry"
        );
        assert!(!modlist.contains_mod_id(PINNED_ID));
        assert_eq!(modlist.rules[1].alternatives[0].mod_id, "modernfix");
        assert_eq!(modlist.rules[1].version_rules.len(), 1);

        let local_jars = modlist_dir.join("local-jars");
        assert!(local_jars.join(OTHER_JAR).exists());
        assert!(
            !local_jars.join(PINNED_JAR).exists(),
            "the replaced jar must be gone, not left behind"
        );
        assert!(
            !local_jars.join(format!("{PINNED_JAR}.replaced")).exists()
                && !local_jars
                    .join(PINNED_JAR.replace(".jar", ".jar.replaced"))
                    .exists(),
            "no stash file survives a successful replacement"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_pin_puts_the_dynamic_entry_back_in_its_place() {
        let root = unique_test_root();
        let modlist_dir = seed_modlist(
            &root,
            vec![
                modrinth_rule("cull-leaves"),
                modrinth_rule("modernfix"),
                modrinth_rule("embeddium"),
            ],
        );
        let before = fs::read(modlist_dir.join("rules.json")).unwrap();

        apply_pin_from_root(&root, &pin_input(&root, false)).unwrap();
        let removed = remove_pin_from_root(
            &root,
            &RemovePinInput {
                modlist_name: "Test Pack".into(),
                pinned_mod_id: PINNED_ID.into(),
            },
        )
        .unwrap();

        assert_eq!(removed.restored_mod_id.as_deref(), Some("modernfix"));
        assert_eq!(
            fs::read(modlist_dir.join("rules.json")).unwrap(),
            before,
            "pin then unpin must land back on the original rules.json"
        );
        assert!(!modlist_dir.join("local-jars").join(PINNED_JAR).exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hand_uploaded_local_jar_is_not_treated_as_a_pin() {
        let root = unique_test_root();
        let mut manual = modrinth_rule("mythicmetals-0.19.11+1.20.1-forge");
        manual.source = ModSource::Local;
        let modlist_dir = seed_modlist(&root, vec![manual]);
        let jar = modlist_dir
            .join("local-jars")
            .join("mythicmetals-0.19.11+1.20.1-forge.jar");
        fs::write(&jar, b"the user's only copy").unwrap();

        let error = remove_pin_from_root(
            &root,
            &RemovePinInput {
                modlist_name: "Test Pack".into(),
                pinned_mod_id: "mythicmetals-0.19.11+1.20.1-forge".into(),
            },
        )
        .expect_err("a local entry without an 'only' rule is not a pin");
        assert!(
            error.to_string().contains("not a pinned version"),
            "unexpected error: {error}"
        );
        assert!(jar.exists(), "the user's jar must still be there");

        fs::remove_dir_all(&root).unwrap();
    }
}
