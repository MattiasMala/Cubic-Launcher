use std::env;
use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use serde::Serialize;

use super::*;

use crate::worlds::list_worlds;

/// The environment variable that turns [`holds_a_session_lock_helper`] from a
/// no-op into a second process holding the lock. A `fcntl` lock belongs to the
/// process that took it, so proving that a held lock is seen needs two
/// processes, and re-running this test binary is the one second process every
/// machine that can run the suite already has.
const LOCK_HELPER_ENV: &str = "CUBIC_TEST_HOLD_SESSION_LOCK";

/// Where the helper says it has the lock. A file and not a pipe: the test
/// harness writes its own lines to the child's stdout, so a pipe would carry
/// its banner first.
const LOCK_MARKER_ENV: &str = "CUBIC_TEST_SESSION_LOCK_MARKER";

fn unique_root(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    env::temp_dir().join(format!("cubic-shared-worlds-{tag}-{stamp}"))
}

#[derive(Serialize)]
struct TestLevelDat {
    #[serde(rename = "Data")]
    data: TestLevelDatData,
}

#[derive(Serialize)]
struct TestLevelDatData {
    #[serde(rename = "LevelName")]
    level_name: String,
    #[serde(rename = "GameType")]
    game_type: i32,
    #[serde(rename = "LastPlayed")]
    last_played: i64,
}

/// A world small enough to live in a temporary directory and shaped enough to
/// be one: a gzipped NBT `level.dat`, a region file written in place, per-world
/// data from a mod, and the `session.lock` Minecraft leaves behind.
fn write_world(directory: &Path, level_name: &str, last_played: i64) {
    fs::create_dir_all(directory.join("region")).expect("failed to create region/");
    fs::create_dir_all(directory.join("serverconfig")).expect("failed to create serverconfig/");

    let nbt = fastnbt::to_bytes(&TestLevelDat {
        data: TestLevelDatData {
            level_name: level_name.to_string(),
            game_type: 0,
            last_played,
        },
    })
    .expect("failed to serialize the test level.dat");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&nbt).expect("failed to gzip the level.dat");
    let gzipped = encoder.finish().expect("failed to finish the gzip stream");

    fs::write(directory.join("level.dat"), gzipped).expect("failed to write the level.dat");
    fs::write(directory.join("region").join("r.0.0.mca"), b"chunks, in place")
        .expect("failed to write the region file");
    fs::write(
        directory.join("serverconfig").join("forge-server.toml"),
        b"[server]\n",
    )
    .expect("failed to write the per-world mod config");
    // Three bytes, `\u{2603}` in UTF-8: what `DirectoryLock` writes, measured
    // in the jar and on the two real worlds (report `064`).
    fs::write(directory.join("session.lock"), "\u{2603}").expect("failed to write the lock file");
}

fn instance_world(root: &Path, modlist: &str, instance: &str, folder: &str) -> PathBuf {
    root.join("mod-lists")
        .join(modlist)
        .join("instances")
        .join(instance)
        .join("saves")
        .join(folder)
}

/// A world of the user's, in an instance, plus a second empty instance to
/// share it with.
fn scratch(tag: &str) -> PathBuf {
    let root = unique_root(tag);
    let world = instance_world(&root, "pack", "1.20.1-forge", "Shared");
    write_world(&world, "Shared", 5_000);
    fs::create_dir_all(
        root.join("mod-lists")
            .join("pack")
            .join("instances")
            .join("1.20.1-fabric")
            .join("saves"),
    )
    .expect("failed to create the second instance");
    root
}

/// The move is the operation that touches bytes that do not come back. It must
/// leave the world identical — `level.dat` included — and it must leave the
/// instance holding a link to it, not a copy and not a hole.
#[test]
fn sharing_moves_the_world_into_the_mod_list_and_links_it_back() {
    let root = scratch("move");
    let source = instance_world(&root, "pack", "1.20.1-forge", "Shared");

    // A copy of the world as it was, to compare against afterwards. It is not
    // the world being moved: it sits outside the launcher root.
    let before = root.join("before");
    copy_directory(&source, &before).expect("failed to take the reference copy");

    let id = share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("sharing must succeed");

    assert_eq!(id.instance_name, None, "a shared world belongs to the mod list");
    // D98: the folder takes the shared name on the way into the mod list.
    assert_eq!(id.folder_name, "Shared shared");
    let moved = modlist_worlds_dir(&root, "pack").join("Shared shared");
    assert!(
        directories_match(&before, &moved).expect("the comparison must run"),
        "every byte of the world must survive the move"
    );

    // The instance it came from now reaches it through a link, and the name
    // the directory used to have is free again.
    assert!(!instance_world(&root, "pack", "1.20.1-forge", "Shared").exists());
    let origin_link = instance_world(&root, "pack", "1.20.1-forge", "Shared shared");
    let link = fs::symlink_metadata(&origin_link).expect("the origin must keep a way in");
    assert!(link.file_type().is_symlink(), "the origin keeps a link, not the world");
    assert_eq!(
        fs::canonicalize(&origin_link).unwrap(),
        fs::canonicalize(&moved).unwrap()
    );
    let target = instance_world(&root, "pack", "1.20.1-fabric", "Shared shared");
    assert!(fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::canonicalize(&target).unwrap(),
        fs::canonicalize(&moved).unwrap()
    );

    // And the listing reports one world, reachable from both.
    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id.instance_name, None);
    let instances: Vec<&str> = entries[0]
        .instances
        .iter()
        .map(|link| link.instance_name.as_str())
        .collect();
    assert_eq!(instances, vec!["1.20.1-fabric", "1.20.1-forge"]);

    let _ = fs::remove_dir_all(&root);
}

/// The guard that protects a running game. A `session.lock` held by another
/// process means Minecraft has the world open, and moving it then would pull
/// the directory out from under a live save.
#[test]
fn a_world_open_in_minecraft_is_not_moved() {
    let root = scratch("open-world");
    let source = instance_world(&root, "pack", "1.20.1-forge", "Shared");

    let held_marker = root.join("held-by-the-helper");
    let mut holder = Command::new(env::current_exe().expect("the test binary must have a path"))
        .args([
            "shared_worlds::tests::holds_a_session_lock_helper",
            "--exact",
            "--test-threads=1",
        ])
        .env(LOCK_HELPER_ENV, source.join("session.lock"))
        .env(LOCK_MARKER_ENV, &held_marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the helper process must start");

    // The helper touches a file once it holds the lock. A marker beats a pipe
    // here: the test harness writes its own lines to the child's stdout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !held_marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the helper never took the lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let refused =
        share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
            .expect_err("a world open in the game must not be moved");
    assert!(
        format!("{refused:#}").contains("open in Minecraft"),
        "the refusal must name the reason: {refused:#}"
    );

    // Nothing moved, nothing was linked.
    assert!(
        fs::symlink_metadata(&source).unwrap().file_type().is_dir(),
        "the world must still be the instance's own directory"
    );
    assert!(!modlist_worlds_dir(&root, "pack").join("Shared").exists());
    assert!(!instance_world(&root, "pack", "1.20.1-fabric", "Shared").exists());

    holder.kill().ok();
    holder.wait().ok();

    // With the holder gone the same call goes through, which is what proves
    // the refusal came from the lock and not from anything else.
    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("once the game is gone the share must succeed");

    let _ = fs::remove_dir_all(&root);
}

/// Not a test: the second process the test above needs. Without the
/// environment variable it does nothing at all, so a plain run of the suite
/// costs one no-op.
#[test]
fn holds_a_session_lock_helper() {
    let (Ok(path), Ok(marker)) = (env::var(LOCK_HELPER_ENV), env::var(LOCK_MARKER_ENV)) else {
        return;
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("the lock file must be there");
    assert!(
        take_the_lock_the_game_takes(&file),
        "the helper must get the lock first"
    );
    fs::write(&marker, b"held").expect("the marker must be writable");
    // Held until the parent kills this process.
    std::thread::sleep(std::time::Duration::from_secs(60));
}

/// The lock **Minecraft** takes, written out here instead of borrowed from
/// `shared_worlds`.
///
/// The duplication is the point. `DirectoryLock.create` calls
/// `FileChannel.tryLock`, which on unix is `fcntl(F_SETLK)`; Rust's
/// `File::try_lock` is `flock`, and the two are independent lock spaces —
/// measured, report `065`. If both sides of this test used the production
/// function, a silent return to `File::try_lock` would still pass, because two
/// `flock`s in two processes conflict with each other perfectly well. Taking
/// the game's own lock here means that regression turns the test red.
fn take_the_lock_the_game_takes(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;

    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = 0;
    lock.l_len = 0;

    // SAFETY: the descriptor outlives the call and `fcntl` only reads the
    // `flock` it is handed.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) == 0 }
}

/// Dropping the last instance is allowed, and the world does not disappear
/// with it: it stays in the mod list and stays in the listing, with nothing
/// able to open it until it is shared again.
#[test]
fn dropping_every_instance_leaves_the_world_in_the_mod_list_and_in_the_listing() {
    let root = scratch("unshare");
    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("sharing must succeed");

    unshare_world_from_instance(&root, "pack", "Shared shared", "1.20.1-fabric")
        .expect("dropping one instance must succeed");
    unshare_world_from_instance(&root, "pack", "Shared shared", "1.20.1-forge")
        .expect("dropping the last instance must succeed");

    let world = modlist_worlds_dir(&root, "pack").join("Shared shared");
    assert!(world.join("level.dat").is_file(), "the world must still be there");
    assert!(!instance_world(&root, "pack", "1.20.1-forge", "Shared shared").exists());

    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    assert_eq!(entries.len(), 1, "an orphaned world must stay visible");
    assert!(
        entries[0].instances.is_empty(),
        "and must say that nothing can open it"
    );

    // Sharing it again needs no move: it is already the mod list's.
    share_world_with_instance(&root, "pack", None, "Shared shared", "1.20.1-forge")
        .expect("re-sharing must succeed");
    let back = list_worlds(&root, &[]).expect("listing must not fail");
    assert_eq!(back[0].instances.len(), 1);

    let _ = fs::remove_dir_all(&root);
}

/// The two shapes that are not ours to remove. A real directory under the
/// name, and a link pointing at something else, both mean "this is not the
/// world you asked about".
#[test]
fn nothing_that_is_not_our_link_is_ever_removed() {
    let root = scratch("refusals");
    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("sharing must succeed");

    // A world of the user's, in a third instance, under the name the shared
    // world carries.
    let his_own = instance_world(&root, "pack", "1.20.1-neoforge", "Shared shared");
    write_world(&his_own, "His Own", 1_000);
    let his_own_backup = root.join("his-own-before");
    copy_directory(&his_own, &his_own_backup).expect("failed to take the reference copy");

    let refused = unshare_world_from_instance(&root, "pack", "Shared shared", "1.20.1-neoforge")
        .expect_err("a real world must not be unshared");
    assert!(format!("{refused:#}").contains("not a link"));
    assert!(his_own.join("level.dat").is_file(), "and must still be there");

    // Sharing into that instance does not replace it either. Since D98 the
    // share no longer refuses — it takes the next free name — but the rule it
    // must not soften is this one: the directory is still the user's world,
    // byte for byte, and still a directory.
    share_world_with_instance(&root, "pack", None, "Shared shared", "1.20.1-neoforge")
        .expect("the share goes around the name instead of through it");
    assert!(!fs::symlink_metadata(&his_own).unwrap().file_type().is_symlink());
    assert!(
        directories_match(&his_own_backup, &his_own).expect("the comparison must run"),
        "the world already under that name must not have lost a byte"
    );
    let link = instance_world(&root, "pack", "1.20.1-neoforge", "Shared shared (2)");
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(modlist_worlds_dir(&root, "pack").join("Shared shared")).unwrap()
    );

    let _ = fs::remove_dir_all(&root);
}

/// A link left pointing at nothing is the state `create_file_link` was fixed
/// for: `exists` follows the link and answers "nothing here", the removal is
/// skipped, and the new link then fails with `File exists`. The same shape
/// appears for a directory, so the same check has to be here.
#[test]
fn a_dangling_link_under_the_name_is_replaced_instead_of_failing() {
    let root = scratch("dangling");
    let target = instance_world(&root, "pack", "1.20.1-fabric", "Shared shared");
    std::os::unix::fs::symlink(root.join("gone"), &target).expect("failed to make a dangling link");
    assert!(!target.exists(), "`exists` follows the link and says nothing is there");

    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("a dangling link must not stop the share");

    assert_eq!(
        fs::canonicalize(&target).unwrap(),
        fs::canonicalize(modlist_worlds_dir(&root, "pack").join("Shared shared")).unwrap()
    );

    let _ = fs::remove_dir_all(&root);
}

/// The slow path of the move, exercised directly: a copy that is verified
/// before the source is removed. `rename` covers the normal case, so this is
/// the only way the copy branch is ever run on a machine with one filesystem.
#[test]
fn the_copy_fallback_verifies_before_it_removes() {
    let root = unique_root("copy-verify");
    let source = root.join("source");
    write_world(&source, "Copied", 7_000);
    let destination = root.join("destination");

    copy_directory(&source, &destination).expect("the copy must succeed");
    assert!(directories_match(&source, &destination).expect("the comparison must run"));

    // One byte different anywhere, and the comparison says so: without that,
    // "verify then remove" would be "remove".
    fs::write(destination.join("region").join("r.0.0.mca"), b"chunks, in placX")
        .expect("failed to damage the copy");
    assert!(!directories_match(&source, &destination).expect("the comparison must run"));

    // And a missing file, not only a changed one.
    fs::remove_file(destination.join("level.dat")).expect("failed to remove the level.dat");
    assert!(!directories_match(&source, &destination).expect("the comparison must run"));

    let _ = fs::remove_dir_all(&root);
}

/// The exact JSON phase 2 reads. `instanceName` is **`null`** for a shared
/// world — that null is how the screen tells a world that belongs to the mod
/// list from one that belongs to an instance — and `instances` is the list the
/// Play button turns into a choice (D95) and the ⋮ menu into the "shared with"
/// sign (D96).
#[test]
fn serializes_the_payload_contract_the_next_phase_reads() {
    let root = scratch("payload");
    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("sharing must succeed");

    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    let payload = serde_json::to_value(&entries).expect("the listing must serialize");
    let world = &payload[0];

    assert_eq!(world["modlistName"], "pack");
    assert!(
        world["instanceName"].is_null(),
        "a shared world belongs to no instance: {world}"
    );
    assert_eq!(world["folderName"], "Shared shared");
    assert_eq!(world["levelName"], "Shared");
    assert_eq!(
        world["instances"],
        serde_json::json!([
            { "instanceName": "1.20.1-fabric", "folderName": "Shared shared" },
            { "instanceName": "1.20.1-forge", "folderName": "Shared shared" },
        ])
    );

    let _ = fs::remove_dir_all(&root);
}

/// **The dead end D98 closes.** Both instances already have a world called
/// `New World` — which is what the real disk looks like — so the link cannot
/// take that name in the destination, and until D98 the share had no way out
/// at all.
///
/// Nothing of the user's may be touched to get out of it: the world already
/// sitting in the destination must still be there, byte for byte, with its own
/// `level.dat`.
#[test]
fn a_destination_that_already_has_a_world_of_that_name_is_still_shared_with() {
    let root = unique_root("collision");
    let source = instance_world(&root, "pack", "26.3-fabric", "New World");
    write_world(&source, "New World", 5_000);
    // A different world, of the user's, under the same name in the other
    // instance. Same folder name, same `LevelName`, different bytes.
    let his_own = instance_world(&root, "pack", "26.3-neoforge", "New World");
    write_world(&his_own, "New World", 9_000);
    fs::write(his_own.join("region").join("r.0.0.mca"), b"his own chunks")
        .expect("failed to make the two worlds differ");
    let his_own_backup = root.join("his-own-before");
    copy_directory(&his_own, &his_own_backup).expect("failed to take the reference copy");

    let id = share_world_with_instance(&root, "pack", Some("26.3-fabric"), "New World", "26.3-neoforge")
        .expect("a name already taken in the destination must not be a dead end");

    // The shared world took a name of its own, and the move still happened.
    assert_eq!(id.folder_name, "New World shared");
    assert!(modlist_worlds_dir(&root, "pack")
        .join("New World shared")
        .join("level.dat")
        .is_file());

    // The user's world in the destination is untouched, and is still a real
    // directory rather than a link.
    assert!(
        fs::symlink_metadata(&his_own).unwrap().file_type().is_dir()
            && !fs::symlink_metadata(&his_own).unwrap().file_type().is_symlink(),
        "the world already in the destination must still be a world"
    );
    assert!(
        directories_match(&his_own_backup, &his_own).expect("the comparison must run"),
        "and must not have lost a byte"
    );

    // Both instances can reach the shared world, each under the name its own
    // `saves/` had free.
    let entries = list_worlds(&root, &[]).expect("listing must not fail");
    let shared = entries
        .iter()
        .find(|entry| entry.id.instance_name.is_none())
        .expect("the shared world must be listed");
    let ways_in: Vec<(&str, &str)> = shared
        .instances
        .iter()
        .map(|link| (link.instance_name.as_str(), link.folder_name.as_str()))
        .collect();
    assert_eq!(
        ways_in,
        vec![
            ("26.3-fabric", "New World shared"),
            ("26.3-neoforge", "New World shared"),
        ]
    );
    for (instance, folder) in ways_in {
        let link = instance_world(&root, "pack", instance, folder);
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(
            fs::canonicalize(&link).unwrap(),
            fs::canonicalize(modlist_worlds_dir(&root, "pack").join("New World shared")).unwrap()
        );
    }

    // And the user's own world is still listed next to it.
    assert_eq!(entries.len(), 2);

    let _ = fs::remove_dir_all(&root);
}

/// The second collision, one level down: the disambiguated name is taken too.
/// `New World shared` is a folder a user can perfectly well have made by hand.
#[test]
fn a_disambiguated_name_that_is_also_taken_gets_another_one() {
    let root = unique_root("collision-twice");
    let source = instance_world(&root, "pack", "26.3-fabric", "New World");
    write_world(&source, "New World", 5_000);
    for taken in ["New World", "New World shared"] {
        let his_own = instance_world(&root, "pack", "26.3-neoforge", taken);
        write_world(&his_own, taken, 9_000);
    }

    share_world_with_instance(&root, "pack", Some("26.3-fabric"), "New World", "26.3-neoforge")
        .expect("two names taken must still not be a dead end");

    let link = instance_world(&root, "pack", "26.3-neoforge", "New World shared (2)");
    assert!(
        fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the third name is the one that was free"
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(modlist_worlds_dir(&root, "pack").join("New World shared")).unwrap()
    );
    // Both of the user's worlds are still real directories.
    for taken in ["New World", "New World shared"] {
        let his_own = instance_world(&root, "pack", "26.3-neoforge", taken);
        assert!(!fs::symlink_metadata(&his_own).unwrap().file_type().is_symlink());
        assert!(his_own.join("level.dat").is_file());
    }

    let _ = fs::remove_dir_all(&root);
}

/// Sharing the same world with the same instance twice must reuse the link it
/// already made, not pile up `… shared (2)`, `… shared (3)` beside it.
#[test]
fn sharing_twice_with_the_same_instance_reuses_the_link() {
    let root = scratch("twice");
    share_world_with_instance(&root, "pack", Some("1.20.1-forge"), "Shared", "1.20.1-fabric")
        .expect("the first share must succeed");
    share_world_with_instance(&root, "pack", None, "Shared shared", "1.20.1-fabric")
        .expect("the second share must succeed");

    let saves = instance_world(&root, "pack", "1.20.1-fabric", "Shared shared")
        .parent()
        .unwrap()
        .to_path_buf();
    let names: Vec<String> = fs::read_dir(&saves)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["Shared shared".to_string()]);

    let _ = fs::remove_dir_all(&root);
}
