//! The singleplayer worlds of every instance, and the Quick Play argument
//! that jumps straight into one of them.
//!
//! Four decisions worth stating once, because all four are load-bearing:
//!
//! - **A world is identified by the triple `(mod list, instance, folder)`,
//!   never by a name.** The two worlds on the real disk are both called
//!   `New World` and both live in a folder called `New World`
//!   (`Drehmal APOTHEOSIS/instances/1.20.1-forge/saves/New World` and
//!   `test2/instances/26.3-fabric/saves/New World`). Only the triple tells
//!   them apart, so it is what the listing reports and what the hidden list
//!   stores.
//! - **The displayed name is `LevelName` from `level.dat`, not the folder
//!   name.** Renaming a world in game rewrites `LevelName` and leaves the
//!   folder alone, so the two drift apart the first time anyone renames
//!   anything. The folder name stays in the entry because Quick Play needs
//!   exactly that.
//! - **An unreadable `level.dat` skips the world, silently.** A world written
//!   by a mod, or by a version whose format this parser does not understand,
//!   must not stop the rest of the list from being shown — and must not spam
//!   a log or a banner either.
//! - **Quick Play is offered only when the version's own manifest offers it.**
//!   See [`quick_play_arguments`]: the criterion is the launch data, not a
//!   comparison of version strings.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;

const INSTANCES_DIR_NAME: &str = "instances";
const SAVES_DIR_NAME: &str = "saves";
const LEVEL_DAT_FILE_NAME: &str = "level.dat";
const WORLD_ICON_FILE_NAME: &str = "icon.png";

/// Where the shared worlds live: `<root>/worlds/<folder>` (D99).
///
/// It was `mod-lists/<name>/worlds/` until D99, and moved out for the reason
/// in [`LauncherPaths::worlds_dir`]: a world shared across two mod lists
/// belongs to neither.
pub const WORLDS_DIR_NAME: &str = "worlds";

/// The `global_settings` key the hidden list lives under. One row holds a JSON
/// array of world triples and their `LastPlayed` baselines: the table is
/// already key/value and this list does not deserve a schema of its own.
pub const HIDDEN_WORLDS_KEY: &str = "hidden_worlds";

/// Ceiling on the decompressed `level.dat` a read is willing to hold in
/// memory. The largest of the two real worlds decompresses to 220 KB; a file
/// that claims far more than this is either not a `level.dat` or is hostile,
/// and either way the world is skipped rather than read.
const MAX_LEVEL_DAT_BYTES: u64 = 16 * 1024 * 1024;

/// Where a world lives, and the only thing that can name it.
///
/// A world is in exactly one of two places, and the enum says so instead of
/// leaving two `Option`s that could both be `None` or disagree. It has been
/// the shape of an identity twice already — `(modlist, instance, folder)` in
/// E10, `(modlist, Option<instance>, folder)` in D94 — and a third change
/// would cost more than making the invalid states unrepresentable now.
///
/// The tag is in the JSON (`scope`), so the frontend gets a discriminated
/// union rather than a pair of nullable fields it has to correlate by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum WorldHome {
    /// In one instance's `saves/`, and nowhere else. It is that instance's
    /// world and the mod list is part of its name.
    Instance {
        modlist_name: String,
        instance_name: String,
    },
    /// In `<root>/worlds/`, shared. **No mod list and no instance**: D99 took
    /// the mod list away for the same reason D94 took the instance away, and
    /// keying it on either would mean hiding it in one place and not in the
    /// other, which is what D95 refuses.
    Shared,
}

/// The identity of one world. Not the name — see the module docs.
///
/// Stored as is in the hidden list. The two older shapes — E10's
/// `(modlist, instance, folder)` and D94's `instanceName: null` — are **not**
/// read back: no release ever wrote a hidden row (the feature is newer than
/// `v0.1.2`), and the only machine that ran the unreleased builds has none
/// (checked, report `067`). A reader for rows that do not exist would be
/// machinery for nothing, in the one place — nested `flatten` over a tagged
/// enum — where serde is least forgiving.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldId {
    #[serde(flatten)]
    pub home: WorldHome,
    pub folder_name: String,
}

impl WorldId {
    /// The mod list a world belongs to, or `None` for a shared one.
    pub fn modlist_name(&self) -> Option<&str> {
        match &self.home {
            WorldHome::Instance { modlist_name, .. } => Some(modlist_name),
            WorldHome::Shared => None,
        }
    }

    pub fn instance_name(&self) -> Option<&str> {
        match &self.home {
            WorldHome::Instance { instance_name, .. } => Some(instance_name),
            WorldHome::Shared => None,
        }
    }
}

/// One way into a world: an instance of some mod list, and the name that
/// instance's own `saves/` uses for it.
///
/// **The mod list is part of it since D99**, because a world can now be shared
/// across mod lists and an instance name alone would not say which one it
/// belongs to — the exact objection the old cross-mod-list filter was written
/// on.
///
/// The folder name matters because it is what `--quickPlaySingleplayer` takes
/// ([`quick_play_arguments`]), and nothing forces a link to carry the name of
/// its target: since D98 the launcher itself gives it a different one when the
/// name is taken.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldInstanceLink {
    pub modlist_name: String,
    pub instance_name: String,
    pub folder_name: String,
}

/// A hidden world and the `LastPlayed` value observed when it was hidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenWorld {
    #[serde(flatten)]
    pub id: WorldId,
    /// Rows written before the baseline existed have no timestamp. They are
    /// hidden when read, and the first listing gives them the world's current
    /// `LastPlayed` as their baseline — see
    /// [`list_worlds_with_baseline_backfill`] — so the old data ends up with
    /// the new meaning: hidden until it is played again, never forever.
    #[serde(default)]
    pub hidden_at_last_played_ms: Option<i64>,
}

/// `GameType` in `level.dat`, as the frontend will read it.
///
/// An unknown value is reported as such rather than guessed: a game mode
/// added after this code was written must not turn into "survival".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorldGameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
    Unknown,
}

impl WorldGameMode {
    fn from_game_type(game_type: Option<i32>) -> Self {
        match game_type {
            Some(0) => Self::Survival,
            Some(1) => Self::Creative,
            Some(2) => Self::Adventure,
            Some(3) => Self::Spectator,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldEntry {
    #[serde(flatten)]
    pub id: WorldId,
    /// `LevelName`, the name the player sees in game.
    pub level_name: String,
    pub game_mode: WorldGameMode,
    /// `LastPlayed`, milliseconds since the epoch. `0` when the field is
    /// missing, which sorts the world to the bottom instead of dropping it.
    pub last_played_ms: i64,
    /// Absolute path of `icon.png` when the world has one. Minecraft only
    /// writes it when the player leaves the world through the menu, so its
    /// absence is ordinary — both real worlds are missing it.
    pub icon_path: Option<String>,
    /// Whether this world is hidden right now. Playing it after it was hidden
    /// makes it visible again without changing the stored hidden list.
    pub hidden: bool,
    /// Every instance that can open this world, sorted by name. One entry for
    /// an ordinary world — itself — and one per link for a shared one, which
    /// is both the sign D96 asks for and the list the Play button needs to
    /// offer a choice (D95).
    ///
    /// **Empty is a real state**: a shared world every instance has dropped
    /// stays in the mod list's `worlds/` folder and stays on this list, with
    /// nothing able to open it until it is shared again. Losing sight of it
    /// would be the only way this feature could lose bytes.
    pub instances: Vec<WorldInstanceLink>,
}

/// The three fields a world listing needs out of `level.dat`.
///
/// Every field is optional on purpose. `level.dat` is a format that changes
/// between versions — the 26.3 world on the real disk has no `hardcore` key
/// where the 1.20.1 one does — so a missing field degrades one column instead
/// of dropping the world.
#[derive(Debug, Deserialize)]
struct LevelDat {
    #[serde(rename = "Data")]
    data: LevelDatData,
}

#[derive(Debug, Default, Deserialize)]
struct LevelDatData {
    #[serde(rename = "LevelName")]
    level_name: Option<String>,
    #[serde(rename = "GameType")]
    game_type: Option<i32>,
    #[serde(rename = "LastPlayed")]
    last_played: Option<i64>,
}

// ── Listing ──────────────────────────────────────────────────────────────────

/// Every world the launcher can see, newest first: the ones that sit in an
/// instance's `saves/`, and the shared ones in `<root>/worlds/` (D99).
///
/// **The unit is the world, not the place.** A shared world is reachable from
/// several instances — since D99 of several mod lists — and must still be one
/// row (D95), so the scan collects candidates by their **canonical path** and
/// merges everything that resolves to the same bytes. That is also what tells
/// a link from its target: `064` measured that two links with different names
/// in one `saves/` resolve to the same world and that nothing in this file
/// used to notice.
///
/// `hidden` decides only the current `hidden` flag on each entry; nothing is
/// filtered out here. An entry with no baseline counts as hidden: it is the
/// state the backfill repairs, and until it does the world stays where the
/// user put it.
pub fn list_worlds(root_dir: &Path, hidden: &[HiddenWorld]) -> Result<Vec<WorldEntry>> {
    let paths = LauncherPaths::new(root_dir.to_path_buf());
    let roots = WorldRoots::resolve(&paths);

    let mut found: BTreeMap<PathBuf, FoundWorld> = BTreeMap::new();

    // The shared worlds. Real directories only: `worlds/` is a folder the
    // launcher fills, not one it follows links out of.
    for world_dir in child_directories(paths.worlds_dir())? {
        let (Some(folder_name), Ok(canonical)) =
            (utf8_file_name(&world_dir), fs::canonicalize(&world_dir))
        else {
            continue;
        };
        claim(
            &mut found,
            canonical,
            Owner::SharedWorlds,
            WorldId {
                home: WorldHome::Shared,
                folder_name,
            },
            world_dir,
        );
    }

    for modlist_dir in child_directories(paths.modlists_dir())? {
        let Some(modlist_name) = utf8_file_name(&modlist_dir) else {
            continue;
        };
        for instance_dir in child_directories(&modlist_dir.join(INSTANCES_DIR_NAME))? {
            let Some(instance_name) = utf8_file_name(&instance_dir) else {
                continue;
            };
            for entry in saves_entries(&instance_dir.join(SAVES_DIR_NAME), &roots)? {
                let Some(folder_name) = utf8_file_name(&entry.path) else {
                    continue;
                };
                let owner = if entry.is_link {
                    Owner::Link
                } else {
                    Owner::InstanceSaves
                };
                let slot = claim(
                    &mut found,
                    entry.canonical,
                    owner,
                    WorldId {
                        home: WorldHome::Instance {
                            modlist_name: modlist_name.clone(),
                            instance_name: instance_name.clone(),
                        },
                        folder_name: folder_name.clone(),
                    },
                    entry.path,
                );
                slot.ways_in.push(WayIn {
                    link: WorldInstanceLink {
                        modlist_name: modlist_name.clone(),
                        instance_name: instance_name.clone(),
                        folder_name,
                    },
                    is_link: entry.is_link,
                });
            }
        }
    }

    let mut entries: Vec<WorldEntry> = Vec::new();
    for world in found.into_values() {
        let Some(level) = read_level_dat(&world.directory.join(LEVEL_DAT_FILE_NAME)) else {
            continue;
        };

        // What stands where the cross-mod-list filter stood. That filter
        // dropped ways in from other mod lists because an instance name could
        // not say which mod list it belonged to; D99 put the mod list into the
        // way in, so the reason is gone. What it also stopped — measured, see
        // `a_hand_made_link_into_another_instances_world_is_not_a_way_in` — is
        // a hand-made link into another instance's own world becoming a way to
        // launch it. So: **a way in is the world's own directory, or a link
        // into `<root>/worlds/`**. An instance's world has exactly one — itself
        // — and a shared world has one per link.
        let shared = world.id.home == WorldHome::Shared;
        let mut instances: Vec<WorldInstanceLink> = world
            .ways_in
            .into_iter()
            .filter(|way| way.is_link == shared)
            .map(|way| way.link)
            .collect();
        instances.sort();
        instances.dedup();

        let last_played_ms = level.last_played.unwrap_or(0);
        let is_hidden = hidden.iter().any(|stored| {
            stored.id == world.id
                && match stored.hidden_at_last_played_ms {
                    Some(hidden_at) => last_played_ms <= hidden_at,
                    None => true,
                }
        });
        entries.push(WorldEntry {
            level_name: level
                .level_name
                .unwrap_or_else(|| world.id.folder_name.clone()),
            game_mode: WorldGameMode::from_game_type(level.game_type),
            last_played_ms,
            icon_path: world_icon_path(&world.directory),
            hidden: is_hidden,
            instances,
            id: world.id,
        });
    }

    // Most recently played first. The id is the tie-break so two worlds last
    // played in the same millisecond still come out in a stable order.
    entries.sort_by(|left, right| {
        right
            .last_played_ms
            .cmp(&left.last_played_ms)
            .then_with(|| left.id.modlist_name().cmp(&right.id.modlist_name()))
            .then_with(|| left.id.instance_name().cmp(&right.id.instance_name()))
            .then_with(|| left.id.folder_name.cmp(&right.id.folder_name))
    });

    Ok(entries)
}

/// One world while the scan is still merging the places it was seen from.
struct FoundWorld {
    owner: Owner,
    id: WorldId,
    /// The path `level.dat` and `icon.png` are read through. Any of the paths
    /// that resolve to this world would do — reading follows links — but the
    /// owner's is the one that is not going to move.
    directory: PathBuf,
    /// Every `saves/` entry that resolved here.
    ways_in: Vec<WayIn>,
}

struct WayIn {
    link: WorldInstanceLink,
    /// A symlink, as opposed to the world's own directory.
    is_link: bool,
}

/// Which of the places a world was seen from gets to name it. Higher wins,
/// and the scan order must not decide: a link in an instance that sorts early
/// can be met before the directory it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    /// A `saves/` entry that is a symlink. It names the world only while
    /// nothing better has been seen — a link is a way in, not a home.
    Link,
    /// A real directory in an instance's `saves/`: an ordinary world.
    InstanceSaves,
    /// `<root>/worlds/`: a shared world, which outranks every instance that
    /// links it.
    SharedWorlds,
}

/// Record one sighting of the world at `canonical`, letting the better owner
/// win, and hand back the slot so the caller can add its way in.
fn claim(
    found: &mut BTreeMap<PathBuf, FoundWorld>,
    canonical: PathBuf,
    owner: Owner,
    id: WorldId,
    directory: PathBuf,
) -> &mut FoundWorld {
    let slot = found.entry(canonical).or_insert_with(|| FoundWorld {
        owner,
        id: id.clone(),
        directory: directory.clone(),
        ways_in: Vec::new(),
    });
    if owner > slot.owner {
        slot.owner = owner;
        slot.id = id;
        slot.directory = directory;
    }
    slot
}

/// Where, of the two places a world may be, a canonical path lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorldPlace {
    /// `mod-lists/<modlist>/instances/<instance>/saves/<folder>`.
    InstanceSaves,
    /// `<root>/worlds/<folder>`.
    SharedWorlds,
}

/// The two places a world is allowed to be, canonicalised once.
///
/// **Two roots, not one root higher.** Until D99 every world was under
/// `mod-lists/`; now shared ones are under `<root>/worlds/`. Admitting
/// "anything under `<root>`" would have been the one-line way to let both
/// through, and it would have let through `cache/`, `skins/`, `java-runtimes/`
/// and every other folder the launcher keeps — the containment `065` closed,
/// reopened one floor up.
pub(crate) struct WorldRoots {
    modlists: Option<PathBuf>,
    worlds: Option<PathBuf>,
}

impl WorldRoots {
    pub(crate) fn resolve(paths: &LauncherPaths) -> Self {
        Self {
            modlists: fs::canonicalize(paths.modlists_dir()).ok(),
            worlds: fs::canonicalize(paths.worlds_dir()).ok(),
        }
    }

    /// Which root `canonical` is a world of, **by shape**: a prefix is not
    /// enough, the path has to be exactly where a world sits.
    pub(crate) fn place(&self, canonical: &Path) -> Option<WorldPlace> {
        let segments = |root: &Path| -> Option<Vec<std::ffi::OsString>> {
            let relative = canonical.strip_prefix(root).ok()?;
            Some(
                relative
                    .components()
                    .filter_map(|component| match component {
                        Component::Normal(segment) => Some(segment.to_os_string()),
                        _ => None,
                    })
                    .collect(),
            )
        };

        if let Some(segments) = self.worlds.as_deref().and_then(segments) {
            if segments.len() == 1 {
                return Some(WorldPlace::SharedWorlds);
            }
        }
        if let Some(segments) = self.modlists.as_deref().and_then(segments) {
            if segments.len() == 5
                && segments[1] == INSTANCES_DIR_NAME
                && segments[3] == SAVES_DIR_NAME
            {
                return Some(WorldPlace::InstanceSaves);
            }
        }
        None
    }

    /// The one gate for a `saves/` entry, and the same check
    /// [`resolve_world_directory`] applies: what the listing shows is exactly
    /// what the ⋮ menu can open.
    fn admits(&self, canonical: &Path) -> bool {
        self.place(canonical).is_some()
    }
}

/// One entry of an instance's `saves/`.
struct SavesEntry {
    path: PathBuf,
    canonical: PathBuf,
    is_link: bool,
}

/// The worlds an instance can open: the real directories in its `saves/`, and
/// the symlinks that point at a world the launcher is allowed to see.
///
/// A missing `saves/` yields nothing: an instance that never ran has none, and
/// that is an ordinary state.
fn saves_entries(dir: &Path, roots: &WorldRoots) -> Result<Vec<SavesEntry>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()))
        }
    };

    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        // A dangling link, or one pointing at a file: `canonicalize` follows,
        // so both fall out here rather than becoming a world that is not one.
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if !canonical.is_dir() {
            continue;
        }
        if !roots.admits(&canonical) {
            continue;
        }
        entries.push(SavesEntry {
            path,
            canonical,
            is_link: file_type.is_symlink(),
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// The listing the command serves: the worlds, plus the one repair the
/// hidden list can need.
///
/// Hiding means "not now", never "never again" (D65), and the rule that
/// brings a world back is its `LastPlayed` moving past the value it had when
/// it was hidden. A row written before that baseline existed has no such
/// value, and reading it as "hidden forever" would turn hiding into the
/// one-way door it was never meant to be — there is no "show hidden" switch
/// anywhere in the app to escape through.
///
/// So the first listing adopts the world's current `LastPlayed` as the
/// missing baseline and writes the row back: the world stays hidden today,
/// exactly where the user left it, and comes back the next time it is
/// played. An entry whose world is no longer on disk keeps its empty
/// baseline, because there is nothing to adopt.
pub fn list_worlds_with_baseline_backfill(
    root_dir: &Path,
    connection: &Connection,
) -> Result<Vec<WorldEntry>> {
    let mut hidden = load_hidden_worlds(connection)?;
    let entries = list_worlds(root_dir, &hidden)?;

    let mut backfilled = false;
    for stored in hidden.iter_mut() {
        if stored.hidden_at_last_played_ms.is_some() {
            continue;
        }
        let Some(world) = entries.iter().find(|entry| entry.id == stored.id) else {
            continue;
        };
        stored.hidden_at_last_played_ms = Some(world.last_played_ms);
        backfilled = true;
    }
    if backfilled {
        write_hidden_worlds(connection, &hidden)?;
    }

    Ok(entries)
}

/// The three fields, or `None` for anything this parser cannot read: a
/// missing file, a file that is not gzip, NBT it does not understand, or a
/// root compound without `Data`. Every one of those is "skip this world".
fn read_level_dat(path: &Path) -> Option<LevelDatData> {
    let file = fs::File::open(path).ok()?;
    let mut decoded = Vec::new();
    GzDecoder::new(file)
        .take(MAX_LEVEL_DAT_BYTES)
        .read_to_end(&mut decoded)
        .ok()?;

    fastnbt::from_bytes::<LevelDat>(&decoded)
        .ok()
        .map(|level| level.data)
}

fn world_icon_path(world_dir: &Path) -> Option<String> {
    let icon = world_dir.join(WORLD_ICON_FILE_NAME);
    if icon.is_file() {
        icon.to_str().map(ToString::to_string)
    } else {
        None
    }
}

/// Directory children of `dir`, sorted. A missing `dir` yields nothing: an
/// instance that never ran has no `saves/`, and that is an ordinary state.
///
/// Symlinked children are not followed. Used for the mod list, instance and
/// `worlds/` levels, none of which is a place a link is ever expected: only
/// an instance's `saves/` holds links, and [`saves_entries`] reads that one
/// with the containment check a followed link needs.
fn child_directories(dir: &Path) -> Result<Vec<PathBuf>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()))
        }
    };

    let mut directories = Vec::new();
    for entry in read_dir {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

fn utf8_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

// ── The hidden list ──────────────────────────────────────────────────────────

/// The hidden worlds and their `LastPlayed` baselines. A missing row is an
/// empty list, and so is a row this build cannot parse: a corrupted value must
/// not stop the home from opening, and the next hide rewrites it.
pub fn load_hidden_worlds(connection: &Connection) -> Result<Vec<HiddenWorld>> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = ?1",
            [HIDDEN_WORLDS_KEY],
            |row| row.get(0),
        )
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .context("failed to read the hidden worlds setting")?;

    Ok(stored
        .and_then(|value| serde_json::from_str::<Vec<HiddenWorld>>(&value).ok())
        .unwrap_or_default())
}

/// Add, update, or remove one hidden world, and return the list as it now
/// stands. Re-hiding updates the baseline so the next play can reveal it
/// again.
///
/// Writes a single `global_settings` row and nothing else — no new file and
/// no database migration.
pub fn set_world_hidden(
    connection: &Connection,
    world: &WorldId,
    hidden: bool,
    hidden_at_last_played_ms: Option<i64>,
) -> Result<Vec<HiddenWorld>> {
    let mut worlds = load_hidden_worlds(connection)?;

    if hidden {
        if let Some(stored) = worlds.iter_mut().find(|stored| &stored.id == world) {
            stored.hidden_at_last_played_ms = hidden_at_last_played_ms;
        } else {
            worlds.push(HiddenWorld {
                id: world.clone(),
                hidden_at_last_played_ms,
            });
        }
    } else {
        worlds.retain(|stored| &stored.id != world);
    }

    write_hidden_worlds(connection, &worlds)?;

    Ok(worlds)
}

/// The one row the hidden list lives in, rewritten whole. It is a single
/// JSON value: there is nothing to merge, and an upsert keeps the other
/// `global_settings` keys — the launch settings — untouched.
fn write_hidden_worlds(connection: &Connection, worlds: &[HiddenWorld]) -> Result<()> {
    let value = serde_json::to_string(worlds).context("failed to serialize the hidden worlds")?;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [HIDDEN_WORLDS_KEY, value.as_str()],
        )
        .context("failed to write the hidden worlds setting")?;
    Ok(())
}

// ── Quick Play ───────────────────────────────────────────────────────────────

/// The game arguments that make the client open a world instead of the menu,
/// or nothing at all.
///
/// **The support criterion is the version's own manifest**, carried here as
/// `version_supports_quick_play`
/// (`minecraft_downloader::MinecraftVersionData::supports_quick_play_singleplayer`):
/// a client whose `arguments.game` declares the
/// `is_quick_play_singleplayer` entry is a client whose parser knows the
/// option. That is the same JSON the launch is about to run, so the answer
/// cannot drift from the binary — unlike comparing `1.20.1` against `26.3`
/// against `23w14a`, three numbering schemes with no common order.
///
/// Everything else here is "when in doubt, launch normally": an unnamed
/// world, a name that is not a single path component, a folder that is not on
/// disk, or a version that does not declare the option all return no
/// arguments, and the player lands on the menu.
pub fn quick_play_arguments(
    instance_root: &Path,
    folder_name: Option<&str>,
    version_supports_quick_play: bool,
) -> Vec<String> {
    if !version_supports_quick_play {
        return Vec::new();
    }
    let Some(folder_name) = folder_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Vec::new();
    };
    if validate_path_component(folder_name).is_err() {
        return Vec::new();
    }
    if !instance_root
        .join(SAVES_DIR_NAME)
        .join(folder_name)
        .is_dir()
    {
        return Vec::new();
    }

    vec![
        "--quickPlaySingleplayer".to_string(),
        folder_name.to_string(),
    ]
}

// ── The folder a world lives in ──────────────────────────────────────────────

/// Rebuild one world's directory from the id the listing handed out, or
/// refuse.
///
/// Two shapes, because a world has two homes:
/// `mod-lists/<modlist>/instances/<instance>/saves/<folder>` for an ordinary
/// world and `<root>/worlds/<folder>` for a shared one (D99).
///
/// Same pact as `screenshots::resolve_screenshot`: the caller never passes a
/// path, each name MUST be a single path component, and the join is
/// canonicalised and checked by [`WorldRoots::place`] — the same function the
/// listing uses, so the two cannot disagree about what is a world. A shared
/// world is reached through its own folder and not through any instance's
/// link, so the answer does not depend on which instance the caller happened
/// to be looking at.
pub(crate) fn resolve_world_directory(root_dir: &Path, id: &WorldId) -> Result<PathBuf> {
    validate_path_component(&id.folder_name)
        .with_context(|| format!("invalid world folder name '{}'", id.folder_name))?;
    let paths = LauncherPaths::new(root_dir.to_path_buf());
    let world_dir = match &id.home {
        WorldHome::Instance {
            modlist_name,
            instance_name,
        } => {
            validate_path_component(modlist_name)
                .with_context(|| format!("invalid mod list name '{modlist_name}'"))?;
            validate_path_component(instance_name)
                .with_context(|| format!("invalid instance name '{instance_name}'"))?;
            paths
                .modlists_dir()
                .join(modlist_name)
                .join(INSTANCES_DIR_NAME)
                .join(instance_name)
                .join(SAVES_DIR_NAME)
                .join(&id.folder_name)
        }
        WorldHome::Shared => paths.worlds_dir().join(&id.folder_name),
    };

    let resolved = fs::canonicalize(&world_dir)
        .with_context(|| format!("no world folder at {}", world_dir.display()))?;
    if WorldRoots::resolve(&paths).place(&resolved).is_none() {
        bail!(
            "the world folder at {} is outside the launcher's worlds",
            world_dir.display()
        );
    }
    if !resolved.is_dir() {
        bail!("{} is not a directory", resolved.display());
    }

    Ok(resolved)
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_worlds_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<Vec<WorldEntry>, String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;

    list_worlds_with_baseline_backfill(launcher_paths.root_dir(), &connection)
        .map_err(|error| error.to_string())
}

/// Hide or unhide one world.
///
/// The id is the one the listing handed out, whole: for a shared world it
/// carries neither a mod list nor an instance, and that is the whole point of
/// D95 — the hidden list keys on the world, so hiding it hides the one card
/// the home draws instead of hiding it in one place and leaving it in another.
#[tauri::command]
pub fn set_world_hidden_command(
    launcher_paths: State<'_, LauncherPaths>,
    world: WorldId,
    hidden: bool,
) -> Result<Vec<WorldId>, String> {
    let hidden_at_last_played_ms = if hidden {
        let world_dir = resolve_world_directory(launcher_paths.root_dir(), &world)
            .map_err(|error| format!("{error:#}"))?;
        read_level_dat(&world_dir.join(LEVEL_DAT_FILE_NAME)).and_then(|level| level.last_played)
    } else {
        None
    };
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;

    set_world_hidden(
        &connection,
        &world,
        hidden,
        hidden_at_last_played_ms,
    )
    .map(|worlds| worlds.into_iter().map(|stored| stored.id).collect())
    .map_err(|error| error.to_string())
}

/// Open the world's folder in the system file manager.
///
/// The ⋮ menu of a world card offers it, and the card only ever holds the
/// id: the path is rebuilt here, refused rather than guessed, exactly
/// like `screenshots::open_screenshot_folder_command` does for a screenshot.
#[tauri::command]
pub fn open_world_folder_command(
    launcher_paths: State<'_, LauncherPaths>,
    world: WorldId,
) -> Result<(), String> {
    let folder = resolve_world_directory(launcher_paths.root_dir(), &world)
        .map_err(|error| format!("{error:#}"))?;

    open::that(&folder).map_err(|error| format!("failed to open {}: {error}", folder.display()))
}

#[cfg(test)]
#[path = "worlds_tests.rs"]
mod tests;
