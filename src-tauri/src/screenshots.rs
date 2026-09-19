//! The global screenshot view's backend.
//!
//! One flat listing of every
//! `mod-lists/<modlist>/instances/<instance>/screenshots/` directory, plus the
//! first deliberate deletion of a user file this launcher performs.
//!
//! Two decisions worth stating once, because both are load-bearing:
//!
//! - **The date comes from the filesystem, not from the filename.** Minecraft
//!   names screenshots `YYYY-MM-DD_HH.MM.SS.png`, which is convenient and
//!   wrong to trust: a file renamed by hand, or copied over from another
//!   machine, would scatter the ordering exactly where the user trusts it
//!   most. `modified_ms` is `Metadata::modified`.
//! - **A missing `screenshots/` directory is the normal case**, not an error:
//!   an instance that never ran never had one. It is skipped silently — no
//!   log, no banner.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tauri::State;

use crate::launcher_paths::LauncherPaths;

const INSTANCES_DIR_NAME: &str = "instances";
const SCREENSHOTS_DIR_NAME: &str = "screenshots";

/// Extensions the view lists and the deletion accepts. Minecraft only ever
/// writes `.png`; the other two are here because a user drops files into the
/// folder and the view should not pretend they are invisible.
const SCREENSHOT_EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotEntry {
    /// Absolute path, which is also the identity the delete and the thumbnail
    /// read use.
    pub path: String,
    pub file_name: String,
    pub modlist_name: String,
    pub instance_name: String,
    /// Filesystem mtime in milliseconds since the epoch (negative before it).
    pub modified_ms: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotListing {
    pub entries: Vec<ScreenshotEntry>,
    /// Whether a deletion would reach the system trash. The confirmation text
    /// depends on this: when it is false the dialog must say the deletion is
    /// permanent instead of pretending there is a safety net.
    pub trash_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScreenshotDisposal {
    /// Moved into the freedesktop trash; recoverable from the file manager.
    Trashed,
    /// Unlinked. Only reachable when the caller explicitly allowed it.
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteScreenshotOutcome {
    pub disposal: ScreenshotDisposal,
    /// The screenshot that is gone, as the caller named it.
    pub path: String,
}

// ── Listing ──────────────────────────────────────────────────────────────────

pub fn list_screenshots(root_dir: &Path) -> Result<Vec<ScreenshotEntry>> {
    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();

    let mut entries: Vec<ScreenshotEntry> = Vec::new();
    for modlist_dir in child_directories(&modlists_dir)? {
        let Some(modlist_name) = utf8_file_name(&modlist_dir) else {
            continue;
        };
        for instance_dir in child_directories(&modlist_dir.join(INSTANCES_DIR_NAME))? {
            let Some(instance_name) = utf8_file_name(&instance_dir) else {
                continue;
            };
            collect_screenshots(
                &instance_dir.join(SCREENSHOTS_DIR_NAME),
                &modlist_name,
                &instance_name,
                &mut entries,
            )?;
        }
    }

    // Newest first. The path is the tie-break so that two files written in the
    // same millisecond still come out in a stable order.
    entries.sort_by(|left, right| {
        right
            .modified_ms
            .cmp(&left.modified_ms)
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(entries)
}

/// Directory children of `dir`, sorted. A missing `dir` yields nothing:
/// a mod list with no `instances/` and an instance with no `screenshots/` are
/// both ordinary states.
///
/// Symlinked children are not followed, so a link cannot make the listing
/// report a file that lives outside the launcher root.
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

fn collect_screenshots(
    screenshots_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    entries: &mut Vec<ScreenshotEntry>,
) -> Result<()> {
    let read_dir = match fs::read_dir(screenshots_dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", screenshots_dir.display()))
        }
    };

    for entry in read_dir {
        let entry = entry.with_context(|| {
            format!("failed to read an entry of {}", screenshots_dir.display())
        })?;
        if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if !has_screenshot_extension(&path) {
            continue;
        }
        let (Some(file_name), Some(path_text)) = (utf8_file_name(&path), path.to_str()) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        entries.push(ScreenshotEntry {
            path: path_text.to_string(),
            file_name,
            modlist_name: modlist_name.to_string(),
            instance_name: instance_name.to_string(),
            modified_ms: modified_ms(&metadata),
            size_bytes: metadata.len(),
        });
    }

    Ok(())
}

fn has_screenshot_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            SCREENSHOT_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
        .unwrap_or(false)
}

fn utf8_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

fn modified_ms(metadata: &fs::Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(since_epoch) => i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX),
        Err(before_epoch) => i64::try_from(before_epoch.duration().as_millis())
            .map(|millis| -millis)
            .unwrap_or(i64::MIN),
    }
}

// ── The path a deletion is allowed to touch ──────────────────────────────────

/// Resolve `path` to the screenshot it names, or refuse.
///
/// Accepted only when all of this holds: the path is absolute, spells no `.`
/// or `..`, names a regular file (a symlink is not one), carries a screenshot
/// extension, and its **canonicalised** parent is exactly
/// `<root>/mod-lists/<modlist>/instances/<instance>/screenshots`.
///
/// Canonicalising the parent is what makes the check hold against links: a
/// `screenshots` directory that is a symlink to somewhere else resolves to its
/// real location, which then fails the prefix test. Same shape as
/// `launch_preview_content::content_pack_cache_path` refusing `..`
/// (`launch_preview_content.rs:89-98` via `path_safety::validate_path_component`),
/// one level up: there a filename, here a whole directory position.
fn resolve_screenshot_path(root_dir: &Path, path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    if !candidate.is_absolute() {
        bail!("screenshot path must be absolute: '{path}'");
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        bail!("screenshot path must not contain '.' or '..': '{path}'");
    }
    if !has_screenshot_extension(candidate) {
        bail!("'{path}' is not a screenshot file");
    }

    let file_name = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("screenshot path has no filename: '{path}'"))?;

    // `symlink_metadata` does not follow: a symlink is refused here rather
    // than resolved, so the delete only ever touches a real file that sits in
    // the folder it claims to sit in.
    let metadata = fs::symlink_metadata(candidate)
        .with_context(|| format!("no screenshot at '{path}'"))?;
    if !metadata.is_file() {
        bail!("'{path}' is not a regular file");
    }

    let parent = candidate
        .parent()
        .with_context(|| format!("screenshot path has no parent: '{path}'"))?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve the folder of '{path}'"))?;

    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();
    let modlists_dir = fs::canonicalize(&modlists_dir)
        .with_context(|| format!("failed to resolve {}", modlists_dir.display()))?;

    let relative = parent
        .strip_prefix(&modlists_dir)
        .map_err(|_| anyhow::anyhow!("'{path}' is outside the mod lists directory"))?;

    let segments: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment),
            _ => None,
        })
        .collect();

    let shaped_like_a_screenshots_folder = segments.len() == 4
        && segments[1] == INSTANCES_DIR_NAME
        && segments[3] == SCREENSHOTS_DIR_NAME;
    if !shaped_like_a_screenshots_folder {
        bail!("'{path}' is not inside an instance's screenshots folder");
    }

    Ok(parent.join(file_name))
}

// ── Deleting ─────────────────────────────────────────────────────────────────

pub fn delete_screenshot(
    root_dir: &Path,
    path: &str,
    allow_permanent: bool,
) -> Result<DeleteScreenshotOutcome> {
    delete_screenshot_into_trash(root_dir, path, allow_permanent, home_trash_dir().as_deref())
}

/// The deletion with its trash directory injected, so the tests can exercise
/// all three outcomes without touching the real one.
fn delete_screenshot_into_trash(
    root_dir: &Path,
    path: &str,
    allow_permanent: bool,
    trash_dir: Option<&Path>,
) -> Result<DeleteScreenshotOutcome> {
    let resolved = resolve_screenshot_path(root_dir, path)?;

    if let Some(trash_dir) = trash_dir {
        match trash_into(trash_dir, &resolved) {
            Ok(_) => {
                return Ok(DeleteScreenshotOutcome {
                    disposal: ScreenshotDisposal::Trashed,
                    path: path.to_string(),
                })
            }
            Err(error) if !allow_permanent => {
                return Err(error.context(
                    "the system trash is unavailable and this deletion was not \
                     confirmed as permanent",
                ))
            }
            // Falls through to the permanent deletion the caller already
            // confirmed: the dialog said "permanently" and meant it.
            Err(_) => {}
        }
    } else if !allow_permanent {
        bail!(
            "the system trash is unavailable and this deletion was not confirmed \
             as permanent"
        );
    }

    fs::remove_file(&resolved)
        .with_context(|| format!("failed to delete {}", resolved.display()))?;
    Ok(DeleteScreenshotOutcome {
        disposal: ScreenshotDisposal::Deleted,
        path: path.to_string(),
    })
}

/// Whether a deletion can reach the system trash right now.
///
/// Trash support is the freedesktop home trash, so: Linux, an
/// `XDG_DATA_HOME`/`HOME` to put it under, and the launcher root on the same
/// filesystem as that trash — a trash on another device cannot receive a
/// `rename`, and copying a user's file across a disk behind their back is not
/// what "delete" means.
pub fn trash_available(root_dir: &Path) -> bool {
    let Some(trash_dir) = home_trash_dir() else {
        return false;
    };
    // The trash directory itself may not exist yet on a fresh account; the
    // device that decides the question is the one holding it.
    let probe = if trash_dir.exists() {
        trash_dir
    } else {
        match trash_dir.parent() {
            Some(parent) if parent.exists() => parent.to_path_buf(),
            _ => return false,
        }
    };
    same_device(&probe, root_dir).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn home_trash_dir() -> Option<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            return Some(PathBuf::from(data_home).join("Trash"));
        }
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/Trash"))
}

/// No home trash outside Linux: the freedesktop layout is not what macOS and
/// Windows use, and guessing at their shells from here would be worse than
/// telling the user plainly that the deletion is permanent.
#[cfg(not(target_os = "linux"))]
fn home_trash_dir() -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn same_device(left: &Path, right: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left_dev = fs::metadata(left)
        .with_context(|| format!("failed to stat {}", left.display()))?
        .dev();
    let right_dev = fs::metadata(right)
        .with_context(|| format!("failed to stat {}", right.display()))?
        .dev();
    Ok(left_dev == right_dev)
}

#[cfg(not(unix))]
fn same_device(_left: &Path, _right: &Path) -> Result<bool> {
    Ok(false)
}

/// Move `file` into the freedesktop trash at `trash_dir`
/// (<https://specifications.freedesktop.org/trash-spec/trashspec-1.0.html>):
/// the file goes to `files/<name>`, and `info/<name>.trashinfo` records where
/// it came from so the file manager can put it back.
///
/// The info file is created with `create_new`, which is what reserves the
/// name: another trashing process racing for the same name loses the create
/// and picks the next one instead of overwriting.
fn trash_into(trash_dir: &Path, file: &Path) -> Result<PathBuf> {
    let files_dir = trash_dir.join("files");
    let info_dir = trash_dir.join("info");
    fs::create_dir_all(&files_dir)
        .with_context(|| format!("failed to create {}", files_dir.display()))?;
    fs::create_dir_all(&info_dir)
        .with_context(|| format!("failed to create {}", info_dir.display()))?;

    let source_dir = file
        .parent()
        .with_context(|| format!("{} has no parent directory", file.display()))?;
    if !same_device(&files_dir, source_dir)? {
        bail!(
            "{} is on another filesystem than the trash at {}",
            file.display(),
            trash_dir.display()
        );
    }

    let file_name = file
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no usable filename", file.display()))?;
    let (trashed_name, mut info_file) = reserve_trash_name(&files_dir, &info_dir, file_name)?;

    let write_info = {
        use std::io::Write;
        write!(
            info_file,
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            percent_encode_path(file),
            deletion_date_now()
        )
    };
    let info_path = info_dir.join(format!("{trashed_name}.trashinfo"));
    if let Err(error) = write_info {
        let _ = fs::remove_file(&info_path);
        return Err(error).with_context(|| format!("failed to write {}", info_path.display()));
    }
    drop(info_file);

    let destination = files_dir.join(&trashed_name);
    if let Err(error) = fs::rename(file, &destination) {
        // Nothing moved, so the record must not survive: a `.trashinfo` with
        // no file behind it is rubbish the file manager will show forever.
        let _ = fs::remove_file(&info_path);
        return Err(error).with_context(|| {
            format!(
                "failed to move {} into {}",
                file.display(),
                files_dir.display()
            )
        });
    }

    Ok(destination)
}

/// Claim a free `<name>` in the trash, returning it with the opened (and
/// exclusively created) info file.
fn reserve_trash_name(
    files_dir: &Path,
    info_dir: &Path,
    file_name: &str,
) -> Result<(String, fs::File)> {
    for attempt in 0..1_000u32 {
        let candidate = if attempt == 0 {
            file_name.to_string()
        } else {
            suffixed_name(file_name, attempt)
        };
        if files_dir.join(&candidate).symlink_metadata().is_ok() {
            continue;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(info_dir.join(format!("{candidate}.trashinfo")))
        {
            Ok(info_file) => return Ok((candidate, info_file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to write into {}", info_dir.display()))
            }
        }
    }
    bail!("no free name left in the trash for '{file_name}'")
}

fn suffixed_name(file_name: &str, attempt: u32) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.{attempt}.{extension}"),
        _ => format!("{file_name}.{attempt}"),
    }
}

/// Percent-encode an absolute path for the `Path=` field, per the spec's
/// reference to RFC 2396: unreserved characters and the separator stay, every
/// other byte becomes `%XX`. Bytes, not chars — a filename is not required to
/// be UTF-8.
fn percent_encode_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes: Vec<u8> = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes: Vec<u8> = path.to_string_lossy().into_owned().into_bytes();

    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// `DeletionDate` in the spec's format: local time, no zone suffix.
#[cfg(unix)]
fn deletion_date_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs() as libc::time_t)
        .unwrap_or(0);

    let mut broken_down: libc::tm = unsafe { std::mem::zeroed() };
    let resolved = unsafe { libc::localtime_r(&seconds, &mut broken_down) };
    if resolved.is_null() {
        // Cannot happen short of a broken libc; the trash entry is still
        // valid, the restore date just reads as the epoch.
        return "1970-01-01T00:00:00".to_string();
    }

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        broken_down.tm_year + 1900,
        broken_down.tm_mon + 1,
        broken_down.tm_mday,
        broken_down.tm_hour,
        broken_down.tm_min,
        broken_down.tm_sec
    )
}

#[cfg(not(unix))]
fn deletion_date_now() -> String {
    "1970-01-01T00:00:00".to_string()
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_screenshots_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<ScreenshotListing, String> {
    let root_dir = launcher_paths.root_dir();
    let entries = list_screenshots(root_dir).map_err(|error| error.to_string())?;
    Ok(ScreenshotListing {
        entries,
        trash_available: trash_available(root_dir),
    })
}

/// `allow_permanent` is the confirmation the user actually saw: the dialog
/// only sets it when it said the deletion is permanent. If the trash turns
/// out to be unusable and it is false, the deletion is refused rather than
/// silently promoted to a permanent one.
#[tauri::command]
pub fn delete_screenshot_command(
    launcher_paths: State<'_, LauncherPaths>,
    path: String,
    allow_permanent: bool,
) -> Result<DeleteScreenshotOutcome, String> {
    delete_screenshot(launcher_paths.root_dir(), &path, allow_permanent)
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub fn open_screenshot_folder_command(
    launcher_paths: State<'_, LauncherPaths>,
    path: String,
) -> Result<(), String> {
    let resolved = resolve_screenshot_path(launcher_paths.root_dir(), &path)
        .map_err(|error| format!("{error:#}"))?;
    let folder = resolved
        .parent()
        .ok_or_else(|| format!("'{path}' has no folder"))?
        .to_path_buf();
    open::that(&folder).map_err(|error| format!("failed to open {}: {error}", folder.display()))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::time::Duration;

    use super::*;

    fn unique_root(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("cubic-screenshots-{tag}-{stamp}"))
    }

    fn screenshots_dir(root: &Path, modlist: &str, instance: &str) -> PathBuf {
        root.join("mod-lists")
            .join(modlist)
            .join(INSTANCES_DIR_NAME)
            .join(instance)
            .join(SCREENSHOTS_DIR_NAME)
    }

    /// Write a screenshot with an exact mtime, so the ordering test measures
    /// the filesystem date and not the order of creation.
    fn write_screenshot(root: &Path, modlist: &str, instance: &str, name: &str, mtime_secs: u64) -> PathBuf {
        let directory = screenshots_dir(root, modlist, instance);
        fs::create_dir_all(&directory).expect("failed to create the screenshots directory");
        let path = directory.join(name);
        fs::write(&path, b"not really a png").expect("failed to write the screenshot");
        let handle = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("failed to reopen the screenshot");
        handle
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs)),
            )
            .expect("failed to set the screenshot mtime");
        path
    }

    #[test]
    fn lists_every_instance_and_ignores_instances_without_a_screenshots_folder() {
        let root = unique_root("listing");
        write_screenshot(&root, "Drehmal", "1.20.1-forge", "2026-08-18_22.08.58.png", 1_000);
        write_screenshot(&root, "Drehmal", "1.20.1-forge", "2026-09-19_22.47.09.png", 3_000);
        write_screenshot(&root, "test2", "26.3-fabric", "2026-09-19_22.50.44.png", 4_000);
        // The 1.20.1-fabric case on the real disk: an instance directory that
        // exists and has no screenshots folder at all.
        fs::create_dir_all(
            root.join("mod-lists")
                .join("Drehmal")
                .join(INSTANCES_DIR_NAME)
                .join("1.20.1-fabric")
                .join("mods"),
        )
        .expect("failed to create the instance without screenshots");

        let entries = list_screenshots(&root).expect("listing must not fail");

        let described: Vec<(String, String, String)> = entries
            .iter()
            .map(|entry| {
                (
                    entry.modlist_name.clone(),
                    entry.instance_name.clone(),
                    entry.file_name.clone(),
                )
            })
            .collect();
        assert_eq!(
            described,
            vec![
                ("test2".into(), "26.3-fabric".into(), "2026-09-19_22.50.44.png".into()),
                ("Drehmal".into(), "1.20.1-forge".into(), "2026-09-19_22.47.09.png".into()),
                ("Drehmal".into(), "1.20.1-forge".into(), "2026-08-18_22.08.58.png".into()),
            ]
        );
        assert_eq!(entries[0].modified_ms, 4_000_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn orders_by_filesystem_time_not_by_filename() {
        let root = unique_root("ordering");
        // The filename says this one is the oldest by two years; the mtime
        // says it is the newest. A file renamed by hand must not jump the
        // queue.
        write_screenshot(&root, "pack", "instance", "2024-01-01_00.00.00.png", 9_000);
        write_screenshot(&root, "pack", "instance", "2026-09-19_22.47.09.png", 5_000);

        let entries = list_screenshots(&root).expect("listing must not fail");
        let names: Vec<&str> = entries.iter().map(|entry| entry.file_name.as_str()).collect();
        assert_eq!(names, vec!["2024-01-01_00.00.00.png", "2026-09-19_22.47.09.png"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launcher_root_without_mod_lists_lists_nothing_and_is_not_an_error() {
        let root = unique_root("empty");
        fs::create_dir_all(&root).expect("failed to create the root");

        let entries = list_screenshots(&root).expect("a missing mod-lists directory is normal");
        assert!(entries.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_every_path_that_leaves_the_screenshots_folder() {
        let root = unique_root("refusal");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);
        let instance_dir = screenshot
            .parent()
            .and_then(|parent| parent.parent())
            .expect("the instance directory must exist")
            .to_path_buf();

        let options_txt = instance_dir.join("options.txt.png");
        fs::write(&options_txt, b"a file that is not in screenshots/")
            .expect("failed to write the decoy");
        let outside = env::temp_dir().join(format!(
            "cubic-screenshots-outside-{}.png",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before unix epoch")
                .as_nanos()
        ));
        fs::write(&outside, b"a file outside the launcher root").expect("failed to write the decoy");

        let traversal = format!(
            "{}/../../options.txt.png",
            screenshot.parent().expect("parent").display()
        );
        let subfolder = screenshot
            .parent()
            .expect("parent")
            .join("nested")
            .join("shot.png");
        fs::create_dir_all(subfolder.parent().expect("parent")).expect("failed to nest");
        fs::write(&subfolder, b"one level too deep").expect("failed to write the nested decoy");

        for rejected in [
            traversal,
            options_txt.display().to_string(),
            outside.display().to_string(),
            subfolder.display().to_string(),
            screenshot.parent().expect("parent").display().to_string(),
            "mod-lists/pack/instances/instance/screenshots/shot.png".to_string(),
        ] {
            let error = delete_screenshot_into_trash(&root, &rejected, true, None)
                .expect_err(&format!("'{rejected}' must be refused"));
            assert!(
                error.to_string().contains("screenshot")
                    || error.to_string().contains("mod lists")
                    || error.to_string().contains("regular file"),
                "unexpected refusal for '{rejected}': {error}"
            );
        }

        // Nothing was deleted by any of the refusals.
        assert!(screenshot.exists());
        assert!(options_txt.exists());
        assert!(outside.exists());

        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_screenshots_folder_that_is_a_symlink_out_of_the_root() {
        let root = unique_root("symlink");
        let elsewhere = unique_root("symlink-target");
        fs::create_dir_all(&elsewhere).expect("failed to create the link target");
        let victim = elsewhere.join("secret.png");
        fs::write(&victim, b"not the launcher's file").expect("failed to write the victim");

        let instance_dir = root
            .join("mod-lists")
            .join("pack")
            .join(INSTANCES_DIR_NAME)
            .join("instance");
        fs::create_dir_all(&instance_dir).expect("failed to create the instance");
        std::os::unix::fs::symlink(&elsewhere, instance_dir.join(SCREENSHOTS_DIR_NAME))
            .expect("failed to link");

        let through_the_link = instance_dir
            .join(SCREENSHOTS_DIR_NAME)
            .join("secret.png")
            .display()
            .to_string();
        let error = delete_screenshot_into_trash(&root, &through_the_link, true, None)
            .expect_err("a linked screenshots folder must be refused");
        assert!(
            error.to_string().contains("outside the mod lists directory"),
            "unexpected refusal: {error}"
        );
        assert!(victim.exists(), "the linked-to file must survive");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn deletion_moves_the_file_into_the_trash_and_records_where_it_came_from() {
        let root = unique_root("trash");
        let trash = unique_root("trash-dir");
        let screenshot = write_screenshot(&root, "pack", "instance", "2026-09-19_22.47.09.png", 1_000);

        let outcome = delete_screenshot_into_trash(
            &root,
            &screenshot.display().to_string(),
            false,
            Some(&trash),
        )
        .expect("the deletion must succeed");

        assert_eq!(outcome.disposal, ScreenshotDisposal::Trashed);
        assert!(!screenshot.exists(), "the screenshot must leave its folder");
        let trashed = trash.join("files").join("2026-09-19_22.47.09.png");
        assert_eq!(
            fs::read(&trashed).expect("the file must be in the trash"),
            b"not really a png"
        );
        let info = fs::read_to_string(
            trash
                .join("info")
                .join("2026-09-19_22.47.09.png.trashinfo"),
        )
        .expect("the trash record must exist");
        assert!(info.starts_with("[Trash Info]\n"), "unexpected record: {info}");
        assert!(
            info.contains(&format!("Path={}\n", percent_encode_path(&screenshot))),
            "the record must point back at the original path: {info}"
        );
        assert!(info.contains("DeletionDate=20"), "unexpected record: {info}");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash);
    }

    #[test]
    fn a_name_already_in_the_trash_does_not_overwrite_the_earlier_file() {
        let root = unique_root("trash-collision");
        let trash = unique_root("trash-collision-dir");
        let first = write_screenshot(&root, "pack", "one", "2026-09-19_22.47.09.png", 1_000);
        fs::write(&first, b"the first one").expect("failed to rewrite the first screenshot");
        let second = write_screenshot(&root, "pack", "two", "2026-09-19_22.47.09.png", 2_000);
        fs::write(&second, b"the second one").expect("failed to rewrite the second screenshot");

        delete_screenshot_into_trash(&root, &first.display().to_string(), false, Some(&trash))
            .expect("the first deletion must succeed");
        delete_screenshot_into_trash(&root, &second.display().to_string(), false, Some(&trash))
            .expect("the second deletion must succeed");

        let files_dir = trash.join("files");
        assert_eq!(
            fs::read(files_dir.join("2026-09-19_22.47.09.png")).expect("the first file"),
            b"the first one"
        );
        assert_eq!(
            fs::read(files_dir.join("2026-09-19_22.47.09.1.png")).expect("the second file"),
            b"the second one"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash);
    }

    #[test]
    fn without_a_trash_the_deletion_needs_the_permanent_confirmation() {
        let root = unique_root("permanent");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);
        let path = screenshot.display().to_string();

        let error = delete_screenshot_into_trash(&root, &path, false, None)
            .expect_err("a deletion with no trash and no confirmation must be refused");
        assert!(
            error.to_string().contains("not confirmed as permanent"),
            "unexpected refusal: {error}"
        );
        assert!(screenshot.exists(), "the refused deletion must not delete");

        let outcome = delete_screenshot_into_trash(&root, &path, true, None)
            .expect("a confirmed permanent deletion must go through");
        assert_eq!(outcome.disposal, ScreenshotDisposal::Deleted);
        assert!(!screenshot.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
