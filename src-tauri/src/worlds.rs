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
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

/// The `global_settings` key the hidden list lives under. One row, a JSON
/// array of triples: the table is already key/value and has no migrations,
/// and a list of hidden worlds does not deserve a schema of its own.
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
    /// Whether the triple is in the hidden list. The entry is still returned:
    /// the menu that un-hides a world needs to be able to name it.
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
/// `hidden` decides only the `hidden` flag on each entry; nothing is filtered
/// out here.
pub fn list_worlds(root_dir: &Path, hidden: &[WorldId]) -> Result<Vec<WorldEntry>> {
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
                let hidden = hidden.contains(&id);
                entries.push(WorldEntry {
                    level_name: level.level_name.unwrap_or_else(|| id.folder_name.clone()),
                    game_mode: WorldGameMode::from_game_type(level.game_type),
                    last_played_ms: level.last_played.unwrap_or(0),
                    icon_path: world_icon_path(&world_dir),
                    hidden,
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

/// The hidden triples. A missing row is an empty list, and so is a row this
/// build cannot parse: a corrupted value must not stop the home from opening,
/// and the next hide rewrites it.
pub fn load_hidden_worlds(connection: &Connection) -> Result<Vec<WorldId>> {
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
        .and_then(|value| serde_json::from_str::<Vec<WorldId>>(&value).ok())
        .unwrap_or_default())
}

/// Add or remove one triple, and return the list as it now stands.
///
/// Writes a single `global_settings` row and nothing else — no new file, no
/// format to migrate.
pub fn set_world_hidden(
    connection: &Connection,
    world: &WorldId,
    hidden: bool,
) -> Result<Vec<WorldId>> {
    let mut worlds = load_hidden_worlds(connection)?;
    let already_hidden = worlds.iter().any(|stored| stored == world);

    if hidden && !already_hidden {
        worlds.push(world.clone());
    } else if !hidden {
        worlds.retain(|stored| stored != world);
    }

    let value = serde_json::to_string(&worlds).context("failed to serialize the hidden worlds")?;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [HIDDEN_WORLDS_KEY, value.as_str()],
        )
        .context("failed to write the hidden worlds setting")?;

    Ok(worlds)
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

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_worlds_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<Vec<WorldEntry>, String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;
    let hidden = load_hidden_worlds(&connection).map_err(|error| error.to_string())?;

    list_worlds(launcher_paths.root_dir(), &hidden).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn set_world_hidden_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    folder_name: String,
    hidden: bool,
) -> Result<Vec<WorldId>, String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;
    let world = WorldId {
        modlist_name,
        instance_name,
        folder_name,
    };

    set_world_hidden(&connection, &world, hidden).map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "worlds_tests.rs"]
mod tests;
