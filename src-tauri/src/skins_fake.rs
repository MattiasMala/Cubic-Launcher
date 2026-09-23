//! A Mojang that lives in the process, for trying the skin screen without
//! spending real writes on a real account (E7 phase 2).
//!
//! Compiled **only** with the `skins-fake-backend` feature, which no release
//! build turns on: `npm run tauri dev -- --features skins-fake-backend`. With
//! the feature every skin command talks to this fake and the live backend is
//! unreachable; without it this file does not exist in the binary.
//!
//! It answers like the Mojang measured in phase 1 (report 061): every write
//! returns the whole profile, an upload is **re-encoded** (the key it comes
//! back under is not the sha256 of what was sent, unless the file is already
//! one of Mojang's), the reset puts Kai slim on, and the player owns Pan and
//! Migrator. Public textures it does not hold are fetched from Mojang's
//! texture server, which is unauthenticated.
//!
//! Its library is its own: a scratch folder (`CUBIC_FAKE_SKINS_ROOT`, else
//! `<temp>/cubic-fake-skins`) with its own database and `skins/`, so nothing
//! the fake does reaches the real account's library.
//!
//! `CUBIC_FAKE_SKINS_SCENARIO` picks a failure to draw:
//! - `profile-error`: Mojang does not answer (every call is a network error);
//! - `rate-limited`: reads work, every write answers 429 with `Retry-After: 42`.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::database::initialize_database;
use crate::launcher_paths::LauncherPaths;
use crate::microsoft_auth::{
    MinecraftProfile, ProfileCape, ProfileSkin, SkinVariant, TextureState,
};

use super::{
    classify_mojang_error, texture_url, validate_skin_png, SkinBackend, SkinContext, SkinError,
    SkinErrorKind, SkinsState, DEFAULT_SKINS,
};

const PLAYER_UUID: &str = "00000000fa4e40000000000000000000";
const PLAYER_NAME: &str = "FakePlayer";
/// A public 64×32 legacy texture, so the first screen has an external card.
const FIRST_WORN: &str = "9df9e241bf5af8500d7146fcecdb36606d8b885ae41d0e08a4539ccf7221655c";
const KAI_SLIM: &str = "226c617fde5b1ba569aa08bd2cb6fd84c93337532a872b3eb7bf66bdd5b395f8";
const PAN: &str = "28de4a81688ad18b49e735a273e086c18f1e3966956123ccb574034c06f5d336";
const MIGRATOR: &str = "2340c0e03dd24a11b15a8b33c2a7e9e32abb2051b2481d0ba7defd635ca7a933";
/// Long enough to see the busy state, short enough not to wait on it.
const LATENCY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Normal,
    ProfileError,
    RateLimited,
}

fn scenario() -> Scenario {
    match env::var("CUBIC_FAKE_SKINS_SCENARIO").as_deref() {
        Ok("profile-error") => Scenario::ProfileError,
        Ok("rate-limited") => Scenario::RateLimited,
        _ => Scenario::Normal,
    }
}

struct FakeMojang {
    profile: MinecraftProfile,
    /// Files it serves, by key: every one hashes to its key.
    textures: HashMap<String, Vec<u8>>,
    /// Keys of files Mojang made, which an upload leaves as they are.
    own_files: HashSet<String>,
}

static FAKE: LazyLock<Mutex<FakeMojang>> = LazyLock::new(|| {
    let mut own_files: HashSet<String> = DEFAULT_SKINS
        .iter()
        .map(|skin| skin.texture_key.to_string())
        .collect();
    own_files.insert(FIRST_WORN.to_string());
    Mutex::new(FakeMojang {
        profile: MinecraftProfile {
            id: PLAYER_UUID.to_string(),
            name: PLAYER_NAME.to_string(),
            skins: vec![worn(FIRST_WORN, SkinVariant::Classic)],
            capes: vec![
                cape("cape-pan", PAN, "Pan"),
                cape("cape-migrator", MIGRATOR, "Migrator"),
            ],
        },
        textures: HashMap::new(),
        own_files,
    })
});

fn worn(texture_key: &str, variant: SkinVariant) -> ProfileSkin {
    ProfileSkin {
        id: format!("fake-{}", &texture_key[..8]),
        state: TextureState::Active,
        url: format!("http://textures.minecraft.net/texture/{texture_key}"),
        texture_key: Some(texture_key.to_string()),
        variant,
        alias: None,
    }
}

fn cape(id: &str, texture_key: &str, alias: &str) -> ProfileCape {
    ProfileCape {
        id: id.to_string(),
        state: TextureState::Inactive,
        url: format!("http://textures.minecraft.net/texture/{texture_key}"),
        alias: Some(alias.to_string()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// What Mojang does to an upload, as far as the launcher can tell: the same
/// pixels in different bytes. Here a `tEXt` chunk after `IHDR` (8-byte
/// signature + 25-byte `IHDR` chunk), which any PNG reader skips.
fn re_encode(png: &[u8]) -> Vec<u8> {
    let data = b"Comment\0re-encoded by the fake Mojang";
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(b"tEXt");
    crc.update(data);
    chunk.extend_from_slice(&crc.sum().to_be_bytes());

    let split = 33.min(png.len());
    let mut out = png[..split].to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&png[split..]);
    out
}

fn unreachable() -> SkinError {
    SkinError::new(
        SkinErrorKind::Network,
        "Couldn't reach Mojang: the fake backend is playing unreachable",
    )
}

/// The fake as the commands see it. Writes go through the launcher's own
/// write guard and a 429 is reported to it, as in `LiveBackend::signed`, so
/// the screen meets the same refusals it would meet live.
pub(crate) struct FakeBackend<'a> {
    state: &'a SkinsState,
}

impl<'a> FakeBackend<'a> {
    pub(crate) fn new(_paths: &'a LauncherPaths, state: &'a SkinsState) -> Result<Self, SkinError> {
        Ok(Self { state })
    }

    async fn before_call(&self, is_write: bool) -> Result<(), SkinError> {
        if is_write {
            self.state.guard.lock().admit(Instant::now())?;
        }
        tokio::time::sleep(LATENCY).await;
        match scenario() {
            Scenario::ProfileError => Err(unreachable()),
            Scenario::RateLimited if is_write => {
                let error = classify_mojang_error(429, Some("42"), "");
                self.state.guard.lock().rate_limited(Instant::now(), 42);
                Err(error)
            }
            _ => Ok(()),
        }
    }
}

impl SkinBackend for FakeBackend<'_> {
    async fn profile(&self) -> Result<MinecraftProfile, SkinError> {
        self.before_call(false).await?;
        Ok(FAKE.lock().profile.clone())
    }

    async fn upload_skin(
        &self,
        png: Vec<u8>,
        variant: SkinVariant,
    ) -> Result<MinecraftProfile, SkinError> {
        self.before_call(true).await?;
        if validate_skin_png(&png).is_err() {
            return Err(classify_mojang_error(
                400,
                None,
                r#"{"details":{"status":"INVALID_IMAGE_DATA"},"errorMessage":"Invalid image data"}"#,
            ));
        }
        let mut fake = FAKE.lock();
        let sent = sha256_hex(&png);
        let (key, served) = if fake.own_files.contains(&sent) {
            (sent, png)
        } else {
            let served = re_encode(&png);
            (sha256_hex(&served), served)
        };
        fake.own_files.insert(key.clone());
        fake.textures.insert(key.clone(), served);
        fake.profile.skins = vec![worn(&key, variant)];
        Ok(fake.profile.clone())
    }

    async fn reset_skin(&self) -> Result<MinecraftProfile, SkinError> {
        self.before_call(true).await?;
        let mut fake = FAKE.lock();
        let mut kai = worn(KAI_SLIM, SkinVariant::Slim);
        kai.alias = Some("KAI".to_string());
        fake.profile.skins = vec![kai];
        Ok(fake.profile.clone())
    }

    async fn set_cape(&self, cape_id: Option<&str>) -> Result<MinecraftProfile, SkinError> {
        self.before_call(true).await?;
        let mut fake = FAKE.lock();
        if let Some(id) = cape_id {
            if !fake.profile.capes.iter().any(|cape| cape.id == id) {
                return Err(classify_mojang_error(
                    400,
                    None,
                    r#"{"errorMessage":"Invalid cape id"}"#,
                ));
            }
        }
        for cape in &mut fake.profile.capes {
            cape.state = if Some(cape.id.as_str()) == cape_id {
                TextureState::Active
            } else {
                TextureState::Inactive
            };
        }
        Ok(fake.profile.clone())
    }

    async fn download_texture(&self, texture_key: &str) -> Result<Vec<u8>, SkinError> {
        if scenario() == Scenario::ProfileError {
            return Err(unreachable());
        }
        let held = FAKE.lock().textures.get(texture_key).cloned();
        if let Some(png) = held {
            return Ok(png);
        }
        if let Some(skin) = DEFAULT_SKINS
            .iter()
            .find(|skin| skin.texture_key == texture_key)
        {
            return Ok(skin.png.to_vec());
        }
        let response = reqwest::get(texture_url(texture_key))
            .await
            .map_err(|error| SkinError::new(SkinErrorKind::Network, format!("{error}")))?;
        if !response.status().is_success() {
            return Err(SkinError::new(
                SkinErrorKind::Mojang,
                format!("The texture server answered {}", response.status()),
            ));
        }
        let png = response
            .bytes()
            .await
            .map_err(|error| SkinError::new(SkinErrorKind::Network, format!("{error}")))?
            .to_vec();
        FAKE.lock()
            .textures
            .insert(texture_key.to_string(), png.clone());
        Ok(png)
    }

    fn cached_profile(&self) -> Option<MinecraftProfile> {
        self.state.last_profile.lock().clone()
    }

    fn remember_profile(&self, profile: &MinecraftProfile) {
        *self.state.last_profile.lock() = Some(profile.clone());
    }
}

/// The fake player's library, in its scratch folder.
pub(crate) fn skin_context(_paths: &LauncherPaths) -> Result<SkinContext, SkinError> {
    let root = env::var_os("CUBIC_FAKE_SKINS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("cubic-fake-skins"));
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).map_err(SkinError::library)?;
    let db_path = root.join("launcher_data.db");
    initialize_database(&db_path).map_err(SkinError::library)?;
    Ok(SkinContext {
        db_path,
        skins_dir,
        player_uuid: PLAYER_UUID.to_string(),
        player_name: Some(PLAYER_NAME.to_string()),
    })
}
