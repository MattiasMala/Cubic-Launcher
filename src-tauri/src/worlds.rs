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

/// The `global_settings` key the hidden list lives under. One row holds a JSON
/// array of world triples and their `LastPlayed` baselines: the table is
/// already key/value and this list does not deserve a schema of its own.
pub const HIDDEN_WORLDS_KEY: &str = "hidden_worlds";

/// Ceiling on the decompressed `level.dat` a read is willing to hold in
/// memory. The largest of the two real worlds decompresses to 220 KB; a file
/// that claims far more than this is either not a `level.dat` or is hostile,
/// and either way the world is skipped rather than read.
const MAX_LEVEL_DAT_BYTES: u64 = 16 * 1024 * 1024;

/// The identity of one world. Not the name — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldId {
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

/// Every world under `mod-lists/<modlist>/instances/<instance>/saves/`,
/// newest first.
///
/// `hidden` decides only the current `hidden` flag on each entry; nothing is
/// filtered out here. An entry with no baseline counts as hidden: it is the
/// state the backfill repairs, and until it does the world stays where the
/// user put it.
pub fn list_worlds(root_dir: &Path, hidden: &[HiddenWorld]) -> Result<Vec<WorldEntry>> {
    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();

    let mut entries: Vec<WorldEntry> = Vec::new();
    for modlist_dir in child_directories(&modlists_dir)? {
        let Some(modlist_name) = utf8_file_name(&modlist_dir) else {
            continue;
        };
        for instance_dir in child_directories(&modlist_dir.join(INSTANCES_DIR_NAME))? {
            let Some(instance_name) = utf8_file_name(&instance_dir) else {
                continue;
            };
            for world_dir in child_directories(&instance_dir.join(SAVES_DIR_NAME))? {
                let Some(folder_name) = utf8_file_name(&world_dir) else {
                    continue;
                };
                let Some(level) = read_level_dat(&world_dir.join(LEVEL_DAT_FILE_NAME)) else {
                    continue;
                };

                let id = WorldId {
                    modlist_name: modlist_name.clone(),
                    instance_name: instance_name.clone(),
                    folder_name,
                };
                let last_played_ms = level.last_played.unwrap_or(0);
                let is_hidden = hidden.iter().any(|stored| {
                    &stored.id == &id
                        && match stored.hidden_at_last_played_ms {
                            Some(hidden_at) => last_played_ms <= hidden_at,
                            None => true,
                        }
                });
                entries.push(WorldEntry {
                    level_name: level.level_name.unwrap_or_else(|| id.folder_name.clone()),
                    game_mode: WorldGameMode::from_game_type(level.game_type),
                    last_played_ms,
                    icon_path: world_icon_path(&world_dir),
                    hidden: is_hidden,
                    id,
                });
            }
        }
    }

    // Most recently played first. The triple is the tie-break so two worlds
    // last played in the same millisecond still come out in a stable order.
    entries.sort_by(|left, right| {
        right
            .last_played_ms
            .cmp(&left.last_played_ms)
            .then_with(|| left.id.modlist_name.cmp(&right.id.modlist_name))
            .then_with(|| left.id.instance_name.cmp(&right.id.instance_name))
            .then_with(|| left.id.folder_name.cmp(&right.id.folder_name))
    });

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
/// Symlinked children are not followed, so a link cannot make the listing
/// report a world that lives outside the launcher root.
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

/// Rebuild one world's directory from the triple the listing handed out, or
/// refuse.
///
/// Same pact as `screenshots::resolve_screenshot`: the caller never passes a
/// path, each name MUST be a single path component, and the join is
/// canonicalised and checked against `<root>/mod-lists` so a `saves`
/// directory that is a symlink elsewhere resolves to its real location and
/// fails the test.
fn resolve_world_directory(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    folder_name: &str,
) -> Result<PathBuf> {
    validate_path_component(modlist_name)
        .with_context(|| format!("invalid mod list name '{modlist_name}'"))?;
    validate_path_component(instance_name)
        .with_context(|| format!("invalid instance name '{instance_name}'"))?;
    validate_path_component(folder_name)
        .with_context(|| format!("invalid world folder name '{folder_name}'"))?;

    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();
    let world_dir = modlists_dir
        .join(modlist_name)
        .join(INSTANCES_DIR_NAME)
        .join(instance_name)
        .join(SAVES_DIR_NAME)
        .join(folder_name);

    let resolved = fs::canonicalize(&world_dir)
        .with_context(|| format!("no world folder at {}", world_dir.display()))?;
    let resolved_modlists_dir = fs::canonicalize(&modlists_dir)
        .with_context(|| format!("failed to resolve {}", modlists_dir.display()))?;

    let relative = resolved.strip_prefix(&resolved_modlists_dir).map_err(|_| {
        anyhow::anyhow!("the world folder of '{modlist_name}/{instance_name}' is outside the mod lists directory")
    })?;
    let segments: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment),
            _ => None,
        })
        .collect();
    let shaped_like_a_world = segments.len() == 5
        && segments[1] == INSTANCES_DIR_NAME
        && segments[3] == SAVES_DIR_NAME;
    if !shaped_like_a_world {
        bail!("the world folder of '{modlist_name}/{instance_name}' is outside the mod lists directory");
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

#[tauri::command]
pub fn set_world_hidden_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    folder_name: String,
    hidden: bool,
) -> Result<Vec<WorldId>, String> {
    validate_path_component(&modlist_name).map_err(|error| error.to_string())?;
    validate_path_component(&instance_name).map_err(|error| error.to_string())?;
    validate_path_component(&folder_name).map_err(|error| error.to_string())?;
    let hidden_at_last_played_ms = if hidden {
        let level_dat_path = launcher_paths
            .modlists_dir()
            .join(&modlist_name)
            .join(INSTANCES_DIR_NAME)
            .join(&instance_name)
            .join(SAVES_DIR_NAME)
            .join(&folder_name)
            .join(LEVEL_DAT_FILE_NAME);
        read_level_dat(&level_dat_path).and_then(|level| level.last_played)
    } else {
        None
    };
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;
    let world = WorldId {
        modlist_name,
        instance_name,
        folder_name,
    };

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
/// triple: the path is rebuilt here, refused rather than guessed, exactly
/// like `screenshots::open_screenshot_folder_command` does for a screenshot.
#[tauri::command]
pub fn open_world_folder_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    folder_name: String,
) -> Result<(), String> {
    let folder = resolve_world_directory(
        launcher_paths.root_dir(),
        &modlist_name,
        &instance_name,
        &folder_name,
    )
    .map_err(|error| format!("{error:#}"))?;

    open::that(&folder).map_err(|error| format!("failed to open {}: {error}", folder.display()))
}

#[cfg(test)]
#[path = "worlds_tests.rs"]
mod tests;
