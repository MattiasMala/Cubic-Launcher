//! Import of a resource pack, data pack or shader the user already has on
//! disk, as a `source: "local"` entry of the mod list.
//!
//! Before this module the only thing in the codebase that ever wrote
//! `source: "local"` was [`crate::modlist_manager::copy_local_jar_from_root`],
//! and it wrote a **mod** rule into `rules.json`: a local pack could not be put
//! into a mod list at all.
//!
//! Where the files live (decision D35): one directory per category inside the
//! mod list, `mod-lists/<name>/resourcepacks/`, `datapacks/`, `shaders/`, the
//! same shape as the existing `local-jars/` for local mods. One copy per mod
//! list, not one per instance: instance directories are per target, and packs
//! are far heavier than jars.
//!
//! A pack is either a `.zip` or an unpacked directory — Minecraft accepts both,
//! and `pack.png`/`pack.mcmeta` sit at the root of either. `pack.png` is read
//! **once, at import**, and written next to the mod list under
//! `.cubic/icons/<category>/`, so drawing a row never reopens an archive and
//! the category JSON the launch path parses stays small.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::content_packs::{
    entry_snapshot, load_content_list, save_content_list, ContentEntry, ContentEntrySnapshot,
};
use crate::launcher_paths::LauncherPaths;
use crate::path_safety::{contained_join, validate_path_component};

/// Upper bound on the `pack.png` we are willing to copy out. Real pack icons on
/// this machine run 6 KB to 149 KB; anything past this is not an icon.
const MAX_PACK_ICON_BYTES: u64 = 1024 * 1024;

/// Upper bound on `pack.mcmeta`, which is a few hundred bytes of JSON.
const MAX_PACK_MCMETA_BYTES: u64 = 256 * 1024;

// ── Where the packs live ────────────────────────────────────────────────────

/// Mod-list-side directory for a content type (decision D35).
///
/// Returns `None` for an unknown content type, which is how this module avoids
/// [`crate::content_packs::filename_for_type`]'s silent `unknown_content.json`
/// fallback: nothing is copied and no JSON is written for a type we do not know.
pub fn modlist_category_dir(content_type: &str) -> Option<&'static str> {
    match content_type {
        "resourcepack" => Some("resourcepacks"),
        "datapack" => Some("datapacks"),
        "shader" => Some("shaders"),
        _ => None,
    }
}

// ── Reading a pack ──────────────────────────────────────────────────────────

/// Shape of the thing the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackShape {
    /// A `.zip` archive.
    Archive,
    /// An unpacked directory.
    Directory,
}

/// What we managed to read out of a pack. Both fields are optional: a pack with
/// no `pack.png` is normal, and `pack.mcmeta` is not required either.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackMetadata {
    /// The bytes of `pack.png`, already checked to be a PNG.
    pub icon_png: Option<Vec<u8>>,
    /// `pack.description` from `pack.mcmeta`, when it is a plain string.
    pub description: Option<String>,
}

/// Read `pack.png` and `pack.mcmeta` from the root of a zip or of a directory.
///
/// Only the root is inspected, because that is where Minecraft looks: a zip
/// that wraps everything in one folder does not load in the game either, so
/// reporting no icon for it is the honest answer, not a bug to paper over.
pub fn read_pack_metadata(source: &Path, shape: PackShape) -> Result<PackMetadata> {
    match shape {
        PackShape::Archive => read_pack_metadata_from_archive(source),
        PackShape::Directory => read_pack_metadata_from_directory(source),
    }
}

fn read_pack_metadata_from_archive(source: &Path) -> Result<PackMetadata> {
    let file =
        fs::File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read zip archive {}", source.display()))?;

    Ok(PackMetadata {
        icon_png: read_archive_member(&mut archive, "pack.png", MAX_PACK_ICON_BYTES)?
            .filter(|bytes| is_png(bytes)),
        description: read_archive_member(&mut archive, "pack.mcmeta", MAX_PACK_MCMETA_BYTES)?
            .as_deref()
            .and_then(description_from_mcmeta),
    })
}

/// Read one root member by exact name. A missing member is `Ok(None)`; a member
/// bigger than `max_bytes` is treated as absent rather than read into memory.
fn read_archive_member<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    let mut member = match archive.by_name(name) {
        Ok(member) => member,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {name} from zip")),
    };
    if member.size() > max_bytes {
        return Ok(None);
    }

    let mut bytes = Vec::with_capacity(member.size() as usize);
    member
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {name} from zip"))?;
    Ok(Some(bytes))
}

fn read_pack_metadata_from_directory(source: &Path) -> Result<PackMetadata> {
    Ok(PackMetadata {
        icon_png: read_directory_member(source, "pack.png", MAX_PACK_ICON_BYTES)?
            .filter(|bytes| is_png(bytes)),
        description: read_directory_member(source, "pack.mcmeta", MAX_PACK_MCMETA_BYTES)?
            .as_deref()
            .and_then(description_from_mcmeta),
    })
}

fn read_directory_member(dir: &Path, name: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    let path = dir.join(name);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Ok(None);
    }

    fs::read(&path)
        .map(Some)
        .with_context(|| format!("failed to read {}", path.display()))
}

/// Do these bytes start with the PNG signature?
///
/// The check mirrors [`crate::modlist_assets`]'s user-picked icon path: a
/// `pack.png` that is not actually a PNG must not reach an `<img>` tag labelled
/// `image/png`.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Where the extracted icon for `<category>/<file_name>` goes, relative to the
/// mod-list directory.
///
/// It lives in the mod list's own `.cubic/` directory, the same convention
/// decision D39 chose for the instance-side manifests: hidden behind a dot, and
/// outside the category folder so it can never be mistaken for a pack. The
/// stored path is relative to the mod list, not to the launcher root, so it
/// survives the mod list being moved.
pub fn icon_relative_path(category_dir: &str, file_name: &str) -> String {
    format!(".cubic/icons/{category_dir}/{file_name}.png")
}

/// Read an entry's icon back as a `data:image/png;base64,…` URL.
///
/// Every failure — path escaping the mod list, missing file, oversized file,
/// bytes that are not a PNG — is "no icon", because an entry without an icon is
/// a normal entry and a broken icon must not break the list.
pub fn read_icon_data_url(modlist_dir: &Path, relative_path: &str) -> Option<String> {
    let path = contained_join(modlist_dir, relative_path).ok()?;
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PACK_ICON_BYTES {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    if !is_png(&bytes) {
        return None;
    }
    Some(format!("data:image/png;base64,{}", BASE64.encode(&bytes)))
}

/// Write the extracted `pack.png` and return its mod-list-relative path.
fn write_icon(
    modlist_dir: &Path,
    category_dir: &str,
    file_name: &str,
    bytes: &[u8],
) -> Result<String> {
    let relative = icon_relative_path(category_dir, file_name);
    let path = contained_join(modlist_dir, &relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(relative)
}

/// `pack.mcmeta` → `pack.description`, but only when it is a plain string.
///
/// The field is also allowed to be a raw JSON text component (an object, or an
/// array of them). Rendering those means implementing Minecraft's text format,
/// so they are ignored: the entry keeps its filename-derived name and no
/// description, which is what it would have had anyway.
pub fn description_from_mcmeta(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let description = value.get("pack")?.get("description")?.as_str()?.trim();
    if description.is_empty() {
        None
    } else {
        Some(description.to_string())
    }
}

// ── Building the entry ──────────────────────────────────────────────────────

/// Display name for a pack stored under `file_name`: the stem for a zip, the
/// directory name as-is for an unpacked pack.
pub fn display_name_for(file_name: &str) -> String {
    let stem = file_name
        .strip_suffix(".zip")
        .or_else(|| {
            file_name
                .len()
                .checked_sub(4)
                .filter(|cut| file_name[*cut..].eq_ignore_ascii_case(".zip"))
                .map(|cut| &file_name[..cut])
        })
        .unwrap_or(file_name);

    if stem.is_empty() {
        file_name.to_string()
    } else {
        stem.to_string()
    }
}

/// Build the entry for a pack that now lives at `<category>/<file_name>`.
///
/// `id` and `file_name` carry the same value here and mean different things:
/// `id` is the identity the rest of the mod list refers to (groups, ordering,
/// removal, version rules), `file_name` is the location the launcher links from.
/// Only one of the two is allowed to change later.
pub fn build_local_entry(
    file_name: &str,
    icon_path: Option<String>,
    description: Option<String>,
) -> ContentEntry {
    ContentEntry {
        id: file_name.to_string(),
        source: "local".to_string(),
        version_rules: vec![],
        name: Some(display_name_for(file_name)),
        file_name: Some(file_name.to_string()),
        icon_path,
        description,
    }
}

// ── Copying ─────────────────────────────────────────────────────────────────

/// Copy a picked pack into the mod list. `dest` must not exist yet.
pub fn copy_pack(source: &Path, dest: &Path, shape: PackShape) -> Result<()> {
    match shape {
        PackShape::Archive => {
            fs::copy(source, dest).with_context(|| {
                format!(
                    "failed to copy '{}' to '{}'",
                    source.display(),
                    dest.display()
                )
            })?;
            Ok(())
        }
        PackShape::Directory => copy_directory_recursive(source, dest),
    }
}

fn copy_directory_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;

    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to inspect entry inside {}", source.display()))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let file_type = fs::symlink_metadata(&from)
            .with_context(|| format!("failed to read metadata for {}", from.display()))?
            .file_type();

        if file_type.is_symlink() {
            // Following a link to a file is what `fs::copy` does anyway, and a
            // pack folder full of symlinked textures is a legitimate layout. A
            // link to a directory could point back up its own tree, so it is
            // refused out loud instead of skipped: skipping would import a pack
            // missing part of itself and say nothing.
            let resolved = fs::metadata(&from)
                .with_context(|| format!("failed to resolve the link at {}", from.display()))?;
            if resolved.is_dir() {
                bail!(
                    "'{}' is a link to a directory; unpack it or zip the pack instead",
                    from.display()
                );
            }
            copy_file(&from, &to)?;
        } else if file_type.is_dir() {
            copy_directory_recursive(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }

    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to)
        .with_context(|| format!("failed to copy '{}' to '{}'", from.display(), to.display()))?;
    Ok(())
}

// ── Installing into an instance ─────────────────────────────────────────────

/// One local pack that has to reach the instance on this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPackInstall {
    /// Name inside the mod-list category directory, and the name the pack gets
    /// inside the instance. This is what goes into the manifest C1 compares
    /// against on the next launch.
    pub file_name: String,
    /// Absolute path of the pack inside the mod list.
    pub source_path: PathBuf,
    /// `false` when the file or folder is not there any more.
    pub present: bool,
}

/// Which `source: "local"` entries of an already-filtered list have to be
/// installed, in list order.
///
/// Pure on purpose: the launch path needs the same decision that the two-pass
/// test needs, and the decision must not depend on an `AppHandle` or on the
/// network. Entries from any other source are skipped here, which is what keeps
/// a local pack out of the Modrinth version lookup by construction.
///
/// A missing file is reported rather than dropped, so the caller can say so in
/// the launch log instead of failing silently. An entry without `file_name`
/// falls back to its `id`: the import writes both with the same value, and an
/// entry written by hand would only have the id.
pub fn plan_local_pack_installs(
    modlist_dir: &Path,
    content_type: &str,
    entries: &[&ContentEntry],
) -> Vec<LocalPackInstall> {
    let Some(category_dir) = modlist_category_dir(content_type) else {
        return Vec::new();
    };
    let category_path = modlist_dir.join(category_dir);

    entries
        .iter()
        .filter(|entry| entry.source == "local")
        .filter_map(|entry| {
            let file_name = entry.file_name.as_deref().unwrap_or(entry.id.as_str());
            validate_path_component(file_name).ok()?;
            let source_path = category_path.join(file_name);
            Some(LocalPackInstall {
                file_name: file_name.to_string(),
                present: source_path.symlink_metadata().is_ok(),
                source_path,
            })
        })
        .collect()
}

/// What happened when a local pack was installed into an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackLinkOutcome {
    /// A symlink, which is the normal case everywhere.
    Linked,
    /// A recursive copy, the fallback for an unpacked pack on a platform that
    /// refuses to create a directory symlink.
    Copied,
    /// Nothing was done: a real directory already sits under that name and the
    /// manifest does not claim it, so it is not ours to delete.
    SkippedForeignDirectory,
}

/// Put a local pack into the instance directory, replacing what the last launch
/// left under that name.
///
/// A file is linked exactly the way a Modrinth pack is
/// ([`crate::instance_mods::create_file_link`]): symlink, falling back to a hard
/// link. A directory is linked with a directory symlink, and where that is
/// refused — Windows without the privilege, which is the same condition the file
/// path already handles — it is **copied**, because a junction would need a new
/// dependency and zipping the pack would change the bytes the user chose and
/// take away the point of keeping it unpacked.
///
/// `directory_is_ours` says whether the previous manifest lists this name, and
/// it gates the one destructive case. A real directory already under that name
/// is either the copy fallback from our own last launch or a folder the user
/// unpacked there; `remove_dir_all` on the second is C1's wipe in a new costume.
/// Without the manifest's word the install is skipped and nothing is touched —
/// the same rule as removal, ownership comes from the manifest, applied on the
/// way in.
pub fn link_local_pack(
    source: &Path,
    target: &Path,
    directory_is_ours: bool,
) -> Result<PackLinkOutcome> {
    let source_is_dir = fs::metadata(source)
        .with_context(|| format!("failed to read {}", source.display()))?
        .is_dir();

    if target_is_real_dir(target) && !directory_is_ours {
        return Ok(PackLinkOutcome::SkippedForeignDirectory);
    }

    remove_existing_target(target)?;

    if !source_is_dir {
        crate::instance_mods::create_file_link(source, target)?;
        return Ok(PackLinkOutcome::Linked);
    }

    match create_directory_symlink(source, target) {
        Ok(()) => Ok(PackLinkOutcome::Linked),
        Err(_) => {
            copy_directory_recursive(source, target)?;
            Ok(PackLinkOutcome::Copied)
        }
    }
}

/// A directory, and not a symlink pointing at one: the only shape whose removal
/// can destroy something the launcher did not create.
fn target_is_real_dir(target: &Path) -> bool {
    fs::symlink_metadata(target)
        .map(|metadata| {
            let file_type = metadata.file_type();
            file_type.is_dir() && !file_type.is_symlink()
        })
        .unwrap_or(false)
}

/// Clear the instance-side name before installing over it. Symmetric with
/// `create_file_link`, which also removes an existing target; the directory
/// cases are the ones it cannot handle.
fn remove_existing_target(target: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(target) else {
        return Ok(());
    };
    let file_type = metadata.file_type();

    let removed = if file_type.is_symlink() {
        remove_symlink(target)
    } else if file_type.is_dir() {
        fs::remove_dir_all(target)
    } else {
        fs::remove_file(target)
    };

    removed.with_context(|| format!("failed to replace {}", target.display()))
}

#[cfg(target_family = "windows")]
fn remove_symlink(target: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        fs::remove_dir(target)
    } else {
        fs::remove_file(target)
    }
}

#[cfg(target_family = "unix")]
fn remove_symlink(target: &Path) -> std::io::Result<()> {
    fs::remove_file(target)
}

#[cfg(target_family = "unix")]
fn create_directory_symlink(source: &Path, target: &Path) -> Result<()> {
    // `symlink` does not care whether the target is a file or a directory, so
    // unix needs no second branch and never reaches the copy fallback.
    std::os::unix::fs::symlink(source, target).with_context(|| {
        format!(
            "failed to link {} to {}",
            target.display(),
            source.display()
        )
    })
}

#[cfg(target_family = "windows")]
fn create_directory_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(source, target).with_context(|| {
        format!(
            "failed to link {} to {}",
            target.display(),
            source.display()
        )
    })
}

// ── Errors the UI has to tell apart ─────────────────────────────────────────

/// Why an import failed, as a value instead of a sentence.
///
/// The caller has to distinguish "that name is taken" from "I cannot read that
/// file" to decide what to show; matching on a message string would break the
/// moment the wording changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentImportErrorCode {
    /// The mod list name is not a single safe path component.
    InvalidModlistName,
    /// No mod list by that name.
    UnknownModlist,
    /// Not one of `resourcepack`, `datapack`, `shader`.
    InvalidContentType,
    /// The picked path has no usable filename, or does not exist.
    InvalidSourcePath,
    /// It exists but is neither a directory nor a `.zip`.
    UnsupportedSourceType,
    /// The category already holds a pack under that name.
    NameCollision,
    /// The source could not be read (unreadable file, corrupt archive).
    ReadFailed,
    /// The copy or the category JSON could not be written.
    WriteFailed,
}

/// A failed import: a code to branch on, and a message to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentImportError {
    pub code: ContentImportErrorCode,
    pub message: String,
}

impl ContentImportError {
    fn new(code: ContentImportErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ── The command ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportLocalContentPackInput {
    pub modlist_name: String,
    pub content_type: String,
    pub source_path: String,
}

#[tauri::command]
pub fn import_local_content_pack_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ImportLocalContentPackInput,
) -> Result<ContentEntrySnapshot, ContentImportError> {
    let entry = import_local_content_pack_from_root(launcher_paths.root_dir(), &input)?;
    let modlist_dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    Ok(entry_snapshot(&entry, &modlist_dir))
}

/// Copy a local pack into the mod list and append its entry, returning the
/// entry that was created.
///
/// Order matters: everything that can be decided is decided before a byte is
/// written, so a rejected import leaves the mod list exactly as it was.
pub fn import_local_content_pack_from_root(
    root_dir: &Path,
    input: &ImportLocalContentPackInput,
) -> Result<ContentEntry, ContentImportError> {
    use ContentImportErrorCode::*;

    validate_path_component(&input.modlist_name)
        .map_err(|error| ContentImportError::new(InvalidModlistName, error.to_string()))?;

    let category_dir = modlist_category_dir(&input.content_type).ok_or_else(|| {
        ContentImportError::new(
            InvalidContentType,
            format!("unknown content type '{}'", input.content_type),
        )
    })?;

    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlist_dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    if !modlist_dir.is_dir() {
        return Err(ContentImportError::new(
            UnknownModlist,
            format!("mod list '{}' does not exist", input.modlist_name),
        ));
    }

    let source = Path::new(&input.source_path);
    let file_name = source
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            ContentImportError::new(
                InvalidSourcePath,
                format!("'{}' has no valid filename", input.source_path),
            )
        })?
        .to_string();
    validate_path_component(&file_name)
        .map_err(|error| ContentImportError::new(InvalidSourcePath, error.to_string()))?;

    let source_metadata = fs::metadata(source).map_err(|error| {
        ContentImportError::new(
            InvalidSourcePath,
            format!("cannot read '{}': {error}", input.source_path),
        )
    })?;
    let shape = if source_metadata.is_dir() {
        PackShape::Directory
    } else if source_metadata.is_file() && file_name.to_ascii_lowercase().ends_with(".zip") {
        PackShape::Archive
    } else {
        return Err(ContentImportError::new(
            UnsupportedSourceType,
            format!("'{file_name}' is neither a folder nor a .zip pack"),
        ));
    };

    let mut list = load_content_list(&modlist_dir, &input.content_type).map_err(|error| {
        ContentImportError::new(
            ReadFailed,
            format!("failed to load the {} list: {error}", input.content_type),
        )
    })?;

    if list
        .entries
        .iter()
        .any(|entry| entry.id == file_name || entry.file_name.as_deref() == Some(&file_name))
    {
        return Err(ContentImportError::new(
            NameCollision,
            format!("'{file_name}' is already in this mod list"),
        ));
    }

    let dest_dir = modlist_dir.join(category_dir);
    let dest = dest_dir.join(&file_name);
    if fs::symlink_metadata(&dest).is_ok() {
        // No entry claims it, but overwriting bytes we did not put there is not
        // ours to decide either.
        return Err(ContentImportError::new(
            NameCollision,
            format!("'{file_name}' already exists in {category_dir}"),
        ));
    }

    let metadata = read_pack_metadata(source, shape).map_err(|error| {
        ContentImportError::new(ReadFailed, format!("cannot read '{file_name}': {error}"))
    })?;

    fs::create_dir_all(&dest_dir).map_err(|error| {
        ContentImportError::new(
            WriteFailed,
            format!("failed to create {}: {error}", dest_dir.display()),
        )
    })?;

    // From here on something exists on disk, and a failure has to undo the
    // bytes we copied: leaving a half-copied pack with no entry would make
    // every retry of the same pack fail as a name collision, an error that
    // lies about what happened and that the user cannot clear from the UI.
    //
    // It undoes the copy and the icon, not the category JSON: `save_content_list`
    // (`content_packs.rs:100`) truncates before writing, so a failure inside it
    // has already lost the previous contents and there is nothing left to
    // restore. That non-atomic write predates this module and is shared by all
    // six content commands, so it is not fixed here.
    let written = (|| -> Result<ContentEntry> {
        copy_pack(source, &dest, shape)?;
        let icon_path = match metadata.icon_png.as_deref() {
            Some(bytes) => Some(write_icon(&modlist_dir, category_dir, &file_name, bytes)?),
            None => None,
        };
        let entry = build_local_entry(&file_name, icon_path, metadata.description.clone());
        list.entries.push(entry.clone());
        save_content_list(&modlist_dir, &list)?;
        Ok(entry)
    })();

    written.map_err(|error| {
        remove_partial_import(&dest, shape, &modlist_dir, category_dir, &file_name);
        ContentImportError::new(WriteFailed, error.to_string())
    })
}

/// Undo a failed import: the copied pack and the extracted icon, if they got
/// as far as existing. Failures here are ignored — we are already returning an
/// error, and the caller gets a worse message if this one replaces it.
fn remove_partial_import(
    dest: &Path,
    shape: PackShape,
    modlist_dir: &Path,
    category_dir: &str,
    file_name: &str,
) {
    match shape {
        PackShape::Archive => {
            fs::remove_file(dest).ok();
        }
        PackShape::Directory => {
            fs::remove_dir_all(dest).ok();
        }
    }
    if let Ok(icon) = contained_join(modlist_dir, &icon_relative_path(category_dir, file_name)) {
        fs::remove_file(icon).ok();
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    use zip::write::FileOptions;

    use crate::content_packs::{load_content_list, ContentEntry};

    use super::{
        build_local_entry, description_from_mcmeta, display_name_for, icon_relative_path,
        import_local_content_pack_from_root, link_local_pack, plan_local_pack_installs,
        read_icon_data_url, ContentImportErrorCode, ImportLocalContentPackInput, PackLinkOutcome,
    };

    /// The eight bytes every PNG starts with, plus a little payload.
    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR-icon";

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-local-packs-test-{timestamp}"))
    }

    fn modlist_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("mod-lists").join(name);
        fs::create_dir_all(&dir).expect("modlist dir should be created");
        dir
    }

    fn write_zip_pack(path: &Path, members: &[(&str, &[u8])]) {
        let file = fs::File::create(path).expect("zip fixture should be creatable");
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in members {
            writer
                .start_file(*name, FileOptions::default())
                .expect("zip member should start");
            writer
                .write_all(bytes)
                .expect("zip member should be written");
        }
        writer.finish().expect("zip fixture should finish");
    }

    fn write_dir_pack(path: &Path, members: &[(&str, &[u8])]) {
        fs::create_dir_all(path).expect("dir fixture should be creatable");
        for (name, bytes) in members {
            let member = path.join(name);
            if let Some(parent) = member.parent() {
                fs::create_dir_all(parent).expect("member parent should be creatable");
            }
            fs::write(member, bytes).expect("member should be written");
        }
    }

    fn import(
        root: &Path,
        modlist: &str,
        content_type: &str,
        source: &Path,
    ) -> Result<ContentEntry, super::ContentImportError> {
        import_local_content_pack_from_root(
            root,
            &ImportLocalContentPackInput {
                modlist_name: modlist.to_string(),
                content_type: content_type.to_string(),
                source_path: source.to_string_lossy().to_string(),
            },
        )
    }

    #[test]
    fn zip_pack_with_pack_png_lands_in_the_category_folder_with_its_icon() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("Faithful 32x.zip");
        write_zip_pack(
            &source,
            &[
                ("pack.png", PNG_BYTES),
                ("assets/minecraft/x.txt", b"texture"),
            ],
        );

        let entry = import(&root, "Sky Pack", "resourcepack", &source).expect("import should work");

        assert_eq!(entry.id, "Faithful 32x.zip");
        assert_eq!(entry.source, "local");
        assert_eq!(entry.name.as_deref(), Some("Faithful 32x"));
        assert_eq!(entry.file_name.as_deref(), Some("Faithful 32x.zip"));
        assert_eq!(
            entry.icon_path.as_deref(),
            Some(icon_relative_path("resourcepacks", "Faithful 32x.zip").as_str()),
        );
        let modlist = root.join("mod-lists").join("Sky Pack");
        assert_eq!(
            fs::read(
                modlist
                    .join(".cubic")
                    .join("icons")
                    .join("resourcepacks")
                    .join("Faithful 32x.zip.png")
            )
            .expect("the extracted icon is a file on disk"),
            PNG_BYTES,
            "pack.png is copied out verbatim, not re-encoded"
        );
        assert_eq!(
            read_icon_data_url(&modlist, entry.icon_path.as_deref().expect("icon path")).as_deref(),
            Some(format!("data:image/png;base64,{}", BASE64.encode(PNG_BYTES)).as_str()),
            "the snapshot side turns it into a data URL"
        );

        let copied = root
            .join("mod-lists")
            .join("Sky Pack")
            .join("resourcepacks")
            .join("Faithful 32x.zip");
        assert!(copied.is_file(), "the zip is copied into the mod list");
        assert_eq!(
            fs::read(&copied).expect("copy should be readable"),
            fs::read(&source).expect("source should be readable"),
            "the copy is byte-identical"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zip_pack_without_pack_png_is_imported_without_an_icon() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("No Icon.zip");
        write_zip_pack(
            &source,
            &[("pack.mcmeta", br#"{"pack":{"pack_format":15}}"#)],
        );

        let entry = import(&root, "Sky Pack", "shader", &source).expect("import should work");

        assert_eq!(entry.icon_path, None, "a pack without pack.png is normal");
        assert_eq!(entry.description, None);
        assert_eq!(entry.name.as_deref(), Some("No Icon"));
        assert!(root
            .join("mod-lists")
            .join("Sky Pack")
            .join("shaders")
            .join("No Icon.zip")
            .is_file());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn directory_pack_with_pack_png_is_copied_whole_with_its_icon() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("Unpacked Pack");
        write_dir_pack(
            &source,
            &[
                ("pack.png", PNG_BYTES),
                ("pack.mcmeta", br#"{"pack":{"description":"Cubic stones"}}"#),
                ("assets/minecraft/textures/stone.png", PNG_BYTES),
            ],
        );

        let entry = import(&root, "Sky Pack", "datapack", &source).expect("import should work");

        assert_eq!(entry.name.as_deref(), Some("Unpacked Pack"));
        assert_eq!(entry.file_name.as_deref(), Some("Unpacked Pack"));
        assert_eq!(
            entry.icon_path.as_deref(),
            Some(icon_relative_path("datapacks", "Unpacked Pack").as_str()),
        );
        assert_eq!(entry.description.as_deref(), Some("Cubic stones"));

        let copied = root
            .join("mod-lists")
            .join("Sky Pack")
            .join("datapacks")
            .join("Unpacked Pack");
        assert!(copied.is_dir());
        assert!(
            copied
                .join("assets")
                .join("minecraft")
                .join("textures")
                .join("stone.png")
                .is_file(),
            "nested files come along, not just the root"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn directory_pack_without_pack_png_is_imported_without_an_icon() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("Bare Folder");
        write_dir_pack(&source, &[("assets/minecraft/x.txt", b"texture")]);

        let entry = import(&root, "Sky Pack", "resourcepack", &source).expect("import should work");

        assert_eq!(entry.icon_path, None);
        assert!(root
            .join("mod-lists")
            .join("Sky Pack")
            .join("resourcepacks")
            .join("Bare Folder")
            .is_dir());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_pack_png_that_is_not_a_png_yields_no_icon() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("Liar.zip");
        write_zip_pack(&source, &[("pack.png", b"GIF89a not really a png")]);

        let entry = import(&root, "Sky Pack", "resourcepack", &source).expect("import should work");

        assert_eq!(
            entry.icon_path, None,
            "bytes that are not a PNG must not be served as image/png"
        );
        assert!(
            !root
                .join("mod-lists")
                .join("Sky Pack")
                .join(".cubic")
                .join("icons")
                .exists(),
            "and nothing is written for them"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mcmeta_description_is_read_only_when_it_is_a_plain_string() {
        assert_eq!(
            description_from_mcmeta(br#"{"pack":{"description":"  Bright stones  "}}"#).as_deref(),
            Some("Bright stones"),
        );
        assert_eq!(
            description_from_mcmeta(br#"{"pack":{"description":[{"text":"Bright"}]}}"#),
            None,
            "a text component is ignored, not stringified"
        );
        assert_eq!(description_from_mcmeta(br#"{"pack":{}}"#), None);
        assert_eq!(description_from_mcmeta(b"{ not json"), None);
        assert_eq!(
            description_from_mcmeta(br#"{"pack":{"description":"  "}}"#),
            None,
            "a blank description is no description"
        );
    }

    #[test]
    fn a_name_already_in_the_list_is_refused_with_its_own_code() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let source = root.join("Twice.zip");
        write_zip_pack(&source, &[("pack.png", PNG_BYTES)]);

        import(&root, "Sky Pack", "resourcepack", &source).expect("first import should work");
        let error =
            import(&root, "Sky Pack", "resourcepack", &source).expect_err("second must be refused");

        assert_eq!(error.code, ContentImportErrorCode::NameCollision);

        let list = load_content_list(&root.join("mod-lists").join("Sky Pack"), "resourcepack")
            .expect("list should load");
        assert_eq!(list.entries.len(), 1, "the refused import adds nothing");

        fs::remove_dir_all(&root).ok();
    }

    /// A folder pack containing a link to a directory is refused by the copy
    /// (it could point back up its own tree). What matters is what is left
    /// behind: nothing, so retrying does not hit a name collision that lies.
    #[test]
    #[cfg(target_family = "unix")]
    fn a_failed_copy_leaves_nothing_behind_and_does_not_poison_a_retry() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let source = root.join("Looping Pack");
        write_dir_pack(&source, &[("pack.png", PNG_BYTES)]);
        std::os::unix::fs::symlink(&source, source.join("self"))
            .expect("directory link fixture should be created");

        let error = import(&root, "Sky Pack", "resourcepack", &source)
            .expect_err("a link to a directory must stop the copy");
        assert_eq!(error.code, ContentImportErrorCode::WriteFailed);

        assert!(
            !modlist.join("resourcepacks").join("Looping Pack").exists(),
            "the half-copied pack is removed"
        );
        assert!(
            !modlist
                .join(".cubic")
                .join("icons")
                .join("resourcepacks")
                .join("Looping Pack.png")
                .exists(),
            "and so is its icon"
        );
        assert!(
            load_content_list(&modlist, "resourcepack")
                .expect("list should load")
                .entries
                .is_empty(),
            "no entry was recorded"
        );

        // The retry fails the same way, not as a collision with a leftover.
        assert_eq!(
            import(&root, "Sky Pack", "resourcepack", &source)
                .expect_err("the retry fails too")
                .code,
            ContentImportErrorCode::WriteFailed
        );

        fs::remove_file(source.join("self")).ok();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_stray_file_of_the_same_name_is_refused_instead_of_overwritten() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let category = modlist.join("resourcepacks");
        fs::create_dir_all(&category).expect("category dir should be created");
        fs::write(category.join("Stray.zip"), b"older bytes").expect("stray should be written");

        let source = root.join("Stray.zip");
        write_zip_pack(&source, &[("pack.png", PNG_BYTES)]);

        let error = import(&root, "Sky Pack", "resourcepack", &source)
            .expect_err("a file we did not record is not ours to overwrite");

        assert_eq!(error.code, ContentImportErrorCode::NameCollision);
        assert_eq!(
            fs::read(category.join("Stray.zip")).expect("stray should still be readable"),
            b"older bytes",
            "the existing bytes survive"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_rejection_has_a_distinguishable_code() {
        let root = unique_test_root();
        modlist_dir(&root, "Sky Pack");
        let zip = root.join("Fine.zip");
        write_zip_pack(&zip, &[("pack.png", PNG_BYTES)]);
        let jar = root.join("mod.jar");
        fs::write(&jar, b"not a pack").expect("jar fixture should be written");

        assert_eq!(
            import(&root, "../escape", "resourcepack", &zip)
                .expect_err("traversal must be refused")
                .code,
            ContentImportErrorCode::InvalidModlistName
        );
        assert_eq!(
            import(&root, "Ghost Pack", "resourcepack", &zip)
                .expect_err("an unknown mod list must be refused")
                .code,
            ContentImportErrorCode::UnknownModlist
        );
        assert_eq!(
            import(&root, "Sky Pack", "plugin", &zip)
                .expect_err("an unknown content type must be refused")
                .code,
            ContentImportErrorCode::InvalidContentType
        );
        assert_eq!(
            import(&root, "Sky Pack", "resourcepack", &root.join("absent.zip"))
                .expect_err("a missing source must be refused")
                .code,
            ContentImportErrorCode::InvalidSourcePath
        );
        assert_eq!(
            import(&root, "Sky Pack", "resourcepack", &jar)
                .expect_err("a .jar is not a pack")
                .code,
            ContentImportErrorCode::UnsupportedSourceType
        );

        assert!(
            !root
                .join("mod-lists")
                .join("Sky Pack")
                .join("resourcepacks")
                .exists(),
            "no rejected import creates the category folder"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// The five-entry `resourcepacks.json` of the real mod list, byte for byte
    /// as `save_content_list` wrote it before the new fields existed.
    const LEGACY_RESOURCEPACKS_JSON: &str = r#"{
  "content_type": "resourcepack",
  "entries": [
    {
      "id": "fresh-animations",
      "source": "modrinth"
    },
    {
      "id": "boss-refreshed",
      "source": "modrinth"
    },
    {
      "id": "enhanced-boss-bars",
      "source": "modrinth"
    },
    {
      "id": "nature-x",
      "source": "modrinth"
    },
    {
      "id": "visual-effects-plus",
      "source": "modrinth"
    }
  ]
}
"#;

    #[test]
    fn the_old_format_still_loads_and_is_rewritten_without_new_keys() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Drehmal APOTHEOSIS");
        fs::write(
            modlist.join("resourcepacks.json"),
            LEGACY_RESOURCEPACKS_JSON,
        )
        .expect("legacy file should be written");

        let list = load_content_list(&modlist, "resourcepack").expect("old format must load");
        assert_eq!(
            list.entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            [
                "fresh-animations",
                "boss-refreshed",
                "enhanced-boss-bars",
                "nature-x",
                "visual-effects-plus"
            ],
            "all five entries survive, in order"
        );
        assert!(list
            .entries
            .iter()
            .all(|e| e.name.is_none() && e.file_name.is_none() && e.icon_path.is_none()));

        // Importing rewrites the file: the Modrinth entries must come out of it
        // exactly as they went in, with no null keys bolted on.
        let source = root.join("Mine.zip");
        write_zip_pack(&source, &[("pack.png", PNG_BYTES)]);
        import(&root, "Drehmal APOTHEOSIS", "resourcepack", &source).expect("import should work");

        let rewritten = fs::read_to_string(modlist.join("resourcepacks.json"))
            .expect("rewritten file should be readable");
        assert!(
            !rewritten.contains("null"),
            "absent optional fields stay absent:\n{rewritten}"
        );
        assert!(
            rewritten.starts_with(LEGACY_RESOURCEPACKS_JSON.trim_end_matches("\n  ]\n}\n")),
            "the five old entries are untouched at the head of the file:\n{rewritten}"
        );

        let reloaded = load_content_list(&modlist, "resourcepack").expect("list should reload");
        assert_eq!(
            reloaded.entries.len(),
            6,
            "a local pack joins the Modrinth ones"
        );
        let local: Vec<_> = reloaded
            .entries
            .iter()
            .filter(|e| e.source == "local")
            .collect();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].file_name.as_deref(), Some("Mine.zip"));
        assert_eq!(
            reloaded
                .entries
                .iter()
                .filter(|e| e.source == "modrinth")
                .count(),
            5,
            "the Modrinth entries keep their source"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn display_name_drops_a_zip_suffix_and_nothing_else() {
        assert_eq!(display_name_for("Faithful 32x.zip"), "Faithful 32x");
        assert_eq!(display_name_for("Faithful 32x.ZIP"), "Faithful 32x");
        assert_eq!(display_name_for("Unpacked Folder"), "Unpacked Folder");
        assert_eq!(
            display_name_for("v1.20.1-pack.zip"),
            "v1.20.1-pack",
            "only the trailing extension goes"
        );
        assert_eq!(display_name_for(".zip"), ".zip", "an empty stem is no name");
    }

    #[test]
    fn a_built_entry_is_local_and_carries_its_location() {
        let entry = build_local_entry(
            "Pack.zip",
            Some(icon_relative_path("resourcepacks", "Pack.zip")),
            Some("desc".to_string()),
        );

        assert_eq!(entry.source, "local");
        assert_eq!(entry.file_name.as_deref(), Some("Pack.zip"));
        assert!(entry.version_rules.is_empty());
    }

    // ── Installing into an instance ──────────────────────────────────────────

    /// One launch: link every local pack of the category and hand the names to
    /// C1's differential sync, exactly as `launch_preview_content.rs` does.
    fn simulate_launch(
        modlist: &Path,
        instance_root: &Path,
        content_type: &str,
        category: &str,
        entries: &[&ContentEntry],
        lookups_complete: bool,
    ) -> (Vec<String>, Vec<String>) {
        let instance_dir = instance_root.join(category);
        fs::create_dir_all(&instance_dir).expect("instance dir should be created");

        let previous = crate::instance_content::read_manifest_files(
            &crate::instance_content::manifest_path(instance_root, category),
        );

        let mut installed = Vec::new();
        for pack in plan_local_pack_installs(modlist, content_type, entries) {
            if !pack.present {
                continue;
            }
            link_local_pack(
                &pack.source_path,
                &instance_dir.join(&pack.file_name),
                previous.iter().any(|name| name == &pack.file_name),
            )
            .expect("a present pack should install");
            installed.push(pack.file_name.clone());
        }

        let removed = crate::instance_content::sync_managed_content_dir(
            instance_root,
            category,
            &instance_dir,
            &installed,
            lookups_complete,
        )
        .expect("sync should succeed");

        (installed, removed)
    }

    fn present_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("instance dir should be readable")
            .map(|entry| {
                entry
                    .expect("entry should be readable")
                    .file_name()
                    .into_string()
                    .expect("utf-8 name")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn only_local_entries_are_planned_so_modrinth_never_reaches_the_link_step() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let zip = root.join("Mine.zip");
        write_zip_pack(&zip, &[("pack.png", PNG_BYTES)]);
        import(&root, "Sky Pack", "resourcepack", &zip).expect("import should work");

        let mut list = load_content_list(&modlist, "resourcepack").expect("list should load");
        list.entries.insert(
            0,
            ContentEntry {
                id: "fresh-animations".to_string(),
                source: "modrinth".to_string(),
                version_rules: vec![],
                name: None,
                file_name: None,
                icon_path: None,
                description: None,
            },
        );
        let entries: Vec<&ContentEntry> = list.entries.iter().collect();

        let planned = plan_local_pack_installs(&modlist, "resourcepack", &entries);
        assert_eq!(
            planned
                .iter()
                .map(|p| p.file_name.as_str())
                .collect::<Vec<_>>(),
            ["Mine.zip"],
            "a Modrinth entry has no local file and must not be planned"
        );
        assert!(planned[0].present);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_entry_whose_file_vanished_is_reported_not_installed() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let zip = root.join("Gone.zip");
        write_zip_pack(&zip, &[("pack.png", PNG_BYTES)]);
        import(&root, "Sky Pack", "resourcepack", &zip).expect("import should work");
        fs::remove_file(modlist.join("resourcepacks").join("Gone.zip"))
            .expect("the copied pack should be removable");

        let list = load_content_list(&modlist, "resourcepack").expect("list should load");
        let entries: Vec<&ContentEntry> = list.entries.iter().collect();
        let planned = plan_local_pack_installs(&modlist, "resourcepack", &entries);

        assert_eq!(planned.len(), 1);
        assert!(
            !planned[0].present,
            "a missing file is reported, not silently dropped"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// **The two-pass test.** A pack linked without being put into the set C1
    /// compares against is deleted on the *next* launch — the wipe C1 closed,
    /// back in a form that takes two launches to see. Both shapes are covered,
    /// because an unpacked pack is a directory and the removal rule had to
    /// change for it.
    #[test]
    fn two_launches_in_a_row_leave_both_a_zip_and_a_folder_pack_in_place() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let instance_root = modlist.join("instances").join("1.20.1-forge");

        let zip = root.join("Zipped Pack.zip");
        write_zip_pack(&zip, &[("pack.png", PNG_BYTES)]);
        let folder = root.join("Folder Pack");
        write_dir_pack(&folder, &[("pack.png", PNG_BYTES), ("assets/a.txt", b"a")]);
        import(&root, "Sky Pack", "resourcepack", &zip).expect("zip import should work");
        import(&root, "Sky Pack", "resourcepack", &folder).expect("folder import should work");

        let list = load_content_list(&modlist, "resourcepack").expect("list should load");
        let entries: Vec<&ContentEntry> = list.entries.iter().collect();
        let instance_dir = instance_root.join("resourcepacks");

        // A file the user dropped in by hand: C1's invariant still holds.
        fs::create_dir_all(&instance_dir).expect("instance dir should be created");
        fs::write(instance_dir.join("user-hand-pack.zip"), b"mine")
            .expect("hand-placed file should be written");

        let (first, removed) = simulate_launch(
            &modlist,
            &instance_root,
            "resourcepack",
            "resourcepacks",
            &entries,
            true,
        );
        assert_eq!(
            first,
            vec!["Zipped Pack.zip".to_string(), "Folder Pack".to_string()]
        );
        assert!(removed.is_empty(), "nothing to remove on a first launch");
        assert_eq!(
            present_names(&instance_dir),
            ["Folder Pack", "Zipped Pack.zip", "user-hand-pack.zip"]
        );
        // The names reached the manifest, which is the part that makes the
        // second launch dangerous: C1 removes manifest names that are missing
        // from the new installed set.
        assert_eq!(
            crate::instance_content::read_manifest_files(&crate::instance_content::manifest_path(
                &instance_root,
                "resourcepacks"
            )),
            first
        );

        let (second, removed) = simulate_launch(
            &modlist,
            &instance_root,
            "resourcepack",
            "resourcepacks",
            &entries,
            true,
        );
        assert_eq!(second, first, "the second launch installs the same set");
        assert!(
            removed.is_empty(),
            "the second launch must not remove what it just installed"
        );
        assert_eq!(
            present_names(&instance_dir),
            ["Folder Pack", "Zipped Pack.zip", "user-hand-pack.zip"],
            "both packs and the hand-placed file survive launch two"
        );
        assert!(
            instance_dir
                .join("Folder Pack")
                .join("assets")
                .join("a.txt")
                .exists(),
            "the folder pack is reachable through whatever link or copy was used"
        );

        // Third launch with the folder pack dropped from the list: only that
        // one goes, whether it is a link or a copied directory.
        let kept: Vec<&ContentEntry> = entries
            .iter()
            .copied()
            .filter(|e| e.file_name.as_deref() != Some("Folder Pack"))
            .collect();
        let (third, removed) = simulate_launch(
            &modlist,
            &instance_root,
            "resourcepack",
            "resourcepacks",
            &kept,
            true,
        );
        assert_eq!(third, vec!["Zipped Pack.zip".to_string()]);
        assert_eq!(removed, vec!["Folder Pack".to_string()]);
        assert_eq!(
            present_names(&instance_dir),
            ["Zipped Pack.zip", "user-hand-pack.zip"]
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A Modrinth lookup that failed freezes removals for the category. A local
    /// pack has no lookup to fail, and must not be collateral damage: its link
    /// stays and its name stays in the manifest.
    #[test]
    fn a_failed_modrinth_lookup_does_not_disturb_a_local_pack() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let instance_root = modlist.join("instances").join("1.20.1-forge");
        let zip = root.join("Local Pack.zip");
        write_zip_pack(&zip, &[("pack.png", PNG_BYTES)]);
        import(&root, "Sky Pack", "resourcepack", &zip).expect("import should work");

        let list = load_content_list(&modlist, "resourcepack").expect("list should load");
        let entries: Vec<&ContentEntry> = list.entries.iter().collect();
        let instance_dir = instance_root.join("resourcepacks");

        simulate_launch(
            &modlist,
            &instance_root,
            "resourcepack",
            "resourcepacks",
            &entries,
            true,
        );
        // A Modrinth pack was linked last launch too.
        fs::write(instance_dir.join("modrinth-pack.zip"), b"cached")
            .expect("modrinth link should be written");
        let manifest = crate::instance_content::manifest_path(&instance_root, "resourcepacks");
        fs::write(
            &manifest,
            serde_json::to_string(&serde_json::json!({
                "version": 1,
                "category": "resourcepacks",
                "files": ["Local Pack.zip", "modrinth-pack.zip"],
            }))
            .expect("manifest json"),
        )
        .expect("manifest should be written");

        // Next launch: the Modrinth request fails, so `installed` is incomplete.
        let (_, removed) = simulate_launch(
            &modlist,
            &instance_root,
            "resourcepack",
            "resourcepacks",
            &entries,
            false,
        );

        assert!(removed.is_empty(), "a failed lookup removes nothing");
        assert_eq!(
            present_names(&instance_dir),
            ["Local Pack.zip", "modrinth-pack.zip"]
        );
        assert_eq!(
            crate::instance_content::read_manifest_files(&manifest),
            vec![
                "Local Pack.zip".to_string(),
                "modrinth-pack.zip".to_string()
            ],
            "the frozen manifest keeps both names"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn installing_over_our_own_directory_replaces_it_but_a_foreign_one_is_left_alone() {
        let root = unique_test_root();
        let modlist = modlist_dir(&root, "Sky Pack");
        let folder = root.join("Folder Pack");
        write_dir_pack(&folder, &[("pack.png", PNG_BYTES)]);
        import(&root, "Sky Pack", "resourcepack", &folder).expect("import should work");
        let source = modlist.join("resourcepacks").join("Folder Pack");

        let instance_dir = root.join("instance").join("resourcepacks");
        fs::create_dir_all(&instance_dir).expect("instance dir should be created");
        let target = instance_dir.join("Folder Pack");

        // A folder the user unpacked there themselves: the manifest does not
        // name it, so it is not ours and nothing happens to it.
        fs::create_dir_all(target.join("theirs")).expect("foreign dir should be created");
        assert_eq!(
            link_local_pack(&source, &target, false).expect("a foreign directory is not an error"),
            PackLinkOutcome::SkippedForeignDirectory
        );
        assert!(
            target.join("theirs").exists(),
            "deleting it would be C1's wipe in a new costume"
        );

        // The same directory, but the manifest says we put it there: it is the
        // copy fallback from our own last launch and it gives way.
        assert_eq!(
            link_local_pack(&source, &target, true).expect("install over our own directory"),
            PackLinkOutcome::Linked
        );
        assert!(
            !target.join("theirs").exists(),
            "our own stale copy is replaced"
        );
        assert!(target.join("pack.png").exists());

        // A stale file under that name is replaced either way: that is what
        // `create_file_link` already does for Modrinth packs.
        fs::remove_file(&target).expect("the link should be removable");
        fs::write(&target, b"stale file").expect("stale file should be written");
        link_local_pack(&source, &target, false).expect("install over a file");
        assert!(target.join("pack.png").exists());

        fs::remove_dir_all(&root).ok();
    }
}
