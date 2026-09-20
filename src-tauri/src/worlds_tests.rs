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

/// Write a world whose `level.dat` is gzipped NBT, like the real ones.
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
    fs::create_dir_all(&directory).expect("failed to create the world directory");

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

    directory
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
                entry.id.modlist_name.clone(),
                entry.id.instance_name.clone(),
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

    let hidden = vec![WorldId {
        modlist_name: "test2".into(),
        instance_name: "26.3-fabric".into(),
        folder_name: "New World".into(),
    }];

    let entries = list_worlds(&root, &hidden).expect("listing must not fail");

    let flags: Vec<(String, bool)> = entries
        .iter()
        .map(|entry| (entry.id.modlist_name.clone(), entry.hidden))
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
fn hiding_a_world_persists_the_triple_and_unhiding_removes_it() {
    let root = unique_root("hidden-db");
    fs::create_dir_all(&root).expect("failed to create the root");
    let connection = open_test_database(&root);

    let world = WorldId {
        modlist_name: "test2".into(),
        instance_name: "26.3-fabric".into(),
        folder_name: "New World".into(),
    };
    let other = WorldId {
        modlist_name: "Drehmal APOTHEOSIS".into(),
        instance_name: "1.20.1-forge".into(),
        folder_name: "New World".into(),
    };

    assert!(load_hidden_worlds(&connection)
        .expect("a missing row is an empty list")
        .is_empty());

    set_world_hidden(&connection, &world, true).expect("hiding must persist");
    // Hiding twice must not duplicate the entry.
    set_world_hidden(&connection, &world, true).expect("hiding twice must be idempotent");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back"),
        vec![world.clone()]
    );

    // The other `New World` is a different world: un-hiding it must not
    // touch this one.
    set_world_hidden(&connection, &other, false).expect("unhiding an absent world is a no-op");
    assert_eq!(
        load_hidden_worlds(&connection).expect("the row must read back"),
        vec![world.clone()]
    );

    set_world_hidden(&connection, &world, false).expect("unhiding must persist");
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

    let world = WorldId {
        modlist_name: "test2".into(),
        instance_name: "26.3-fabric".into(),
        folder_name: "New World".into(),
    };
    set_world_hidden(&connection, &world, true).expect("hiding must persist");

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
        vec![world]
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
