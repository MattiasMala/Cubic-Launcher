use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::database::initialize_database;
use crate::microsoft_auth::{ProfileCape, ProfileSkin, TextureState};

use super::*;

const PLAYER: &str = "0123456789abcdef0123456789abcdef";
const OTHER_PLAYER: &str = "fedcba9876543210fedcba9876543210";
const KAI_SLIM: &str = "226c617fde5b1ba569aa08bd2cb6fd84c93337532a872b3eb7bf66bdd5b395f8";

fn unique_root(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    env::temp_dir().join(format!("cubic-skins-{tag}-{stamp}"))
}

fn open_test_database(root: &Path) -> Connection {
    fs::create_dir_all(root).expect("failed to create the root");
    let database_path = root.join("launcher_data.db");
    initialize_database(&database_path).expect("database should initialize");
    Connection::open(&database_path).expect("database should open")
}

fn context(root: &Path) -> SkinContext {
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).expect("failed to create the skins folder");
    SkinContext {
        db_path: root.join("launcher_data.db"),
        skins_dir,
        player_uuid: PLAYER.to_string(),
        player_name: Some("Player".to_string()),
    }
}

/// A PNG as far as the local check reads it: signature and IHDR. `salt`
/// makes two textures of the same size different files.
fn fake_png(width: u32, height: u32, salt: &[u8]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&[0; 4]);
    png.extend_from_slice(salt);
    png
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn skin(texture_key: &str, variant: SkinVariant, state: TextureState) -> ProfileSkin {
    ProfileSkin {
        id: "skin-id".to_string(),
        state,
        url: format!("http://textures.minecraft.net/texture/{texture_key}"),
        texture_key: Some(texture_key.to_string()),
        variant,
        alias: None,
    }
}

fn profile_wearing(texture_key: &str, variant: SkinVariant) -> MinecraftProfile {
    MinecraftProfile {
        id: PLAYER.to_string(),
        name: "Player".to_string(),
        skins: vec![skin(texture_key, variant, TextureState::Active)],
        capes: vec![
            ProfileCape {
                id: "cape-pan".to_string(),
                state: TextureState::Inactive,
                url: "http://textures.minecraft.net/texture/28de4a81688ad18b49e735a273e086c18f1e3966956123ccb574034c06f5d336".to_string(),
                alias: Some("Pan".to_string()),
            },
            ProfileCape {
                id: "cape-migrator".to_string(),
                state: TextureState::Inactive,
                url: "http://textures.minecraft.net/texture/2340c0e03dd24a11b15a8b33c2a7e9e32abb2051b2481d0ba7defd635ca7a933".to_string(),
                alias: Some("Migrator".to_string()),
            },
        ],
    }
}

fn saved(player: &str, texture_key: &str, variant: SkinVariant) -> SavedSkin {
    SavedSkin {
        player_uuid: player.to_string(),
        texture_key: texture_key.to_string(),
        variant,
        name: None,
    }
}

fn active_cards(view: &[ReconciledSkin]) -> Vec<(SkinSource, String, SkinVariant)> {
    view.iter()
        .filter(|card| card.active)
        .map(|card| (card.source, card.texture_key.clone(), card.variant))
        .collect()
}

// ── A Mojang that answers like the one measured in phase A ─────────────────

/// Uploads re-encode: the profile comes back with `canonical_key`, whose PNG
/// is `canonical_png`, exactly as Mojang answered a 64×64 upload with a key
/// that was not the sha256 of the uploaded file.
struct FakeMojang {
    profile: Mutex<MinecraftProfile>,
    textures: HashMap<String, Vec<u8>>,
    canonical: Option<(String, Vec<u8>)>,
    upload_error: Option<SkinError>,
    uploads: Mutex<Vec<(Vec<u8>, SkinVariant)>>,
    resets: Mutex<usize>,
    calls: Mutex<usize>,
    cache: Mutex<Option<MinecraftProfile>>,
}

impl FakeMojang {
    fn wearing(profile: MinecraftProfile) -> Self {
        Self {
            profile: Mutex::new(profile),
            textures: HashMap::new(),
            canonical: None,
            upload_error: None,
            uploads: Mutex::new(Vec::new()),
            resets: Mutex::new(0),
            calls: Mutex::new(0),
            cache: Mutex::new(None),
        }
    }

    fn serving(mut self, png: Vec<u8>) -> Self {
        self.textures.insert(sha(&png), png);
        self
    }

    fn call(&self) {
        *self.calls.lock() += 1;
    }

    fn wear(&self, texture_key: &str, variant: SkinVariant) -> MinecraftProfile {
        let mut profile = self.profile.lock();
        profile.skins = vec![skin(texture_key, variant, TextureState::Active)];
        profile.clone()
    }
}

impl SkinBackend for FakeMojang {
    async fn profile(&self) -> Result<MinecraftProfile, SkinError> {
        self.call();
        Ok(self.profile.lock().clone())
    }

    async fn upload_skin(
        &self,
        png: Vec<u8>,
        variant: SkinVariant,
    ) -> Result<MinecraftProfile, SkinError> {
        self.call();
        if let Some(error) = &self.upload_error {
            return Err(error.clone());
        }
        let key = match &self.canonical {
            Some((key, _)) => key.clone(),
            None => sha(&png),
        };
        self.uploads.lock().push((png, variant));
        Ok(self.wear(&key, variant))
    }

    async fn reset_skin(&self) -> Result<MinecraftProfile, SkinError> {
        self.call();
        *self.resets.lock() += 1;
        Ok(self.wear(KAI_SLIM, SkinVariant::Slim))
    }

    async fn set_cape(&self, cape_id: Option<&str>) -> Result<MinecraftProfile, SkinError> {
        self.call();
        let mut profile = self.profile.lock();
        for cape in &mut profile.capes {
            cape.state = if Some(cape.id.as_str()) == cape_id {
                TextureState::Active
            } else {
                TextureState::Inactive
            };
        }
        Ok(profile.clone())
    }

    async fn download_texture(&self, texture_key: &str) -> Result<Vec<u8>, SkinError> {
        self.call();
        if let Some((key, png)) = &self.canonical {
            if key == texture_key {
                return Ok(png.clone());
            }
        }
        self.textures
            .get(texture_key)
            .cloned()
            .ok_or_else(|| SkinError::new(SkinErrorKind::Network, "not served"))
    }

    fn cached_profile(&self) -> Option<MinecraftProfile> {
        self.cache.lock().clone()
    }

    fn remember_profile(&self, profile: &MinecraftProfile) {
        *self.cache.lock() = Some(profile.clone());
    }
}

// ── The PNG check ──────────────────────────────────────────────────────────

#[test]
fn only_modern_and_legacy_skin_sizes_pass_the_local_check() {
    assert!(validate_skin_png(&fake_png(64, 64, b"")).is_ok());
    assert!(
        validate_skin_png(&fake_png(64, 32, b"")).is_ok(),
        "64×32 is accepted: Mojang took the legacy file as it is (phase A)"
    );

    for (bytes, why) in [
        (fake_png(32, 32, b""), "wrong size"),
        (fake_png(128, 128, b""), "HD skins are not a Mojang upload"),
        (b"GIF89a-not-a-png".to_vec(), "not a PNG"),
        (b"\x89PNG\r\n\x1a\n".to_vec(), "signature without IHDR"),
    ] {
        let error = validate_skin_png(&bytes).expect_err(why);
        assert_eq!(error.kind, SkinErrorKind::InvalidSkin, "{why}");
    }
}

// ── The embedded defaults ──────────────────────────────────────────────────

#[test]
fn the_default_skins_are_the_eighteen_vanilla_textures_addressed_by_their_hash() {
    assert_eq!(DEFAULT_SKINS.len(), 18);

    let mut names: Vec<(&str, SkinVariant)> = DEFAULT_SKINS
        .iter()
        .map(|skin| (skin.name, skin.variant))
        .collect();
    names.sort_by_key(|(name, variant)| (*name, *variant == SkinVariant::Slim));
    names.dedup();
    assert_eq!(names.len(), 18, "nine names, each classic and slim");
    for name in [
        "Alex", "Ari", "Efe", "Kai", "Makena", "Noor", "Steve", "Sunny", "Zuri",
    ] {
        for variant in [SkinVariant::Classic, SkinVariant::Slim] {
            assert!(
                names.contains(&(name, variant)),
                "{name} {variant:?} is missing"
            );
        }
    }

    for skin in DEFAULT_SKINS.iter() {
        assert_eq!(
            sha(skin.png),
            skin.texture_key,
            "{} {:?}: the embedded file must be the one Mojang serves under that key",
            skin.name,
            skin.variant
        );
        assert_eq!(
            validate_skin_png(skin.png).expect("a default must pass the check"),
            (64, 64)
        );
    }
}

// ── Mojang's errors ────────────────────────────────────────────────────────

#[test]
fn mojang_errors_are_classified_by_status_not_by_text() {
    let limited = classify_mojang_error(429, Some("30"), "");
    assert_eq!(limited.kind, SkinErrorKind::RateLimited);
    assert_eq!(limited.retry_after_seconds, Some(30));

    let limited = classify_mojang_error(429, None, r#"{"errorMessage":"anything at all"}"#);
    assert_eq!(limited.kind, SkinErrorKind::RateLimited);
    assert_eq!(
        limited.retry_after_seconds,
        Some(DEFAULT_RATE_LIMIT_COOLDOWN_SECONDS)
    );

    let not_limited =
        classify_mojang_error(400, None, r#"{"errorMessage":"429 Too Many Requests"}"#);
    assert_ne!(
        not_limited.kind,
        SkinErrorKind::RateLimited,
        "a body that talks about 429 is not a 429"
    );

    // The body Mojang returned to a malformed PNG in phase A.
    let invalid = classify_mojang_error(
        400,
        None,
        r#"{"path":"/minecraft/profile/skins","details":{"status":"INVALID_IMAGE_DATA"},"errorMessage":"Invalid image data"}"#,
    );
    assert_eq!(invalid.kind, SkinErrorKind::InvalidSkin);
    assert_eq!(invalid.message, "Invalid image data");

    let expired = classify_mojang_error(401, None, "");
    assert_eq!(expired.kind, SkinErrorKind::NotSignedIn);
    assert_eq!(expired.status, Some(401));

    let broken = classify_mojang_error(503, None, "upstream down");
    assert_eq!(broken.kind, SkinErrorKind::Mojang);
    assert_eq!(broken.status, Some(503));
}

// ── The write guard ────────────────────────────────────────────────────────

#[test]
fn the_write_guard_keeps_the_assumed_rate_and_honours_a_429() {
    let start = Instant::now();
    let mut guard = WriteGuard::default();

    for index in 0..ASSUMED_WRITES_PER_MINUTE {
        guard
            .admit(start + Duration::from_millis(index as u64))
            .expect("writes within the assumed rate pass");
    }
    let refused = guard
        .admit(start + Duration::from_secs(1))
        .expect_err("one more inside the same minute is refused locally");
    assert_eq!(refused.kind, SkinErrorKind::RateLimited);
    assert!(refused
        .retry_after_seconds
        .is_some_and(|seconds| seconds >= 1));
    guard
        .admit(start + Duration::from_secs(61))
        .expect("a minute later the window is free again");

    let mut guard = WriteGuard::default();
    guard.rate_limited(start, 30);
    assert_eq!(
        guard
            .admit(start + Duration::from_secs(29))
            .expect_err("after a 429 nothing is sent until Retry-After")
            .kind,
        SkinErrorKind::RateLimited
    );
    guard
        .admit(start + Duration::from_secs(31))
        .expect("after Retry-After writes pass again");
}

// ── The library ────────────────────────────────────────────────────────────

#[test]
fn changing_the_arms_of_a_saved_skin_keeps_its_file_and_its_place() {
    let root = unique_root("arms");
    let connection = open_test_database(&root);
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).unwrap();
    let first = fake_png(64, 64, b"first");
    let second = fake_png(64, 64, b"second");
    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &first,
        SkinVariant::Classic,
        None,
    )
    .unwrap();
    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &second,
        SkinVariant::Classic,
        None,
    )
    .unwrap();

    set_variant_in_library(
        &connection,
        PLAYER,
        &sha(&first),
        SkinVariant::Classic,
        SkinVariant::Slim,
    )
    .unwrap();
    assert_eq!(
        saved_skins_for(&connection, PLAYER).unwrap(),
        vec![
            saved(PLAYER, &sha(&second), SkinVariant::Classic),
            saved(PLAYER, &sha(&first), SkinVariant::Slim),
        ]
    );
    assert!(
        skins_dir.join(format!("{}.png", sha(&first))).exists(),
        "the file stays"
    );

    // Already saved with the other arms: the two become one entry.
    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &second,
        SkinVariant::Slim,
        None,
    )
    .unwrap();
    set_variant_in_library(
        &connection,
        PLAYER,
        &sha(&second),
        SkinVariant::Classic,
        SkinVariant::Slim,
    )
    .unwrap();
    assert_eq!(
        saved_skins_for(&connection, PLAYER)
            .unwrap()
            .iter()
            .filter(|skin| skin.texture_key == sha(&second))
            .count(),
        1
    );

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn renaming_names_one_entry_in_place_and_an_empty_name_clears_it() {
    let root = unique_root("rename");
    let connection = open_test_database(&root);
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).unwrap();
    let first = fake_png(64, 64, b"first");
    let second = fake_png(64, 64, b"second");
    for (player, png) in [(PLAYER, &first), (PLAYER, &second), (OTHER_PLAYER, &first)] {
        add_to_library(
            &connection,
            &skins_dir,
            player,
            png,
            SkinVariant::Classic,
            None,
        )
        .unwrap();
    }

    rename_in_library(
        &connection,
        PLAYER,
        &sha(&first),
        SkinVariant::Classic,
        Some("  Knight  ".to_string()),
    )
    .unwrap();
    let named = |player: &str| -> Vec<(String, Option<String>)> {
        saved_skins_for(&connection, player)
            .unwrap()
            .into_iter()
            .map(|skin| (skin.texture_key, skin.name))
            .collect()
    };
    assert_eq!(
        named(PLAYER),
        vec![
            (sha(&second), None),
            (sha(&first), Some("Knight".to_string())),
        ],
        "trimmed, and the entry keeps its place"
    );
    assert_eq!(
        named(OTHER_PLAYER),
        vec![(sha(&first), None)],
        "the other player's entry for the same file is another entry"
    );

    rename_in_library(
        &connection,
        PLAYER,
        &sha(&first),
        SkinVariant::Classic,
        Some("   ".to_string()),
    )
    .unwrap();
    assert_eq!(named(PLAYER)[1], (sha(&first), None));

    let error = rename_in_library(
        &connection,
        PLAYER,
        &sha(&first),
        SkinVariant::Slim,
        Some("Slim".to_string()),
    )
    .expect_err("no entry with slim arms");
    assert_eq!(error.kind, SkinErrorKind::Library);

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn saved_skins_are_files_named_by_their_hash_with_entries_per_player() {
    let root = unique_root("library");
    let connection = open_test_database(&root);
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).unwrap();
    let first = fake_png(64, 32, b"first");
    let second = fake_png(64, 64, b"second");

    let added = add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &first,
        SkinVariant::Classic,
        Some("Mine".into()),
    )
    .expect("a valid skin is saved");
    assert_eq!(added.texture_key, sha(&first));
    assert_eq!(
        fs::read(skins_dir.join(format!("{}.png", sha(&first)))).expect("the file is on disk"),
        first,
        "the file is kept as it came: a 64×32 is not normalized"
    );

    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &second,
        SkinVariant::Slim,
        None,
    )
    .unwrap();
    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &first,
        SkinVariant::Classic,
        None,
    )
    .unwrap();
    let mine = saved_skins_for(&connection, PLAYER).unwrap();
    assert_eq!(
        mine.iter()
            .map(|skin| skin.texture_key.clone())
            .collect::<Vec<_>>(),
        vec![sha(&second), sha(&first)],
        "newest first, and the same texture and variant is not saved twice"
    );
    assert_eq!(mine[1].name.as_deref(), Some("Mine"));

    add_to_library(
        &connection,
        &skins_dir,
        OTHER_PLAYER,
        &first,
        SkinVariant::Classic,
        None,
    )
    .unwrap();
    assert_eq!(saved_skins_for(&connection, OTHER_PLAYER).unwrap().len(), 1);
    assert_eq!(
        saved_skins_for(&connection, PLAYER).unwrap().len(),
        2,
        "each account has its own list"
    );

    remove_from_library(
        &connection,
        &skins_dir,
        PLAYER,
        &sha(&first),
        SkinVariant::Classic,
    )
    .unwrap();
    assert!(
        skins_dir.join(format!("{}.png", sha(&first))).exists(),
        "another account still uses the file"
    );
    remove_from_library(
        &connection,
        &skins_dir,
        OTHER_PLAYER,
        &sha(&first),
        SkinVariant::Classic,
    )
    .unwrap();
    assert!(
        !skins_dir.join(format!("{}.png", sha(&first))).exists(),
        "nobody uses it any more"
    );

    let unknown = add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &first,
        SkinVariant::Unknown,
        None,
    )
    .expect_err("a skin is saved as classic or slim");
    assert_eq!(unknown.kind, SkinErrorKind::InvalidSkin);

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unreadable_entry_is_dropped_not_the_whole_library() {
    let root = unique_root("lenient");
    let connection = open_test_database(&root);
    let skins_dir = root.join("skins");
    fs::create_dir_all(&skins_dir).unwrap();
    let key = sha(b"whatever");
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
            [
                SAVED_SKINS_KEY.to_string(),
                format!(
                    r#"[{{"playerUuid":"{PLAYER}","textureKey":"{key}","variant":"slim"}}, 42, {{"playerUuid":"{PLAYER}"}}]"#
                ),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES ('min_ram_mb', '4096')",
            [],
        )
        .unwrap();

    assert_eq!(
        saved_skins_for(&connection, PLAYER).unwrap(),
        vec![saved(PLAYER, &key, SkinVariant::Slim)]
    );

    add_to_library(
        &connection,
        &skins_dir,
        PLAYER,
        &fake_png(64, 64, b"new"),
        SkinVariant::Classic,
        None,
    )
    .unwrap();
    let ram: String = connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = 'min_ram_mb'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ram, "4096", "the other settings are not touched");
    assert_eq!(saved_skins_for(&connection, PLAYER).unwrap().len(), 2);

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

// ── The reconciliation ─────────────────────────────────────────────────────

#[test]
fn the_active_skin_marks_the_saved_card_that_has_it_and_adds_nothing() {
    let key = sha(b"mine");
    let cards = reconcile(
        &[saved(PLAYER, &key, SkinVariant::Classic)],
        Some((&key, SkinVariant::Classic)),
    );

    assert_eq!(
        active_cards(&cards),
        vec![(SkinSource::Saved, key, SkinVariant::Classic)]
    );
    assert!(cards.iter().all(|card| card.source != SkinSource::External));
    assert_eq!(cards.len(), 1 + DEFAULT_SKINS.len());
}

#[test]
fn the_active_skin_marks_the_default_that_has_it() {
    let cards = reconcile(&[], Some((KAI_SLIM, SkinVariant::Slim)));

    let active: Vec<_> = cards.iter().filter(|card| card.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].source, SkinSource::Default);
    assert_eq!(active[0].name.as_deref(), Some("Kai"));
    assert_eq!(cards.len(), DEFAULT_SKINS.len());
}

#[test]
fn an_active_skin_found_nowhere_becomes_a_card_of_its_own() {
    let key = sha(b"from somewhere else");
    let cards = reconcile(
        &[saved(PLAYER, &sha(b"mine"), SkinVariant::Classic)],
        Some((&key, SkinVariant::Classic)),
    );

    assert_eq!(
        cards[0].source,
        SkinSource::External,
        "the current skin comes first"
    );
    assert_eq!(
        active_cards(&cards),
        vec![(SkinSource::External, key, SkinVariant::Classic)]
    );
    assert_eq!(cards.len(), 2 + DEFAULT_SKINS.len());
}

#[test]
fn the_same_texture_on_the_other_arms_is_not_the_same_skin() {
    let cards = reconcile(&[], Some((KAI_SLIM, SkinVariant::Classic)));

    assert_eq!(
        active_cards(&cards),
        vec![(
            SkinSource::External,
            KAI_SLIM.to_string(),
            SkinVariant::Classic
        )],
        "Kai's slim texture worn with classic arms is not the Kai slim card"
    );
}

#[test]
fn without_a_profile_no_card_is_active() {
    let cards = reconcile(&[saved(PLAYER, &sha(b"mine"), SkinVariant::Classic)], None);
    assert!(active_cards(&cards).is_empty());
    assert_eq!(cards.len(), 1 + DEFAULT_SKINS.len());
}

// ── The operations ─────────────────────────────────────────────────────────

#[tokio::test]
async fn equipping_a_saved_skin_keeps_the_outgoing_one_and_adopts_mojangs_key() {
    let root = unique_root("equip");
    let connection = open_test_database(&root);
    let ctx = context(&root);

    let local = fake_png(64, 64, b"as the user saved it");
    let canonical = fake_png(64, 64, b"as Mojang re-encoded it");
    let outgoing = fake_png(64, 32, b"worn before");
    add_to_library(
        &connection,
        &ctx.skins_dir,
        PLAYER,
        &local,
        SkinVariant::Slim,
        Some("New".into()),
    )
    .unwrap();
    drop(connection);

    let mut mojang = FakeMojang::wearing(profile_wearing(&sha(&outgoing), SkinVariant::Classic))
        .serving(outgoing.clone());
    mojang.canonical = Some((sha(&canonical), canonical.clone()));

    let view = equip(&ctx, &mojang, &sha(&local), SkinVariant::Slim)
        .await
        .expect("the equip succeeds");

    assert_eq!(
        *mojang.uploads.lock(),
        vec![(local.clone(), SkinVariant::Slim)]
    );

    let connection = Connection::open(&ctx.db_path).unwrap();
    let mine = saved_skins_for(&connection, PLAYER).unwrap();
    assert_eq!(
        mine.iter()
            .map(|skin| (skin.texture_key.clone(), skin.variant))
            .collect::<Vec<_>>(),
        vec![
            (sha(&canonical), SkinVariant::Slim),
            (sha(&outgoing), SkinVariant::Classic),
        ],
        "the outgoing skin is kept, and the equipped one now carries Mojang's key"
    );
    assert_eq!(
        mine[0].name.as_deref(),
        Some("New"),
        "the rekey keeps the entry, not just the key"
    );
    assert!(ctx
        .skins_dir
        .join(format!("{}.png", sha(&canonical)))
        .exists());
    assert!(
        !ctx.skins_dir.join(format!("{}.png", sha(&local))).exists(),
        "the local file nobody refers to any more is gone"
    );

    let skins: Vec<_> = view.skins.iter().map(|card| &card.skin).collect();
    assert_eq!(
        skins
            .iter()
            .filter(|card| card.active)
            .map(|card| (card.source, card.texture_key.clone()))
            .collect::<Vec<_>>(),
        vec![(SkinSource::Saved, sha(&canonical))]
    );
    assert!(skins.iter().all(|card| card.source != SkinSource::External));
    assert!(
        view.skins
            .iter()
            .filter(|card| card.skin.source == SkinSource::Saved)
            .all(|card| card.texture_url.starts_with("data:image/png;base64,")),
        "saved skins are drawn from the local file, network or not"
    );
    assert!(view.profile_error.is_none());
    assert!(
        mojang.cached_profile().is_some(),
        "the answer is remembered for library-only operations"
    );

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_refused_upload_does_not_rekey_anything() {
    let root = unique_root("refused");
    let connection = open_test_database(&root);
    let ctx = context(&root);
    let local = fake_png(64, 64, b"local");
    add_to_library(
        &connection,
        &ctx.skins_dir,
        PLAYER,
        &local,
        SkinVariant::Classic,
        None,
    )
    .unwrap();
    drop(connection);

    let mut mojang = FakeMojang::wearing(profile_wearing(KAI_SLIM, SkinVariant::Slim));
    mojang.upload_error = Some(classify_mojang_error(429, Some("40"), ""));

    let error = equip(&ctx, &mojang, &sha(&local), SkinVariant::Classic)
        .await
        .expect_err("the 429 reaches the caller");
    assert_eq!(error.kind, SkinErrorKind::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(40));

    let connection = Connection::open(&ctx.db_path).unwrap();
    assert_eq!(
        saved_skins_for(&connection, PLAYER).unwrap(),
        vec![saved(PLAYER, &sha(&local), SkinVariant::Classic)],
        "the default being worn is not saved, and the entry keeps its key"
    );
    assert!(ctx.skins_dir.join(format!("{}.png", sha(&local))).exists());

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn equipping_a_default_uploads_the_embedded_texture() {
    let root = unique_root("default");
    open_test_database(&root);
    let ctx = context(&root);
    let steve = DEFAULT_SKINS
        .iter()
        .find(|skin| skin.name == "Steve" && skin.variant == SkinVariant::Classic)
        .unwrap();
    let mojang = FakeMojang::wearing(profile_wearing(KAI_SLIM, SkinVariant::Slim));

    let view = equip(&ctx, &mojang, steve.texture_key, SkinVariant::Classic)
        .await
        .unwrap();

    assert_eq!(
        *mojang.uploads.lock(),
        vec![(steve.png.to_vec(), SkinVariant::Classic)]
    );
    let active: Vec<_> = view.skins.iter().filter(|card| card.skin.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].skin.source, SkinSource::Default);
    assert_eq!(active[0].skin.name.as_deref(), Some("Steve"));

    fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn resetting_keeps_the_outgoing_skin_and_shows_the_default_mojang_picks() {
    let root = unique_root("reset");
    open_test_database(&root);
    let ctx = context(&root);
    let outgoing = fake_png(64, 64, b"custom");
    let mojang = FakeMojang::wearing(profile_wearing(&sha(&outgoing), SkinVariant::Classic))
        .serving(outgoing.clone());

    let view = reset(&ctx, &mojang).await.expect("the reset succeeds");

    assert_eq!(*mojang.resets.lock(), 1);
    let connection = Connection::open(&ctx.db_path).unwrap();
    assert_eq!(
        saved_skins_for(&connection, PLAYER).unwrap(),
        vec![saved(PLAYER, &sha(&outgoing), SkinVariant::Classic)]
    );
    let active: Vec<_> = view
        .skins
        .iter()
        .filter(|card| card.skin.active)
        .map(|card| &card.skin)
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(
        (active[0].source, active[0].name.as_deref()),
        (SkinSource::Default, Some("Kai"))
    );

    drop(connection);
    fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn the_view_lists_every_owned_cape_and_which_one_is_shown() {
    let root = unique_root("capes");
    open_test_database(&root);
    let ctx = context(&root);
    let mojang = FakeMojang::wearing(profile_wearing(KAI_SLIM, SkinVariant::Slim));

    let shown = set_cape(&ctx, &mojang, Some("cape-pan")).await.unwrap();
    assert_eq!(
        shown
            .capes
            .iter()
            .map(|cape| (cape.id.as_str(), cape.active))
            .collect::<Vec<_>>(),
        vec![("cape-pan", true), ("cape-migrator", false)]
    );
    assert_eq!(
        shown.capes[0].texture_url,
        "https://textures.minecraft.net/texture/28de4a81688ad18b49e735a273e086c18f1e3966956123ccb574034c06f5d336",
        "https: the http URL Mojang writes is the same file, and CORS allows it"
    );

    let hidden = set_cape(&ctx, &mojang, None).await.unwrap();
    assert!(hidden.capes.iter().all(|cape| !cape.active));

    fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn keys_from_the_frontend_never_leave_the_skins_folder() {
    let root = unique_root("traversal");
    open_test_database(&root);
    let ctx = context(&root);
    let mojang = FakeMojang::wearing(profile_wearing(KAI_SLIM, SkinVariant::Slim));

    for key in ["../launcher_data", "../../etc/passwd", "", "ABCDEF"] {
        let error = equip(&ctx, &mojang, key, SkinVariant::Classic)
            .await
            .expect_err("not a texture key");
        assert_eq!(error.kind, SkinErrorKind::InvalidSkin, "{key}");
        let error = remove_saved(&ctx, &mojang, key, SkinVariant::Classic)
            .await
            .expect_err("not a texture key");
        assert_eq!(error.kind, SkinErrorKind::InvalidSkin, "{key}");
    }
    assert_eq!(*mojang.calls.lock(), 0, "nothing reaches Mojang");
    assert!(root.join("launcher_data.db").exists());

    fs::remove_dir_all(&root).unwrap();
}
