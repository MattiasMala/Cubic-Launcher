//! The player's skin (E7): the saved library, the defaults, and how the skin
//! Mojang says is worn is reconciled with both.
//!
//! **Where things live.** A saved skin is a PNG in `<root>/skins/`, named by
//! its sha256 — the same address Mojang gives a texture, since the key in a
//! texture URL is the sha256 of the file it serves (measured in 056 and again
//! here). Which skins each player saved, in which order, is one JSON value in
//! `global_settings` under `saved_skins`, like the hidden worlds (D65) and the
//! shared file groups (D75). Every entry carries the player's UUID: the
//! `accounts` table holds more than one account, and two accounts must not
//! see one library.
//!
//! **Mojang re-encodes what it receives** (measured, phase A): a 64×64 PNG
//! uploaded came back under a key that was *not* its sha256, with the same
//! pixels in a different file. A file Mojang itself produced comes back under
//! its own key (the player's 64×32 legacy skin, re-uploaded, returned
//! `9df9…655c` unchanged). So after an upload the saved entry is moved to
//! Mojang's key and file, and from then on its key and Mojang's agree.

use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::microsoft_auth::{AccountsRepository, MinecraftProfile, SkinVariant, TextureState};
use crate::offline_account::resolve_cached_profile_username;

#[path = "skins_api.rs"]
mod api;
pub use api::{
    classify_mojang_error, texture_url, SkinError, SkinErrorKind, WriteGuard,
    ASSUMED_WRITES_PER_MINUTE, DEFAULT_RATE_LIMIT_COOLDOWN_SECONDS,
};
pub(crate) use api::{LiveBackend, SkinBackend};

#[cfg(test)]
#[path = "skins_tests.rs"]
mod tests;

/// The `global_settings` row that lists the saved skins.
pub const SAVED_SKINS_KEY: &str = "saved_skins";

/// A ceiling on a skin file, ours: a 64×64 RGBA image is 16 KB uncompressed,
/// and Mojang's own limit is not documented.
pub const MAX_SKIN_BYTES: u64 = 1024 * 1024;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

// ── The data ───────────────────────────────────────────────────────────────

/// One saved skin of one player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSkin {
    /// The Minecraft profile id, lowercase and without dashes.
    pub player_uuid: String,
    pub texture_key: String,
    pub variant: SkinVariant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A vanilla default skin, embedded in the binary so that the screen works
/// without a network.
#[derive(Debug)]
pub struct DefaultSkin {
    pub name: &'static str,
    pub variant: SkinVariant,
    pub texture_key: &'static str,
    pub png: &'static [u8],
}

macro_rules! default_skin {
    ($name:literal, $file:literal, $variant:ident, $key:literal) => {
        DefaultSkin {
            name: $name,
            variant: SkinVariant::$variant,
            texture_key: $key,
            png: include_bytes!(concat!("../assets/default-skins/", $file, ".png")),
        }
    };
}

/// The nine vanilla skins, each with classic and slim arms, as Mojang serves
/// them from `https://textures.minecraft.net/texture/<texture_key>`.
///
/// Provenance (report 061): the eighteen **texture keys** are taken from
/// Modrinth's `packages/app-lib/src/api/minecraft_skins/assets/default/default_skins.rs`
/// (GPL-3.0, like this launcher), which lists them. The **files** were
/// downloaded from Mojang's texture server under those keys, not copied from
/// Modrinth; each hashes to its key (the test below checks it) and matches
/// pixel for pixel the same-named texture in the vanilla 26.3 client jar,
/// which is where the name and the arms come from.
pub static DEFAULT_SKINS: [DefaultSkin; 18] = [
    default_skin!(
        "Alex",
        "alex-classic",
        Classic,
        "1abc803022d8300ab7578b189294cce39622d9a404cdc00d3feacfdf45be6981"
    ),
    default_skin!(
        "Alex",
        "alex-slim",
        Slim,
        "46acd06e8483b176e8ea39fc12fe105eb3a2a4970f5100057e9d84d4b60bdfa7"
    ),
    default_skin!(
        "Ari",
        "ari-classic",
        Classic,
        "4c05ab9e07b3505dc3ec11370c3bdce5570ad2fb2b562e9b9dd9cf271f81aa44"
    ),
    default_skin!(
        "Ari",
        "ari-slim",
        Slim,
        "6ac6ca262d67bcfb3dbc924ba8215a18195497c780058a5749de674217721892"
    ),
    default_skin!(
        "Efe",
        "efe-classic",
        Classic,
        "daf3d88ccb38f11f74814e92053d92f7728ddb1a7955652a60e30cb27ae6659f"
    ),
    default_skin!(
        "Efe",
        "efe-slim",
        Slim,
        "fece7017b1bb13926d1158864b283b8b930271f80a90482f174cca6a17e88236"
    ),
    default_skin!(
        "Kai",
        "kai-classic",
        Classic,
        "e5cdc3243b2153ab28a159861be643a4fc1e3c17d291cdd3e57a7f370ad676f3"
    ),
    default_skin!(
        "Kai",
        "kai-slim",
        Slim,
        "226c617fde5b1ba569aa08bd2cb6fd84c93337532a872b3eb7bf66bdd5b395f8"
    ),
    default_skin!(
        "Makena",
        "makena-classic",
        Classic,
        "dc0fcfaf2aa040a83dc0de4e56058d1bbb2ea40157501f3e7d15dc245e493095"
    ),
    default_skin!(
        "Makena",
        "makena-slim",
        Slim,
        "7cb3ba52ddd5cc82c0b050c3f920f87da36add80165846f479079663805433db"
    ),
    default_skin!(
        "Noor",
        "noor-classic",
        Classic,
        "90e75cd429ba6331cd210b9bd19399527ee3bab467b5a9f61cb8a27b177f6789"
    ),
    default_skin!(
        "Noor",
        "noor-slim",
        Slim,
        "6c160fbd16adbc4bff2409e70180d911002aebcfa811eb6ec3d1040761aea6dd"
    ),
    default_skin!(
        "Steve",
        "steve-classic",
        Classic,
        "31f477eb1a7beee631c2ca64d06f8f68fa93a3386d04452ab27f43acdf1b60cb"
    ),
    default_skin!(
        "Steve",
        "steve-slim",
        Slim,
        "d5c4ee5ce20aed9e33e866c66caa37178606234b3721084bf01d13320fb2eb3f"
    ),
    default_skin!(
        "Sunny",
        "sunny-classic",
        Classic,
        "a3bd16079f764cd541e072e888fe43885e711f98658323db0f9a6045da91ee7a"
    ),
    default_skin!(
        "Sunny",
        "sunny-slim",
        Slim,
        "b66bc80f002b10371e2fa23de6f230dd5e2f3affc2e15786f65bc9be4c6eb71a"
    ),
    default_skin!(
        "Zuri",
        "zuri-classic",
        Classic,
        "f5dddb41dcafef616e959c2817808e0be741c89ffbfed39134a13e75b811863d"
    ),
    default_skin!(
        "Zuri",
        "zuri-slim",
        Slim,
        "eee522611005acf256dbd152e992c60c0bb7978cb0f3127807700e478ad97664"
    ),
];

fn default_skin(texture_key: &str, variant: SkinVariant) -> Option<&'static DefaultSkin> {
    DEFAULT_SKINS
        .iter()
        .find(|skin| skin.texture_key == texture_key && skin.variant == variant)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkinSource {
    /// In this player's library.
    Saved,
    /// One of the eighteen vanilla skins.
    Default,
    /// Worn on the profile and found nowhere else: shown as a card of its own
    /// until it is saved or replaced.
    External,
}

/// A card of the skin screen, before its texture is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconciledSkin {
    pub source: SkinSource,
    pub texture_key: String,
    pub variant: SkinVariant,
    pub name: Option<String>,
    /// Worn on the profile right now.
    pub active: bool,
}

/// A card as the frontend receives it: everything the 3D preview needs is
/// the texture and the arms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinCard {
    #[serde(flatten)]
    pub skin: ReconciledSkin,
    /// A `data:image/png;base64,…` for saved and default skins, drawn from the
    /// local file with or without a network; Mojang's `https://` URL for an
    /// external one.
    pub texture_url: String,
}

/// A cape the player owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapeCard {
    pub id: String,
    pub alias: Option<String>,
    pub texture_url: String,
    pub active: bool,
}

/// Everything the skin screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinsView {
    pub player_uuid: String,
    pub player_name: Option<String>,
    /// External first, then saved (newest first), then the defaults.
    pub skins: Vec<SkinCard>,
    /// Every owned cape. Empty when the profile could not be read.
    pub capes: Vec<CapeCard>,
    /// Why the profile could not be read. The library is shown anyway, with
    /// no card marked active.
    pub profile_error: Option<SkinError>,
}

/// Where one player's skins are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkinContext {
    pub db_path: PathBuf,
    pub skins_dir: PathBuf,
    pub player_uuid: String,
    pub player_name: Option<String>,
}

/// The skin screen's shared state: the write guard, and the profile Mojang
/// last answered with.
#[derive(Debug, Default)]
pub struct SkinsState {
    guard: Mutex<WriteGuard>,
    last_profile: Mutex<Option<MinecraftProfile>>,
}

// ── The PNG ────────────────────────────────────────────────────────────────

/// Checks what can be checked without decoding: the PNG signature and the
/// IHDR size. 64×64 and the legacy 64×32 pass — Mojang accepted a 64×32
/// upload as it was (phase A), so nothing here normalizes it. Mojang decodes
/// the file anyway and answers `INVALID_IMAGE_DATA` to what it can't read.
pub fn validate_skin_png(bytes: &[u8]) -> Result<(u32, u32), SkinError> {
    if bytes.len() as u64 > MAX_SKIN_BYTES {
        return Err(SkinError::invalid("The file is larger than any skin"));
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(SkinError::invalid("A skin is a PNG file"));
    }
    // The first chunk of a PNG is IHDR: length 13, type, width, height.
    let Some(header) = bytes.get(8..24) else {
        return Err(SkinError::invalid("The PNG ends before its header"));
    };
    if header[0..4] != 13_u32.to_be_bytes() || &header[4..8] != b"IHDR" {
        return Err(SkinError::invalid("The PNG does not start with its header"));
    }
    let width = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let height = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
    match (width, height) {
        (64, 64) | (64, 32) => Ok((width, height)),
        _ => Err(SkinError::invalid(format!(
            "A skin is 64×64 or 64×32 pixels; this one is {width}×{height}"
        ))),
    }
}

/// A texture key as Mojang writes it: 64 lowercase hex digits. Anything else
/// coming from the frontend is refused before it becomes a path.
pub fn is_texture_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn texture_path(skins_dir: &Path, texture_key: &str) -> Result<PathBuf, SkinError> {
    if !is_texture_key(texture_key) {
        return Err(SkinError::invalid(format!(
            "'{texture_key}' is not a texture key"
        )));
    }
    Ok(skins_dir.join(format!("{texture_key}.png")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Writes a checked skin under its sha256 and returns the key. The file is
/// written once: the same key is the same bytes.
fn store_texture(skins_dir: &Path, bytes: &[u8]) -> Result<String, SkinError> {
    validate_skin_png(bytes)?;
    let texture_key = sha256_hex(bytes);
    let path = texture_path(skins_dir, &texture_key)?;
    if !path.is_file() {
        fs::create_dir_all(skins_dir).map_err(SkinError::library)?;
        let partial = skins_dir.join(format!("{texture_key}.png.part"));
        fs::write(&partial, bytes).map_err(SkinError::library)?;
        fs::rename(&partial, &path).map_err(SkinError::library)?;
    }
    Ok(texture_key)
}

fn delete_texture_if_unused(
    all: &[SavedSkin],
    skins_dir: &Path,
    texture_key: &str,
) -> Result<(), SkinError> {
    if all.iter().any(|skin| skin.texture_key == texture_key) {
        return Ok(());
    }
    match fs::remove_file(texture_path(skins_dir, texture_key)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SkinError::library(error)),
    }
}

fn data_url(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!("data:image/png;base64,{}", STANDARD.encode(bytes))
}

/// A Minecraft profile id as the library stores it: lowercase, no dashes.
pub fn normalize_uuid(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

// ── The library ────────────────────────────────────────────────────────────

/// Every saved skin of every player. A missing row is an empty library; an
/// entry that does not read is dropped and the others kept, because one bad
/// entry must not cost the player the whole list.
pub fn load_saved_skins(connection: &Connection) -> Result<Vec<SavedSkin>, SkinError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = ?1",
            [SAVED_SKINS_KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(SkinError::library)?;
    let entries = match stored.map(|value| serde_json::from_str::<serde_json::Value>(&value)) {
        Some(Ok(serde_json::Value::Array(entries))) => entries,
        _ => Vec::new(),
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<SavedSkin>(entry).ok())
        .filter(|skin| is_texture_key(&skin.texture_key))
        .collect())
}

/// The one row the library lives in, rewritten whole; the other
/// `global_settings` keys are left alone.
fn write_saved_skins(connection: &Connection, skins: &[SavedSkin]) -> Result<(), SkinError> {
    let value = serde_json::to_string(skins).map_err(SkinError::library)?;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SAVED_SKINS_KEY, value.as_str()],
        )
        .map_err(SkinError::library)?;
    Ok(())
}

pub fn saved_skins_for(
    connection: &Connection,
    player_uuid: &str,
) -> Result<Vec<SavedSkin>, SkinError> {
    let player_uuid = normalize_uuid(player_uuid);
    Ok(load_saved_skins(connection)?
        .into_iter()
        .filter(|skin| skin.player_uuid == player_uuid)
        .collect())
}

fn is_entry(skin: &SavedSkin, player_uuid: &str, texture_key: &str, variant: SkinVariant) -> bool {
    skin.player_uuid == player_uuid && skin.texture_key == texture_key && skin.variant == variant
}

/// Saves a skin for one player, newest first. The same texture with the same
/// arms is not saved twice; a name given again replaces the old one.
pub fn add_to_library(
    connection: &Connection,
    skins_dir: &Path,
    player_uuid: &str,
    png: &[u8],
    variant: SkinVariant,
    name: Option<String>,
) -> Result<SavedSkin, SkinError> {
    if variant == SkinVariant::Unknown {
        return Err(SkinError::invalid(
            "A skin is saved with classic or slim arms",
        ));
    }
    let texture_key = store_texture(skins_dir, png)?;
    let entry = SavedSkin {
        player_uuid: normalize_uuid(player_uuid),
        texture_key,
        variant,
        name,
    };

    let mut all = load_saved_skins(connection)?;
    let saved = match all
        .iter_mut()
        .find(|skin| is_entry(skin, &entry.player_uuid, &entry.texture_key, entry.variant))
    {
        Some(existing) => {
            if entry.name.is_some() {
                existing.name = entry.name;
            }
            existing.clone()
        }
        None => {
            all.insert(0, entry.clone());
            entry
        }
    };
    write_saved_skins(connection, &all)?;
    Ok(saved)
}

/// Removes one entry, and its file when no entry of any player refers to it.
pub fn remove_from_library(
    connection: &Connection,
    skins_dir: &Path,
    player_uuid: &str,
    texture_key: &str,
    variant: SkinVariant,
) -> Result<(), SkinError> {
    texture_path(skins_dir, texture_key)?;
    let player_uuid = normalize_uuid(player_uuid);
    let mut all = load_saved_skins(connection)?;
    all.retain(|skin| !is_entry(skin, &player_uuid, texture_key, variant));
    write_saved_skins(connection, &all)?;
    delete_texture_if_unused(&all, skins_dir, texture_key)
}

/// Changes the arms of one saved skin, keeping its file and its place. If the
/// texture is already saved with those arms, the two become one entry.
pub fn set_variant_in_library(
    connection: &Connection,
    player_uuid: &str,
    texture_key: &str,
    from: SkinVariant,
    to: SkinVariant,
) -> Result<(), SkinError> {
    if !is_texture_key(texture_key) {
        return Err(SkinError::invalid(format!(
            "'{texture_key}' is not a texture key"
        )));
    }
    if to == SkinVariant::Unknown {
        return Err(SkinError::invalid(
            "A skin is saved with classic or slim arms",
        ));
    }
    let player_uuid = normalize_uuid(player_uuid);
    let mut all = load_saved_skins(connection)?;
    let target_exists = all
        .iter()
        .any(|skin| is_entry(skin, &player_uuid, texture_key, to));
    if target_exists {
        all.retain(|skin| !is_entry(skin, &player_uuid, texture_key, from));
    } else if let Some(skin) = all
        .iter_mut()
        .find(|skin| is_entry(skin, &player_uuid, texture_key, from))
    {
        skin.variant = to;
    }
    write_saved_skins(connection, &all)
}

/// After an upload: the entries that pointed at the file the player saved now
/// point at the file Mojang made of it, and the old file goes if nobody uses
/// it. Two entries that become the same are kept once.
fn rekey_in_library(
    connection: &Connection,
    skins_dir: &Path,
    player_uuid: &str,
    old_key: &str,
    new_key: &str,
) -> Result<(), SkinError> {
    let mut all = load_saved_skins(connection)?;
    for skin in all
        .iter_mut()
        .filter(|skin| skin.player_uuid == player_uuid && skin.texture_key == old_key)
    {
        skin.texture_key = new_key.to_string();
    }
    let mut seen = Vec::new();
    all.retain(|skin| {
        let identity = (
            skin.player_uuid.clone(),
            skin.texture_key.clone(),
            skin.variant,
        );
        if seen.contains(&identity) {
            false
        } else {
            seen.push(identity);
            true
        }
    });
    write_saved_skins(connection, &all)?;
    delete_texture_if_unused(&all, skins_dir, old_key)
}

/// The skin just worn goes first in the player's list.
fn move_to_front(
    connection: &Connection,
    player_uuid: &str,
    texture_key: &str,
    variant: SkinVariant,
) -> Result<(), SkinError> {
    let mut all = load_saved_skins(connection)?;
    if let Some(index) = all
        .iter()
        .position(|skin| is_entry(skin, player_uuid, texture_key, variant))
    {
        let skin = all.remove(index);
        all.insert(0, skin);
        write_saved_skins(connection, &all)?;
    }
    Ok(())
}

// ── The reconciliation ─────────────────────────────────────────────────────

/// One list of cards from the library, the defaults, and what the profile
/// wears. The worn skin is **not** a card of its own when its texture and
/// arms are already a saved or a default card: that card is marked active.
/// Only when it matches nothing does it become a card, first in the list.
/// A saved card wins over a default with the same texture.
pub fn reconcile(saved: &[SavedSkin], active: Option<(&str, SkinVariant)>) -> Vec<ReconciledSkin> {
    let mut found = false;
    let mut is_worn = |texture_key: &str, variant: SkinVariant| {
        let worn = !found && active == Some((texture_key, variant));
        found |= worn;
        worn
    };

    let mut cards: Vec<ReconciledSkin> = saved
        .iter()
        .map(|skin| ReconciledSkin {
            source: SkinSource::Saved,
            texture_key: skin.texture_key.clone(),
            variant: skin.variant,
            name: skin.name.clone(),
            active: is_worn(&skin.texture_key, skin.variant),
        })
        .collect();
    cards.extend(DEFAULT_SKINS.iter().map(|skin| ReconciledSkin {
        source: SkinSource::Default,
        texture_key: skin.texture_key.to_string(),
        variant: skin.variant,
        name: Some(skin.name.to_string()),
        active: is_worn(skin.texture_key, skin.variant),
    }));

    if let Some((texture_key, variant)) = active {
        if !found {
            cards.insert(
                0,
                ReconciledSkin {
                    source: SkinSource::External,
                    texture_key: texture_key.to_string(),
                    variant,
                    name: None,
                    active: true,
                },
            );
        }
    }
    cards
}

fn build_view(
    ctx: &SkinContext,
    profile: Option<&MinecraftProfile>,
    profile_error: Option<SkinError>,
) -> Result<SkinsView, SkinError> {
    let saved = saved_skins_for(&open_database(&ctx.db_path)?, &ctx.player_uuid)?;
    let active = profile
        .and_then(MinecraftProfile::active_skin)
        .and_then(|skin| {
            skin.texture_key()
                .map(|texture_key| (texture_key, skin.variant))
        });

    let skins = reconcile(&saved, active)
        .into_iter()
        .filter_map(|skin| {
            let texture_url = match skin.source {
                // A saved entry whose file is gone can't be drawn or worn:
                // it is left out of the screen, not out of the library.
                SkinSource::Saved => {
                    data_url(&fs::read(texture_path(&ctx.skins_dir, &skin.texture_key).ok()?).ok()?)
                }
                SkinSource::Default => data_url(default_skin(&skin.texture_key, skin.variant)?.png),
                SkinSource::External => texture_url(&skin.texture_key),
            };
            Some(SkinCard { skin, texture_url })
        })
        .collect();

    let capes = profile
        .map(|profile| {
            profile
                .capes
                .iter()
                .map(|cape| CapeCard {
                    id: cape.id.clone(),
                    alias: cape.alias.clone(),
                    texture_url: cape
                        .url
                        .rsplit('/')
                        .next()
                        .filter(|texture_key| is_texture_key(texture_key))
                        .map(texture_url)
                        .unwrap_or_else(|| cape.url.clone()),
                    active: cape.state == TextureState::Active,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(SkinsView {
        player_uuid: ctx.player_uuid.clone(),
        player_name: profile
            .map(|profile| profile.name.clone())
            .or_else(|| ctx.player_name.clone()),
        skins,
        capes,
        profile_error,
    })
}

// ── The operations ─────────────────────────────────────────────────────────
//
// Each one opens the database only between network calls: nothing holds a
// connection across an `.await`.

fn open_database(db_path: &Path) -> Result<Connection, SkinError> {
    Connection::open(db_path).map_err(SkinError::library)
}

async fn current_profile<B: SkinBackend>(backend: &B) -> Result<MinecraftProfile, SkinError> {
    let profile = backend.profile().await?;
    backend.remember_profile(&profile);
    Ok(profile)
}

/// The profile for an operation that does not change it: the one last read if
/// it belongs to this player, else a fresh read.
async fn known_profile<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
) -> (Option<MinecraftProfile>, Option<SkinError>) {
    if let Some(profile) = backend
        .cached_profile()
        .filter(|profile| normalize_uuid(&profile.id) == ctx.player_uuid)
    {
        return (Some(profile), None);
    }
    match current_profile(backend).await {
        Ok(profile) => (Some(profile), None),
        Err(error) => (None, Some(error)),
    }
}

/// Before the worn skin is replaced: if it is neither saved nor a default,
/// it goes into the library, so that changing skin never loses one. It is
/// fetched from Mojang's public texture server and must hash to its key.
async fn keep_outgoing_skin<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    profile: &MinecraftProfile,
) -> Result<(), SkinError> {
    let Some(skin) = profile.active_skin() else {
        return Ok(());
    };
    let Some(texture_key) = skin.texture_key().filter(|key| is_texture_key(key)) else {
        return Ok(());
    };
    if skin.variant == SkinVariant::Unknown || default_skin(texture_key, skin.variant).is_some() {
        return Ok(());
    }
    let already_saved = saved_skins_for(&open_database(&ctx.db_path)?, &ctx.player_uuid)?
        .iter()
        .any(|saved| saved.texture_key == texture_key && saved.variant == skin.variant);
    if already_saved {
        return Ok(());
    }

    let png = download_verified(backend, texture_key).await?;
    add_to_library(
        &open_database(&ctx.db_path)?,
        &ctx.skins_dir,
        &ctx.player_uuid,
        &png,
        skin.variant,
        None,
    )?;
    Ok(())
}

async fn download_verified<B: SkinBackend>(
    backend: &B,
    texture_key: &str,
) -> Result<Vec<u8>, SkinError> {
    let png = backend.download_texture(texture_key).await?;
    if sha256_hex(&png) != texture_key {
        return Err(SkinError::new(
            SkinErrorKind::Mojang,
            format!("The texture server sent a file that is not {texture_key}"),
        ));
    }
    Ok(png)
}

/// The screen as it is now: asks Mojang for the profile, and shows the
/// library anyway if that fails.
pub(crate) async fn load_view<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
) -> Result<SkinsView, SkinError> {
    match current_profile(backend).await {
        Ok(profile) => build_view(ctx, Some(&profile), None),
        Err(error) => build_view(ctx, None, Some(error)),
    }
}

/// Puts a saved or default skin on the profile. The outgoing skin is kept
/// first; a saved skin then adopts the key Mojang gave its upload.
pub(crate) async fn equip<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    texture_key: &str,
    variant: SkinVariant,
) -> Result<SkinsView, SkinError> {
    let path = texture_path(&ctx.skins_dir, texture_key)?;
    let is_saved = saved_skins_for(&open_database(&ctx.db_path)?, &ctx.player_uuid)?
        .iter()
        .any(|skin| skin.texture_key == texture_key && skin.variant == variant);
    let png = if is_saved {
        fs::read(&path).map_err(SkinError::library)?
    } else if let Some(skin) = default_skin(texture_key, variant) {
        skin.png.to_vec()
    } else {
        return Err(SkinError::library(format!(
            "{texture_key} with {variant:?} arms is neither saved nor a default skin"
        )));
    };

    let current = current_profile(backend).await?;
    keep_outgoing_skin(ctx, backend, &current).await?;

    let profile = backend.upload_skin(png, variant).await?;
    backend.remember_profile(&profile);

    if is_saved {
        let worn = profile
            .active_skin()
            .and_then(|skin| skin.texture_key())
            .filter(|key| is_texture_key(key) && *key != texture_key)
            .map(str::to_string);
        let mut kept_key = texture_key.to_string();
        if let Some(worn) = worn {
            // The skin is on the profile whatever happens here: a failed
            // adoption leaves the entry under its local key, not an error.
            match adopt_mojang_texture(ctx, backend, texture_key, &worn).await {
                Ok(()) => kept_key = worn,
                Err(error) => eprintln!("[Skins] Keeping the local texture key: {error}"),
            }
        }
        move_to_front(
            &open_database(&ctx.db_path)?,
            &ctx.player_uuid,
            &kept_key,
            variant,
        )?;
    }

    build_view(ctx, Some(&profile), None)
}

async fn adopt_mojang_texture<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    local_key: &str,
    mojang_key: &str,
) -> Result<(), SkinError> {
    let png = download_verified(backend, mojang_key).await?;
    store_texture(&ctx.skins_dir, &png)?;
    rekey_in_library(
        &open_database(&ctx.db_path)?,
        &ctx.skins_dir,
        &ctx.player_uuid,
        local_key,
        mojang_key,
    )
}

/// Back to the default skin Mojang picks for the account, keeping the
/// outgoing one.
pub(crate) async fn reset<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
) -> Result<SkinsView, SkinError> {
    let current = current_profile(backend).await?;
    keep_outgoing_skin(ctx, backend, &current).await?;
    let profile = backend.reset_skin().await?;
    backend.remember_profile(&profile);
    build_view(ctx, Some(&profile), None)
}

/// Shows an owned cape, or hides the shown one with `None`.
pub(crate) async fn set_cape<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    cape_id: Option<&str>,
) -> Result<SkinsView, SkinError> {
    let profile = backend.set_cape(cape_id).await?;
    backend.remember_profile(&profile);
    build_view(ctx, Some(&profile), None)
}

/// Saves the skin the profile wears when it is in no list yet.
pub(crate) async fn save_worn<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
) -> Result<SkinsView, SkinError> {
    let current = current_profile(backend).await?;
    keep_outgoing_skin(ctx, backend, &current).await?;
    build_view(ctx, Some(&current), None)
}

/// Saves a PNG the user picked. Nothing is sent to Mojang.
pub(crate) async fn add_file<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    path: &Path,
    variant: SkinVariant,
    name: Option<String>,
) -> Result<SkinsView, SkinError> {
    let metadata = fs::metadata(path).map_err(SkinError::library)?;
    if !metadata.is_file() {
        return Err(SkinError::invalid(format!(
            "{} is not a file",
            path.display()
        )));
    }
    if metadata.len() > MAX_SKIN_BYTES {
        return Err(SkinError::invalid("The file is larger than any skin"));
    }
    let png = fs::read(path).map_err(SkinError::library)?;
    let name = name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        });
    add_to_library(
        &open_database(&ctx.db_path)?,
        &ctx.skins_dir,
        &ctx.player_uuid,
        &png,
        variant,
        name,
    )?;
    let (profile, error) = known_profile(ctx, backend).await;
    build_view(ctx, profile.as_ref(), error)
}

pub(crate) async fn set_saved_variant<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    texture_key: &str,
    from: SkinVariant,
    to: SkinVariant,
) -> Result<SkinsView, SkinError> {
    set_variant_in_library(
        &open_database(&ctx.db_path)?,
        &ctx.player_uuid,
        texture_key,
        from,
        to,
    )?;
    let (profile, error) = known_profile(ctx, backend).await;
    build_view(ctx, profile.as_ref(), error)
}

pub(crate) async fn remove_saved<B: SkinBackend>(
    ctx: &SkinContext,
    backend: &B,
    texture_key: &str,
    variant: SkinVariant,
) -> Result<SkinsView, SkinError> {
    remove_from_library(
        &open_database(&ctx.db_path)?,
        &ctx.skins_dir,
        &ctx.player_uuid,
        texture_key,
        variant,
    )?;
    let (profile, error) = known_profile(ctx, backend).await;
    build_view(ctx, profile.as_ref(), error)
}

// ── The commands ───────────────────────────────────────────────────────────
//
// Each returns the whole screen. The error is an object (`SkinError`), not a
// string: the frontend reads `kind`, and `retryAfterSeconds` for a 429.

/// The active account's player, from the database: the library opens without
/// a network.
fn skin_context(paths: &LauncherPaths) -> Result<SkinContext, SkinError> {
    let connection = open_database(paths.database_path())?;
    let account = AccountsRepository::new(&connection)
        .load_active_account()
        .map_err(SkinError::library)?
        .ok_or_else(|| {
            SkinError::new(
                SkinErrorKind::NotSignedIn,
                "No Microsoft account is signed in",
            )
        })?;
    let player_uuid = account
        .minecraft_uuid
        .as_deref()
        .map(normalize_uuid)
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| {
            SkinError::new(
                SkinErrorKind::NotSignedIn,
                "The account has no Minecraft profile",
            )
        })?;
    let player_name = resolve_cached_profile_username(
        account.profile_data.as_deref(),
        account.xbox_gamertag.as_deref(),
    )
    .ok()
    .flatten();
    Ok(SkinContext {
        db_path: paths.database_path().to_path_buf(),
        skins_dir: paths.skins_dir().to_path_buf(),
        player_uuid,
        player_name,
    })
}

#[tauri::command]
pub async fn load_skins_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    load_view(&ctx, &LiveBackend::new(&launcher_paths, &skins)?).await
}

#[tauri::command]
pub async fn add_skin_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
    path: String,
    variant: SkinVariant,
    name: Option<String>,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    let backend = LiveBackend::new(&launcher_paths, &skins)?;
    add_file(&ctx, &backend, Path::new(&path), variant, name).await
}

#[tauri::command]
pub async fn set_saved_skin_variant_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
    texture_key: String,
    variant: SkinVariant,
    new_variant: SkinVariant,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    let backend = LiveBackend::new(&launcher_paths, &skins)?;
    set_saved_variant(&ctx, &backend, &texture_key, variant, new_variant).await
}

#[tauri::command]
pub async fn remove_saved_skin_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
    texture_key: String,
    variant: SkinVariant,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    let backend = LiveBackend::new(&launcher_paths, &skins)?;
    remove_saved(&ctx, &backend, &texture_key, variant).await
}

#[tauri::command]
pub async fn equip_skin_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
    texture_key: String,
    variant: SkinVariant,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    let backend = LiveBackend::new(&launcher_paths, &skins)?;
    equip(&ctx, &backend, &texture_key, variant).await
}

#[tauri::command]
pub async fn reset_skin_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    reset(&ctx, &LiveBackend::new(&launcher_paths, &skins)?).await
}

#[tauri::command]
pub async fn set_cape_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
    cape_id: Option<String>,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    let backend = LiveBackend::new(&launcher_paths, &skins)?;
    set_cape(&ctx, &backend, cape_id.as_deref()).await
}

#[tauri::command]
pub async fn save_worn_skin_command(
    launcher_paths: State<'_, LauncherPaths>,
    skins: State<'_, SkinsState>,
) -> Result<SkinsView, SkinError> {
    let ctx = skin_context(&launcher_paths)?;
    save_worn(&ctx, &LiveBackend::new(&launcher_paths, &skins)?).await
}
