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
