//! Sharing one world between the instances of a mod list (E4).
//!
//! Four decisions shape everything here, and the order matters:
//!
//! - **The world moves into the mod list** (D94). It does not stay in the
//!   instance that made it with the others linking to it: at the first share it
//!   is moved to `mod-lists/<name>/worlds/<folder>` and **every** instance that
//!   can open it, the original one included, reaches it through a link. The
//!   alternative leaves an owning instance, and deleting that instance takes
//!   the world away from all the others.
//! - **A directory link, never a hard link.** The `region/*.mca` are written in
//!   place, so hard-linking the files inside would make two worlds into one
//!   world with two names, while `level.dat` goes through `level.dat_new` +
//!   rename, which **breaks** the link — chunks shared, metadata diverged.
//!   Report `064` measured that a directory symlink has neither problem: the
//!   rename happens inside the target directory and leaves the link alone.
//! - **On Windows a junction** (D97), through the `junction` crate pinned to
//!   `2.0.0`. A directory symlink there needs Developer Mode or an elevated
//!   process, and the hard-link fallback `instance_mods::create_file_link` uses
//!   does not exist for a directory. **If the junction fails the share is
//!   refused**: [`crate::local_content_packs::link_local_pack`] falls back to a
//!   copy, which is right for a resource pack and catastrophic for a world —
//!   two worlds that diverge at the first play.
//! - **Nothing here overwrites and nothing here deletes a world.** The only
//!   destructive call in this file is the removal of the *source* after a
//!   verified copy, and the only link this file removes is one it can prove
//!   points at the world it was asked about.
//!
//! The move is the dangerous operation, and it is guarded three ways: the
//! world's own `session.lock` is held for the whole of it so a running game
//! cannot be moved out from under itself, `rename` is preferred because it is
//! atomic, and the fallback for a cross-filesystem move copies, **verifies byte
//! for byte**, and only then removes the source.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;
use crate::worlds::{
    list_worlds_with_baseline_backfill, WorldEntry, WorldId, MODLIST_WORLDS_DIR_NAME,
};

const INSTANCES_DIR_NAME: &str = "instances";
const SAVES_DIR_NAME: &str = "saves";
const SESSION_LOCK_FILE_NAME: &str = "session.lock";

/// `mod-lists/<modlist>/worlds/`.
pub fn modlist_worlds_dir(root_dir: &Path, modlist_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join(MODLIST_WORLDS_DIR_NAME)
}

fn instance_saves_dir(root_dir: &Path, modlist_name: &str, instance_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join(INSTANCES_DIR_NAME)
        .join(instance_name)
        .join(SAVES_DIR_NAME)
}

// ── The session lock ─────────────────────────────────────────────────────────

/// A held `session.lock`, released when it is dropped.
///
/// Holding it for the whole move is not the same as checking it first: a game
/// started between the check and the `rename` would be moved out from under
/// itself. While this is alive Minecraft's own `DirectoryLock.create` fails and
/// the world shows as *"Locked by another running instance"*, which is exactly
/// the right answer for the half second the move takes.
pub struct SessionLockGuard {
    _file: File,
}

/// Take the world's `session.lock`, or say who has it.
///
/// `Ok(None)` means there is no lock file at all: Minecraft creates it the
/// first time it opens a world, so its absence is a world that was never
/// opened, not a world that is open. Creating one here would mean writing into
/// a world to find out whether it is safe to touch, which is the wrong trade.
///
/// **The lock must be the same kind the game takes.** Minecraft calls
/// `FileChannel.tryLock`, which on unix is `fcntl(F_SETLK)`; Rust's own
/// `File::try_lock` is `flock`, and the two do not see each other — measured,
/// report `065`: with a `fcntl` lock held, `File::try_lock` still succeeded. So
/// unix goes to `fcntl` directly and Windows keeps `File::try_lock`, which is
/// `LockFileEx` there, the same call the JVM makes.
pub fn try_hold_session_lock(world_dir: &Path) -> Result<Option<SessionLockGuard>> {
    let path = world_dir.join(SESSION_LOCK_FILE_NAME);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()))
        }
    };

    if try_take_exclusive_lock(&file)
        .with_context(|| format!("failed to lock {}", path.display()))?
    {
        Ok(Some(SessionLockGuard { _file: file }))
    } else {
        bail!(
            "the world at {} is open in Minecraft right now",
            world_dir.display()
        )
    }
}

#[cfg(unix)]
fn try_take_exclusive_lock(file: &File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = 0;
    // Zero means "to the end of the file, however long it grows".
    lock.l_len = 0;

    // SAFETY: `fcntl` reads one `flock` through the pointer for the whole call
    // and the file descriptor outlives it.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EAGAIN) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(windows)]
fn try_take_exclusive_lock(file: &File) -> std::io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

// ── The move ─────────────────────────────────────────────────────────────────

/// Move a directory, preferring the atomic call.
///
/// `rename` is one syscall and cannot leave half a world behind, so it is
/// always tried first. It fails with `EXDEV` when the two paths are on
/// different filesystems, and only then does the slow path run: copy the whole
/// tree, **compare it byte for byte with the source**, and remove the source
/// last. In that order nothing can delete bytes that were not written
/// somewhere else first.
fn move_directory(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if is_cross_device(&error) => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    destination.display()
                )
            })
        }
    }

    if let Err(error) = copy_directory(source, destination) {
        // The destination is ours and incomplete: removing it is the only way
        // back, and the source has not been touched.
        let _ = fs::remove_dir_all(destination);
        return Err(error);
    }
    match directories_match(source, destination) {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_dir_all(destination);
            bail!(
                "the copy of {} did not come out identical, nothing was removed",
                source.display()
            );
        }
        Err(error) => {
            let _ = fs::remove_dir_all(destination);
            return Err(error);
        }
    }

    fs::remove_dir_all(source).with_context(|| {
        format!(
            "the world is safe at {} but {} could not be cleared",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(unix)]
fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EXDEV)
}

/// `ERROR_NOT_SAME_DEVICE`, the Windows spelling of `EXDEV`.
#[cfg(windows)]
fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(17)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", source.display()))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = fs::symlink_metadata(&from)
            .with_context(|| format!("failed to read metadata for {}", from.display()))?
            .file_type();

        if file_type.is_dir() {
            copy_directory(&from, &to)?;
        } else if file_type.is_symlink() {
            // A world with a link inside it is not a shape Minecraft writes,
            // and copying it would silently change what the world contains.
            bail!("{} is a link, which a world folder should not contain", from.display());
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("failed to copy {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Whether two directories hold the same names and the same bytes.
///
/// Sizes would be cheaper and would not be a verification: this is the check
/// that stands between a copied world and a removed one.
pub fn directories_match(left: &Path, right: &Path) -> Result<bool> {
    let mut left_names = read_dir_names(left)?;
    let mut right_names = read_dir_names(right)?;
    left_names.sort();
    right_names.sort();
    if left_names != right_names {
        return Ok(false);
    }

    for name in left_names {
        let left_path = left.join(&name);
        let right_path = right.join(&name);
        let left_type = fs::symlink_metadata(&left_path)?.file_type();
        let right_type = fs::symlink_metadata(&right_path)?.file_type();
        if left_type.is_dir() != right_type.is_dir() {
            return Ok(false);
        }
        if left_type.is_dir() {
            if !directories_match(&left_path, &right_path)? {
                return Ok(false);
            }
        } else if fs::read(&left_path)? != fs::read(&right_path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_dir_names(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        names.push(
            entry
                .with_context(|| format!("failed to read an entry of {}", dir.display()))?
                .file_name(),
        );
    }
    Ok(names)
}

// ── The link ─────────────────────────────────────────────────────────────────

/// Point `link` at `world_dir`.
///
/// The existence check is `symlink_metadata` and not `exists`, for the reason
/// `instance_mods::create_file_link` already documents: `exists` **follows**
/// the link, so on a dangling one it answers "nothing here", the removal is
/// skipped, and the creation then fails with `File exists` — measured again for
/// directories in report `064`. A real directory under that name is never
/// removed: it is a world of the user's, and this function refuses instead.
fn link_world_into(world_dir: &Path, link: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(link) {
        if !metadata.file_type().is_symlink() {
            bail!(
                "{} is already there and is not a link to a world",
                link.display()
            );
        }
        remove_directory_link(link)
            .with_context(|| format!("failed to remove the old link at {}", link.display()))?;
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    create_directory_link(world_dir, link).with_context(|| {
        format!(
            "failed to link {} to {}",
            link.display(),
            world_dir.display()
        )
    })
}

#[cfg(unix)]
fn create_directory_link(world_dir: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(world_dir, link).map_err(Into::into)
}

/// A junction, not `symlink_dir`: D97. No copy fallback — a copied world is two
/// worlds.
#[cfg(windows)]
fn create_directory_link(world_dir: &Path, link: &Path) -> Result<()> {
    junction::create(world_dir, link).map_err(Into::into)
}

#[cfg(unix)]
fn remove_directory_link(link: &Path) -> std::io::Result<()> {
    fs::remove_file(link)
}

/// `remove_dir` removes the junction and leaves the target and its contents
/// alone; a recursive removal is what must never be used here.
#[cfg(windows)]
fn remove_directory_link(link: &Path) -> std::io::Result<()> {
    fs::remove_dir(link)
}

// ── Sharing ──────────────────────────────────────────────────────────────────

/// Give `target_instance_name` a way into this world, moving the world into the
/// mod list first if it is still inside an instance.
///
/// `instance_name` is where the world is today: `Some` for a world that has
/// never been shared, `None` for one that already lives in `worlds/`. The
/// result is the world's new id, which is always a shared one.
pub fn share_world_with_instance(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: Option<&str>,
    folder_name: &str,
    target_instance_name: &str,
) -> Result<WorldId> {
    validate_path_component(modlist_name)
        .with_context(|| format!("invalid mod list name '{modlist_name}'"))?;
    validate_path_component(folder_name)
        .with_context(|| format!("invalid world folder name '{folder_name}'"))?;
    validate_path_component(target_instance_name)
        .with_context(|| format!("invalid instance name '{target_instance_name}'"))?;
    if let Some(instance_name) = instance_name {
        validate_path_component(instance_name)
            .with_context(|| format!("invalid instance name '{instance_name}'"))?;
    }

    let world_dir = modlist_worlds_dir(root_dir, modlist_name).join(folder_name);

    if let Some(instance_name) = instance_name {
        let source = instance_saves_dir(root_dir, modlist_name, instance_name).join(folder_name);
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("no world at {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "{} is already a link: the world it points at is the one to share",
                source.display()
            );
        }
        if !metadata.is_dir() {
            bail!("{} is not a world folder", source.display());
        }
        if fs::symlink_metadata(&world_dir).is_ok() {
            bail!(
                "{} already holds a world called '{folder_name}'",
                world_dir.display()
            );
        }

        // Held across the move, not checked before it.
        let _lock = try_hold_session_lock(&source)?;
        fs::create_dir_all(modlist_worlds_dir(root_dir, modlist_name)).with_context(|| {
            format!(
                "failed to create {}",
                modlist_worlds_dir(root_dir, modlist_name).display()
            )
        })?;
        move_directory(&source, &world_dir)?;

        // The instance the world came from is an instance like the others now:
        // it gets a link back, in the place the directory used to be.
        if let Err(error) = link_world_into(&world_dir, &source) {
            // The world is whole in the mod list, so nothing is lost; putting
            // it back is still the state the user asked for least.
            let _ = move_directory(&world_dir, &source);
            return Err(error);
        }
    } else if !world_dir.is_dir() {
        bail!("no shared world at {}", world_dir.display());
    }

    let target = instance_saves_dir(root_dir, modlist_name, target_instance_name).join(folder_name);
    link_world_into(&world_dir, &target)?;

    Ok(WorldId {
        modlist_name: modlist_name.to_string(),
        instance_name: None,
        folder_name: folder_name.to_string(),
    })
}

/// Take one instance's way into a shared world away.
///
/// **Only a link is ever removed**, never a directory: if the name in that
/// instance's `saves/` is a real world, or a link pointing somewhere else, this
/// refuses and touches nothing.
///
/// Taking the last one away is allowed and loses nothing: the world stays in
/// the mod list's `worlds/` folder and stays in the listing with an empty
/// instance list, so it is still visible, still openable from the ⋮ menu, and
/// one share away from being playable again. The alternative — moving it back
/// into the instance that is being dropped — would put a world somewhere the
/// user did not ask for, and refusing outright would leave no way to undo a
/// share.
pub fn unshare_world_from_instance(
    root_dir: &Path,
    modlist_name: &str,
    folder_name: &str,
    instance_name: &str,
) -> Result<()> {
    validate_path_component(modlist_name)
        .with_context(|| format!("invalid mod list name '{modlist_name}'"))?;
    validate_path_component(folder_name)
        .with_context(|| format!("invalid world folder name '{folder_name}'"))?;
    validate_path_component(instance_name)
        .with_context(|| format!("invalid instance name '{instance_name}'"))?;

    let world_dir = modlist_worlds_dir(root_dir, modlist_name).join(folder_name);
    let resolved_world = fs::canonicalize(&world_dir)
        .with_context(|| format!("no shared world at {}", world_dir.display()))?;

    let link = instance_saves_dir(root_dir, modlist_name, instance_name).join(folder_name);
    let metadata = fs::symlink_metadata(&link)
        .with_context(|| format!("{} does not hold this world", link.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!(
            "{} is a world folder of its own, not a link to the shared one",
            link.display()
        );
    }
    let resolved_link = fs::canonicalize(&link)
        .with_context(|| format!("failed to resolve {}", link.display()))?;
    if resolved_link != resolved_world {
        bail!(
            "{} points at {}, not at the shared world",
            link.display(),
            resolved_link.display()
        );
    }

    remove_directory_link(&link)
        .with_context(|| format!("failed to remove {}", link.display()))
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// Share a world with one instance, and hand back the listing as it now
/// stands — the same shape `set_world_hidden_command` follows, so the caller
/// never has to guess what the disk looks like afterwards.
#[tauri::command]
pub fn share_world_with_instance_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: Option<String>,
    folder_name: String,
    target_instance_name: String,
) -> Result<Vec<WorldEntry>, String> {
    share_world_with_instance(
        launcher_paths.root_dir(),
        &modlist_name,
        instance_name.as_deref(),
        &folder_name,
        &target_instance_name,
    )
    .map_err(|error| format!("{error:#}"))?;

    listing(&launcher_paths)
}

#[tauri::command]
pub fn unshare_world_from_instance_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    folder_name: String,
    instance_name: String,
) -> Result<Vec<WorldEntry>, String> {
    unshare_world_from_instance(
        launcher_paths.root_dir(),
        &modlist_name,
        &folder_name,
        &instance_name,
    )
    .map_err(|error| format!("{error:#}"))?;

    listing(&launcher_paths)
}

fn listing(launcher_paths: &LauncherPaths) -> Result<Vec<WorldEntry>, String> {
    let connection = rusqlite::Connection::open(launcher_paths.database_path())
        .map_err(|error| error.to_string())?;
    list_worlds_with_baseline_backfill(launcher_paths.root_dir(), &connection)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "shared_worlds_tests.rs"]
mod tests;
