use std::env;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::Connection;
use serde::Serialize;

use crate::app_shell::{save_global_settings, ShellGlobalSettingsInput};
use crate::database::initialize_database;

use super::*;

fn unique_root(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    env::temp_dir().join(format!("cubic-worlds-{tag}-{stamp}"))
}

#[derive(Serialize)]
struct TestLevelDat {
    #[serde(rename = "Data")]
    data: TestLevelDatData,
}

/// Only the three fields the listing reads, plus one the listing must ignore:
/// a real `level.dat` carries dozens, and a parser that chokes on the ones it
/// does not know would drop every world.
#[derive(Serialize)]
struct TestLevelDatData {
    #[serde(rename = "LevelName")]
    level_name: String,
    #[serde(rename = "GameType")]
    game_type: i32,
    #[serde(rename = "LastPlayed")]
    last_played: i64,
    #[serde(rename = "DataVersion")]
    data_version: i32,
}

fn world_dir(root: &Path, modlist: &str, instance: &str, folder: &str) -> PathBuf {
    root.join("mod-lists")
        .join(modlist)
        .join(INSTANCES_DIR_NAME)
        .join(instance)
        .join(SAVES_DIR_NAME)
        .join(folder)
}

/// Write a world whose `level.dat` is gzipped NBT, like the real ones, at an
/// arbitrary directory. A shared world lives in the mod list's `worlds/`, so
/// the instance triple is no longer the only place one can sit.
fn write_level_dat(directory: &Path, level_name: &str, game_type: i32, last_played: i64) {
    fs::create_dir_all(directory).expect("failed to create the world directory");

    let nbt = fastnbt::to_bytes(&TestLevelDat {
        data: TestLevelDatData {
            level_name: level_name.to_string(),
            game_type,
            last_played,
            data_version: 3465,
        },
    })
    .expect("failed to serialize the test level.dat");

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&nbt).expect("failed to gzip the level.dat");
    let gzipped = encoder.finish().expect("failed to finish the gzip stream");
    fs::write(directory.join(LEVEL_DAT_FILE_NAME), gzipped).expect("failed to write the level.dat");
}

fn write_world(
    root: &Path,
    modlist: &str,
    instance: &str,
    folder: &str,
    level_name: &str,
    game_type: i32,
    last_played: i64,
) -> PathBuf {
    let directory = world_dir(root, modlist, instance, folder);
    write_level_dat(&directory, level_name, game_type, last_played);
    directory
}

/// The id of a world that lives in one instance's `saves/`.
fn instance_id(modlist: &str, instance: &str, folder: &str) -> WorldId {
    WorldId {
        home: WorldHome::Instance {
            modlist_name: modlist.to_string(),
            instance_name: instance.to_string(),
        },
        folder_name: folder.to_string(),
    }
}

fn open_test_database(root: &Path) -> Connection {
    let database_path = root.join("launcher_data.db");
    initialize_database(&database_path).expect("database should initialize");
    Connection::open(&database_path).expect("database should open")
}

fn ids(entries: &[WorldEntry]) -> Vec<(String, String, String)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.id.modlist_name().unwrap_or_default().to_string(),
                entry.id.instance_name().unwrap_or_default().to_string(),
                entry.id.folder_name.clone(),
            )
        })
        .collect()
}

#[test]
fn reads_the_name_mode_and_date_out_of_a_level_dat() {
    let root = unique_root("fields");
    let directory = write_world(
        &root,
        "Drehmal APOTHEOSIS",
        "1.20.1-forge",
        "New World",
        "Renamed In Game",
        2,
        1_789_850_834_552,
    );

    let entries = list_worlds(&root, &[]).expect("listing must not fail");

    assert_eq!(entries.len(), 1);
    let world = &entries[0];
    // The folder is still `New World`: renaming in game rewrites `LevelName`
    // and leaves the directory alone.
    assert_eq!(world.id.folder_name, "New World");
    assert_eq!(world.level_name, "Renamed In Game");
    assert_eq!(world.game_mode, WorldGameMode::Adventure);
    assert_eq!(world.last_played_ms, 1_789_850_834_552);
    assert_eq!(world.icon_path, None);
    assert!(!world.hidden);

    // The icon is the one field that comes from the directory, not the NBT.
    fs::write(directory.join(WORLD_ICON_FILE_NAME), b"not really a png")
        .expect("failed to write the world icon");
    let with_icon = list_worlds(&root, &[]).expect("listing must not fail");
    assert_eq!(
        with_icon[0].icon_path.as_deref(),
        directory.join(WORLD_ICON_FILE_NAME).to_str()
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_unreadable_level_dat_skips_the_world_and_keeps_the_others() {
    let root = unique_root("unreadable");
    write_world(&root, "pack", "1.20.1-forge", "Good", "Good", 0, 2_000);

    // A world from a mod, or from a format this parser does not know: the
    // file is there and is not gzipped NBT.
    let broken = world_dir(&root, "pack", "1.20.1-forge", "Broken");
    fs::create_dir_all(&broken).expect("failed to create the broken world");
    fs::write(broken.join(LEVEL_DAT_FILE_NAME), b"this is not NBT")
        .expect("failed to write the broken level.dat");

    // And a directory under `saves/` with no `level.dat` at all.
    fs::create_dir_all(world_dir(&root, "pack", "1.20.1-forge", "Empty"))
        .expect("failed to create the empty world");

    let entries = list_worlds(&root, &[]).expect("a broken world must not fail the listing");

    assert_eq!(
        ids(&entries),
        vec![(
            "pack".to_string(),
            "1.20.1-forge".to_string(),
            "Good".to_string()
        )]
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn orders_by_last_played_and_not_by_folder_name() {
    let root = unique_root("ordering");
    // Alphabetically `Alpha` comes first; by `LastPlayed` it comes last.
    write_world(&root, "pack", "1.20.1-forge", "Alpha", "Alpha", 0, 1_000);
    write_world(&root, "pack", "1.20.1-forge", "Zulu", "Zulu", 0, 9_000);

    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry.level_name.as_str())
        .collect();
    assert_eq!(names, vec!["Zulu", "Alpha"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn two_worlds_named_new_world_in_different_instances_stay_two_entries() {
    // The real disk, exactly: same folder name, same `LevelName`, two
    // instances in two mod lists.
    let root = unique_root("homonyms");
    write_world(
        &root,
        "Drehmal APOTHEOSIS",
        "1.20.1-forge",
        "New World",
        "New World",
        1,
        1_789_850_834_552,
    );
    write_world(
        &root,
        "test2",
        "26.3-fabric",
        "New World",
        "New World",
        1,
        1_789_851_046_877,
    );

    let entries = list_worlds(&root, &[]).expect("listing must not fail");

    assert_eq!(
        ids(&entries),
        vec![
            (
                "test2".to_string(),
                "26.3-fabric".to_string(),
                "New World".to_string()
            ),
            (
                "Drehmal APOTHEOSIS".to_string(),
                "1.20.1-forge".to_string(),
                "New World".to_string()
            ),
        ]
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_hidden_list_matches_the_triple_and_not_the_folder_name() {
    let root = unique_root("hidden");
    write_world(
        &root,
        "Drehmal APOTHEOSIS",
        "1.20.1-forge",
        "New World",
        "New World",
        1,
        1_000,
    );
    write_world(&root, "test2", "26.3-fabric", "New World", "New World", 1, 2_000);

    let hidden = vec![HiddenWorld {
        id: instance_id("test2", "26.3-fabric", "New World"),
        hidden_at_last_played_ms: Some(2_000),
    }];

    let entries = list_worlds(&root, &hidden).expect("listing must not fail");

    let flags: Vec<(String, bool)> = entries
        .iter()
        .map(|entry| (entry.id.modlist_name().unwrap_or_default().to_string(), entry.hidden))
        .collect();
    assert_eq!(
        flags,
        vec![
            ("test2".to_string(), true),
            ("Drehmal APOTHEOSIS".to_string(), false),
        ]
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_old_hidden_row_hides_today_and_comes_back_after_the_next_play() {
    // A row written before D65: the triple, no baseline. It must not mean
    // "hidden forever" — there is no show-hidden switch to escape through —
    // so the first listing adopts the world's current `LastPlayed` and the
    // world behaves like anything hidden today.
    let root = unique_root("old-hidden-row");
    let directory = write_world(&root, "pack", "1.20.1-forge", "Old World", "Old World", 0, 9_000);
    let connection = open_test_database(&root);
    let old_value = r#"[{"modlistName":"pack","instanceName":"1.20.1-forge","folderName":"Old World"}]"#;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
            [HIDDEN_WORLDS_KEY, old_value],
        )
        .expect("the old-format row should write");

    assert_eq!(
        load_hidden_worlds(&connection).expect("the old-format row should deserialize"),
        vec![HiddenWorld {
            id: instance_id("pack", "1.20.1-forge", "Old World"),
            hidden_at_last_played_ms: None,
        }]
    );

    let entries = list_worlds_with_baseline_backfill(&root, &connection)
        .expect("listing must not fail");
    assert!(entries[0].hidden, "the world stays where the user left it");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back"),
        vec![HiddenWorld {
            id: instance_id("pack", "1.20.1-forge", "Old World"),
            hidden_at_last_played_ms: Some(9_000),
        }],
        "the missing baseline is adopted from the world itself"
    );

    // He plays it again: it comes back on its own.
    drop(directory);
    write_world(&root, "pack", "1.20.1-forge", "Old World", "Old World", 0, 9_500);
    let after_play = list_worlds_with_baseline_backfill(&root, &connection)
        .expect("listing must not fail");
    assert!(!after_play[0].hidden);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_backfill_leaves_a_hidden_world_that_is_no_longer_on_disk_alone() {
    // Nothing to adopt: the row keeps its empty baseline instead of being
    // dropped or given a made-up one.
    let root = unique_root("old-hidden-gone");
    fs::create_dir_all(&root).expect("failed to create the root");
    let connection = open_test_database(&root);
    let old_value = r#"[{"modlistName":"pack","instanceName":"1.20.1-forge","folderName":"Deleted"}]"#;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
            [HIDDEN_WORLDS_KEY, old_value],
        )
        .expect("the old-format row should write");

    let entries = list_worlds_with_baseline_backfill(&root, &connection)
        .expect("listing must not fail");
    assert!(entries.is_empty());
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back")[0].hidden_at_last_played_ms,
        None
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_world_becomes_visible_when_last_played_advances_past_the_hide_baseline() {
    let root = unique_root("hidden-until-played");
    write_world(&root, "pack", "1.20.1-forge", "World", "World", 0, 1_000);
    let hidden = vec![HiddenWorld {
        id: instance_id("pack", "1.20.1-forge", "World"),
        hidden_at_last_played_ms: Some(1_000),
    }];

    let at_baseline = list_worlds(&root, &hidden).expect("listing must not fail");
    assert!(at_baseline[0].hidden);

    write_world(&root, "pack", "1.20.1-forge", "World", "World", 0, 1_250);
    let after_play = list_worlds(&root, &hidden).expect("listing must not fail");
    assert!(!after_play[0].hidden);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rehiding_after_a_later_play_updates_the_stored_baseline() {
    let root = unique_root("rehide-baseline");
    let directory = write_world(&root, "pack", "1.20.1-forge", "World", "World", 0, 1_000);
    let connection = open_test_database(&root);
    let world = instance_id("pack", "1.20.1-forge", "World");
    let level_dat_path = directory.join(LEVEL_DAT_FILE_NAME);

    let first_baseline = read_level_dat(&level_dat_path).and_then(|level| level.last_played);
    set_world_hidden(&connection, &world, true, first_baseline)
        .expect("the first hide must persist");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the first baseline must read back"),
        vec![HiddenWorld {
            id: world.clone(),
            hidden_at_last_played_ms: Some(1_000),
        }]
    );

    write_world(&root, "pack", "1.20.1-forge", "World", "World", 0, 1_250);
    let later_baseline = read_level_dat(&level_dat_path).and_then(|level| level.last_played);
    set_world_hidden(&connection, &world, true, later_baseline)
        .expect("re-hiding must persist the later baseline");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the later baseline must read back"),
        vec![HiddenWorld {
            id: world,
            hidden_at_last_played_ms: Some(1_250),
        }]
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hiding_a_world_persists_one_entry_and_unhiding_removes_it() {
    let root = unique_root("hidden-db");
    fs::create_dir_all(&root).expect("failed to create the root");
    let connection = open_test_database(&root);

    let world = instance_id("test2", "26.3-fabric", "New World");
    let other = instance_id("Drehmal APOTHEOSIS", "1.20.1-forge", "New World");

    assert!(load_hidden_worlds(&connection)
        .expect("a missing row is an empty list")
        .is_empty());

    set_world_hidden(&connection, &world, true, Some(2_000)).expect("hiding must persist");
    // Hiding twice must update the same entry rather than duplicate it.
    set_world_hidden(&connection, &world, true, Some(2_000))
        .expect("hiding twice must remain one entry");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back"),
        vec![HiddenWorld {
            id: world.clone(),
            hidden_at_last_played_ms: Some(2_000),
        }]
    );

    // The other `New World` is a different world: un-hiding it must not
    // touch this one.
    set_world_hidden(&connection, &other, false, None)
        .expect("unhiding an absent world is a no-op");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back"),
        vec![HiddenWorld {
            id: world.clone(),
            hidden_at_last_played_ms: Some(2_000),
        }]
    );

    set_world_hidden(&connection, &world, false, None).expect("unhiding must persist");
    assert!(load_hidden_worlds(&connection)
        .expect("the row must read back")
        .is_empty());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn saving_the_settings_form_leaves_the_hidden_worlds_alone() {
    let root = unique_root("settings-save");
    fs::create_dir_all(&root).expect("failed to create the root");
    let connection = open_test_database(&root);

    let world = instance_id("test2", "26.3-fabric", "New World");
    set_world_hidden(&connection, &world, true, Some(1_000)).expect("hiding must persist");

    save_global_settings(
        &connection,
        &ShellGlobalSettingsInput {
            min_ram_mb: 2048,
            max_ram_mb: 4096,
            custom_jvm_args: "-XX:+UseG1GC".into(),
            profiler_enabled: false,
            update_notifications_enabled: true,
            update_notifications_resource_packs: true,
            update_notifications_data_packs: true,
            update_notifications_shaders: true,
            wrapper_command: String::new(),
            java_path_override: String::new(),
        },
    )
    .expect("saving the settings must succeed");

    assert_eq!(
        load_hidden_worlds(&connection).expect("the hidden list must survive a settings save"),
        vec![HiddenWorld {
            id: world,
            hidden_at_last_played_ms: Some(1_000),
        }]
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn quick_play_is_offered_only_when_the_version_declares_it() {
    let root = unique_root("quickplay");
    write_world(&root, "pack", "1.20.1-forge", "New World", "New World", 0, 1_000);
    let instance_root = root
        .join("mod-lists")
        .join("pack")
        .join(INSTANCES_DIR_NAME)
        .join("1.20.1-forge");

    assert_eq!(
        quick_play_arguments(&instance_root, Some("New World"), true),
        vec![
            "--quickPlaySingleplayer".to_string(),
            "New World".to_string()
        ]
    );

    // A client whose manifest does not declare the option: the argument is
    // not passed, the player lands on the menu, and the game still starts.
    assert!(quick_play_arguments(&instance_root, Some("New World"), false).is_empty());

    // No world asked for: an ordinary Play.
    assert!(quick_play_arguments(&instance_root, None, true).is_empty());

    // A world that is not on disk, and a name that tries to leave `saves/`.
    assert!(quick_play_arguments(&instance_root, Some("Deleted"), true).is_empty());
    assert!(quick_play_arguments(&instance_root, Some("../../options.txt"), true).is_empty());

    let _ = fs::remove_dir_all(&root);
}

// ── E4: i mondi condivisi ────────────────────────────────────────────────────

/// A world reached through a directory symlink into **another instance's
/// `saves/`** — a link the launcher never makes, made by hand — is still one
/// world and not two: the scan merges by canonical path.
///
/// **Changed meaning in E4 phase 3.** Until then the test also asserted that
/// the hand-made link counted as a way in (`["1.20.1-fabric", "1.20.1-forge"]`).
/// It no longer does: see `a_hand_made_link_into_another_instances_world_is_not_a_way_in`
/// for why, and for what the removed cross-mod-list filter let through.
#[test]
fn a_link_that_stays_inside_the_mod_lists_directory_is_listed() {
    let root = unique_root("link-inside");
    write_world(&root, "pack", "1.20.1-forge", "Shared", "Shared", 0, 4_000);
    let real = world_dir(&root, "pack", "1.20.1-forge", "Shared");

    let other_saves = root
        .join("mod-lists")
        .join("pack")
        .join(INSTANCES_DIR_NAME)
        .join("1.20.1-fabric")
        .join(SAVES_DIR_NAME);
    fs::create_dir_all(&other_saves).expect("failed to create the second instance");
    std::os::unix::fs::symlink(&real, other_saves.join("Shared")).expect("failed to link");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");

    // One world, not two: the two instances are two ways onto the same bytes.
    assert_eq!(entries.len(), 1, "one world is one card, not one per path to it");
    assert_eq!(entries[0].id, instance_id("pack", "1.20.1-forge", "Shared"));
    let instances: Vec<&str> = entries[0]
        .instances
        .iter()
        .map(|link| link.instance_name.as_str())
        .collect();
    assert_eq!(instances, vec!["1.20.1-forge"]);

    let _ = fs::remove_dir_all(&root);
}

/// The hole the containment closes. `child_directories` refused to follow any
/// link at all, and its docstring said why: *"a link cannot make the listing
/// report a world that lives outside the launcher root"*. Following links
/// without a containment check hands that refusal back, and the recon `064`
/// measured what it costs — the world is listed and launchable but
/// `resolve_world_directory` refuses to open its folder, so the ⋮ menu errors
/// on a card the home drew.
#[test]
fn a_link_that_leaves_the_mod_lists_directory_is_not_listed() {
    let root = unique_root("link-outside");
    write_world(&root, "pack", "1.20.1-forge", "Mine", "Mine", 0, 4_000);

    // A world of the user's, somewhere else entirely.
    let elsewhere = unique_root("link-outside-target");
    write_level_dat(&elsewhere, "Not The Launcher's", 0, 9_000);

    let saves = root
        .join("mod-lists")
        .join("pack")
        .join(INSTANCES_DIR_NAME)
        .join("1.20.1-forge")
        .join(SAVES_DIR_NAME);
    std::os::unix::fs::symlink(&elsewhere, saves.join("Escape")).expect("failed to link");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry.level_name.as_str())
        .collect();
    assert_eq!(names, vec!["Mine"], "a link out of the root is not a world");

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&elsewhere);
}

// ── E4 fase 3: i mondi fuori dalle modlist (D99) ─────────────────────────────

fn saves_of(root: &Path, modlist: &str, instance: &str) -> PathBuf {
    let saves = root
        .join("mod-lists")
        .join(modlist)
        .join(INSTANCES_DIR_NAME)
        .join(instance)
        .join(SAVES_DIR_NAME);
    fs::create_dir_all(&saves).expect("failed to create the saves folder");
    saves
}

/// The positive case of D99: a world in `<root>/worlds/`, linked from two
/// instances **of two different mod lists**, is one card that both can open,
/// and each way in says which mod list it belongs to.
#[test]
fn a_world_shared_across_mod_lists_lists_every_instance_that_links_it() {
    let root = unique_root("across-modlists");
    let shared = root.join(WORLDS_DIR_NAME).join("Shared");
    write_level_dat(&shared, "Shared", 0, 4_000);

    std::os::unix::fs::symlink(&shared, saves_of(&root, "test2", "26.3-fabric").join("Shared"))
        .expect("failed to link");
    std::os::unix::fs::symlink(
        &shared,
        saves_of(&root, "Drehmal APOTHEOSIS", "1.20.1-forge").join("Shared (2)"),
    )
    .expect("failed to link");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].id,
        WorldId {
            home: WorldHome::Shared,
            folder_name: "Shared".into(),
        }
    );
    let ways: Vec<(&str, &str, &str)> = entries[0]
        .instances
        .iter()
        .map(|link| {
            (
                link.modlist_name.as_str(),
                link.instance_name.as_str(),
                link.folder_name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        ways,
        vec![
            ("Drehmal APOTHEOSIS", "1.20.1-forge", "Shared (2)"),
            ("test2", "26.3-fabric", "Shared"),
        ]
    );

    let _ = fs::remove_dir_all(&root);
}

/// **What the cross-mod-list filter used to stop.** It dropped any way in
/// from another mod list, on the argument that an instance name alone could
/// not say which mod list it belonged to. D99 puts the mod list into the way
/// in, so the argument is gone and the filter has to go too — otherwise the
/// positive test above could not pass.
///
/// Removing it lets through one thing the launcher never makes: a hand-made
/// link from one instance into **another instance's own world**. Measured with
/// the filter gone and nothing in its place, that link became a way in, and
/// the card of a world that is not shared offered to launch it from another
/// mod list — with no Shared badge, no "Stop sharing" entry that could work
/// (the unshare only removes links into `<root>/worlds/`), and the D94
/// asymmetry back: delete the owning instance and the other one loses it.
///
/// What holds in its place: **a way in is either the world's own directory
/// or a link into `<root>/worlds/`**. An instance's world has exactly one —
/// itself — and a shared world has one per link.
#[test]
fn a_hand_made_link_into_another_instances_world_is_not_a_way_in() {
    let root = unique_root("hand-made-link");
    write_world(&root, "test2", "26.3-fabric", "Mine", "Mine", 0, 4_000);
    let real = world_dir(&root, "test2", "26.3-fabric", "Mine");
    std::os::unix::fs::symlink(
        &real,
        saves_of(&root, "Drehmal APOTHEOSIS", "1.20.1-forge").join("Borrowed"),
    )
    .expect("failed to link");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");

    assert_eq!(entries.len(), 1, "the link does not make a second world");
    assert_eq!(entries[0].id, instance_id("test2", "26.3-fabric", "Mine"));
    let ways: Vec<(&str, &str)> = entries[0]
        .instances
        .iter()
        .map(|link| (link.modlist_name.as_str(), link.instance_name.as_str()))
        .collect();
    assert_eq!(
        ways,
        vec![("test2", "26.3-fabric")],
        "only the world's own instance can open a world that is not shared"
    );

    let _ = fs::remove_dir_all(&root);
}

/// **The containment, extended and not loosened.** D99 put worlds in a second
/// place, `<root>/worlds/`, and the one-line way to admit it would have been
/// "anything under `<root>`". That admits every other folder the launcher
/// keeps: a link into `cache/`, `skins/` or `java-runtimes/` would become a
/// card, launchable, whose folder `resolve_world_directory` then refuses —
/// the hole `065` saw open and closed, one floor up.
#[test]
fn a_link_into_another_folder_of_the_launcher_is_not_listed() {
    let root = unique_root("link-into-cache");
    write_world(&root, "pack", "1.20.1-forge", "Mine", "Mine", 0, 4_000);

    // Inside the launcher root, but in neither of the two places a world may be.
    let decoy = root.join("cache").join("Decoy");
    write_level_dat(&decoy, "Decoy", 0, 9_000);
    std::os::unix::fs::symlink(&decoy, saves_of(&root, "pack", "1.20.1-forge").join("Decoy"))
        .expect("failed to link");

    // And one level too deep inside the shared folder: a directory of a
    // world is not a world.
    let nested = root.join(WORLDS_DIR_NAME).join("Real").join("DIM1");
    write_level_dat(&nested, "Nested", 0, 8_000);
    std::os::unix::fs::symlink(&nested, saves_of(&root, "pack", "1.20.1-forge").join("Nested"))
        .expect("failed to link");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry.level_name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["Mine"],
        "only the two roots hold worlds, and only at the depth a world sits"
    );

    let _ = fs::remove_dir_all(&root);
}
