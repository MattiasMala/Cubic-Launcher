//! Writing a file so that an interruption never leaves it half written.
//!
//! The bytes go to `<name>.part` in the target's own folder, are flushed to
//! disk, and only then renamed over the target: a process killed, a full disk
//! or a power cut before the rename leaves the previous file as it was. It is
//! what `skins.rs` and `launch_preview_cache.rs` already do for their
//! downloads, and what Minecraft does for `level.dat` (`level.dat_new`).
//!
//! The partial never goes to `env::temp_dir()`: a rename across filesystems
//! fails with `EXDEV`, and `/tmp` is often a filesystem of its own.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Replace `path` with `bytes`, or leave the file that was there untouched.
///
/// Every step is an error, the rename included: `Ok` means the new bytes are
/// under `path`. On an error the partial is removed; if even that fails, the
/// next save truncates it.
pub fn write_atomically(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    write_atomically_with(path, |file| file.write_all(bytes.as_ref()))
}

/// Where the bytes go before the rename: next to the target.
fn partial_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} does not name a file", path.display()),
        )
    })?;
    let mut partial = name.to_os_string();
    partial.push(".part");
    Ok(path.with_file_name(partial))
}

/// [`write_atomically`] with the filling of the partial handed to the caller,
/// so a test can stop it where a kill would.
fn write_atomically_with(
    path: &Path,
    fill: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let partial = partial_path(path)?;
    let result = write_then_rename(&partial, path, fill);
    if result.is_err() {
        // The error that matters is the one above; a partial that cannot be
        // removed is truncated by the next save.
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_then_rename(
    partial: &Path,
    path: &Path,
    fill: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = File::create(partial)?;
    fill(&mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(partial, path)
}

/// Remove the `*.part` files under `root` that nobody has written to for
/// `older_than`, and say how many went.
///
/// A partial is left behind only when its writer was killed (a crash, the
/// launcher closed mid-download, a power cut): the error paths remove their
/// own. A download that is never asked again — another version won, the mod
/// left the list — kept its partial in the cache forever
/// (r2-primo-avvio-ambiente-degradato-5). The age keeps clear of a download
/// still running elsewhere, which keeps touching its partial. Symlinks are
/// neither followed nor removed; every error is skipped, because a sweep that
/// cannot finish must not stop what called it.
pub fn remove_stale_partials(root: &Path, older_than: Duration, recursive: bool) -> usize {
    let now = SystemTime::now();
    let mut removed = 0;
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if recursive {
                    folders.push(path);
                }
                continue;
            }
            let is_partial = metadata.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "part");
            let is_stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= older_than);
            if is_partial && is_stale && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::ModList;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A scratch root on the same disk as the build, not in `env::temp_dir()`:
    /// here `/tmp` is a tmpfs, so a partial put there instead of next to the
    /// target would cross filesystems and the rename would fail with `EXDEV`.
    fn scratch_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("atomic-write-tests")
            .join(format!("{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch root should be created");
        root
    }

    fn mod_list(name: &str) -> ModList {
        ModList {
            modlist_name: name.to_string(),
            author: "Mattias".to_string(),
            description: "a mod list that must survive a crash".to_string(),
            rules: Vec::new(),
        }
    }

    fn interrupted() -> io::Error {
        io::Error::new(io::ErrorKind::Interrupted, "the process died here")
    }

    #[test]
    fn a_save_cut_short_leaves_the_previous_file_byte_for_byte_and_readable() {
        let root = scratch_root("cut-short");
        let rules = root.join("rules.json");
        mod_list("Before")
            .write_to_file(&rules)
            .expect("first save");
        let before = fs::read(&rules).expect("the first save is on disk");

        let after = serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 4,
            "modlist_name": "After",
            "author": "Mattias",
            "description": "the save that never finished",
            "rules": []
        }))
        .expect("serializes");
        let result = write_atomically_with(&rules, |file| {
            file.write_all(&after[..after.len() / 2])?;
            Err(interrupted())
        });

        assert!(
            result.is_err(),
            "an interrupted save must not report success"
        );
        assert_eq!(
            fs::read(&rules).expect("the file is still there"),
            before,
            "byte for byte what the last good save wrote"
        );
        let read_back = ModList::read_from_file(&rules).expect("the mod list still reads");
        assert_eq!(read_back.modlist_name, "Before");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_save_that_goes_through_holds_the_new_bytes_and_leaves_no_partial() {
        let root = scratch_root("goes-through");
        let target = root.join("content.json");
        fs::write(&target, b"old bytes").expect("fixture");

        write_atomically(&target, b"new bytes").expect("the save succeeds");

        assert_eq!(fs::read(&target).expect("target"), b"new bytes");
        assert_eq!(
            fs::read_dir(&root).expect("root").count(),
            1,
            "nothing but the target is left in the folder"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// r2-primo-avvio-ambiente-degradato-5: i `.part` di un download ucciso
    /// se ne vanno; quelli ancora scritti da poco, i file veri e quelli che
    /// solo finiscono per «part» restano.
    #[test]
    fn stale_partials_are_swept_and_everything_else_stays() {
        let root = scratch_root("stale-partials");
        let nested = root.join("libraries").join("org").join("lwjgl");
        fs::create_dir_all(&nested).expect("nested folder");
        let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        let stale = |path: &Path| {
            fs::write(path, b"half").expect("partial");
            File::options()
                .write(true)
                .open(path)
                .expect("open")
                .set_modified(old)
                .expect("age the partial");
        };
        stale(&root.join("sodium.jar.part"));
        stale(&nested.join("lwjgl.jar.part"));
        let old_real_file = root.join("counterpart");
        stale(&old_real_file);
        fs::write(root.join("running.jar.part"), b"being written").expect("fresh partial");
        fs::write(root.join("sodium.jar"), b"whole").expect("real file");

        // Not recursive: only the top folder.
        assert_eq!(
            remove_stale_partials(&root, Duration::from_secs(60 * 60), false),
            1
        );
        assert!(nested.join("lwjgl.jar.part").exists());

        assert_eq!(
            remove_stale_partials(&root, Duration::from_secs(60 * 60), true),
            1
        );
        assert!(!root.join("sodium.jar.part").exists());
        assert!(!nested.join("lwjgl.jar.part").exists());
        assert!(root.join("running.jar.part").exists());
        assert!(root.join("sodium.jar").exists());
        assert!(old_real_file.exists());
        // A folder that is not there is nothing to sweep.
        assert_eq!(
            remove_stale_partials(&root.join("missing"), Duration::ZERO, true),
            0
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_did_not_exist_is_created() {
        let root = scratch_root("created");
        let target = root.join("managed-resourcepacks.json");

        write_atomically(&target, b"{}").expect("the save succeeds");

        assert_eq!(fs::read(&target).expect("target"), b"{}");
        assert!(!partial_path(&target).expect("partial").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_partial_sits_in_the_folder_of_the_target_so_the_rename_never_crosses_filesystems() {
        let root = scratch_root("same-folder");
        let target = root.join("version.json");

        let partial = partial_path(&target).expect("partial");
        assert_eq!(partial.parent(), target.parent());
        assert_eq!(
            partial.file_name().and_then(|name| name.to_str()),
            Some("version.json.part")
        );

        // With the scratch root off `/tmp`, a partial that went to
        // `env::temp_dir()` would fail this save with `EXDEV` on a machine
        // where `/tmp` is its own filesystem.
        write_atomically(&target, b"{\"id\":\"1.21.1\"}").expect("the save succeeds");
        assert_eq!(fs::read(&target).expect("target"), b"{\"id\":\"1.21.1\"}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rename_that_fails_is_an_error_and_takes_its_partial_with_it() {
        let root = scratch_root("rename-fails");
        // A directory under the target name: the bytes can be written next to
        // it, the rename over it cannot happen.
        let target = root.join("rules.json");
        fs::create_dir_all(&target).expect("the directory in the way");
        fs::write(target.join("inside.txt"), b"mine").expect("its content");

        let result = write_atomically(&target, b"new bytes");

        assert!(result.is_err(), "a save that did not land must say so");
        assert!(target.is_dir(), "what was there stays");
        assert_eq!(
            fs::read(target.join("inside.txt")).expect("content"),
            b"mine"
        );
        assert!(
            !partial_path(&target).expect("partial").exists(),
            "a failed save cleans up after itself"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_save_after_an_interrupted_one_goes_through() {
        let root = scratch_root("save-after");
        let rules = root.join("rules.json");
        mod_list("First").write_to_file(&rules).expect("first save");

        let result = write_atomically_with(&rules, |file| {
            file.write_all(b"{\"schema_ver")?;
            Err(interrupted())
        });
        assert!(result.is_err());

        mod_list("Second")
            .write_to_file(&rules)
            .expect("the next save is not blocked");
        assert_eq!(
            ModList::read_from_file(&rules).expect("reads").modlist_name,
            "Second"
        );
        assert!(!partial_path(&rules).expect("partial").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_left_by_a_killed_save_does_not_block_the_next_one() {
        let root = scratch_root("killed");
        let rules = root.join("rules.json");
        mod_list("Before the crash")
            .write_to_file(&rules)
            .expect("first save");
        let before = fs::read(&rules).expect("on disk");
        // A process killed between the write and the rename runs no cleanup:
        // this is what it leaves behind.
        fs::write(
            partial_path(&rules).expect("partial"),
            b"{\"schema_version\": 4, \"modl",
        )
        .expect("the leftover partial");

        assert_eq!(fs::read(&rules).expect("target"), before);
        assert_eq!(
            ModList::read_from_file(&rules).expect("reads").modlist_name,
            "Before the crash"
        );

        mod_list("After the crash")
            .write_to_file(&rules)
            .expect("the next save goes through");
        assert_eq!(
            ModList::read_from_file(&rules).expect("reads").modlist_name,
            "After the crash"
        );
        assert_eq!(
            fs::read_dir(&root).expect("root").count(),
            1,
            "the leftover is gone with the save that replaced it"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
