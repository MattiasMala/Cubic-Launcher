//! Differential sync of the content-pack links the launcher creates inside an
//! instance (`resourcepacks/`, `shaderpacks/`, `datapacks/`).
//!
//! Every launch used to wipe those directories before linking, so packs the
//! user had dropped in by hand disappeared, and in `shaderpacks/` the
//! Iris/Optifine `*.txt` configs went with them. Instead we record what we
//! linked in a per-category manifest and, on the next launch, remove only the
//! manifest entries that are no longer wanted.
//!
//! Ownership is decided by the manifest, not by "is this a symlink into the
//! cache": [`crate::instance_mods::create_file_link`] falls back to a hard link
//! when symlink creation fails (Windows without developer mode), so the symlink
//! criterion breaks on exactly the platform where it would matter.
//!
//! A missing, unreadable or future-versioned manifest means "I do not know what
//! I put here", so nothing is removed and the manifest is simply rewritten. The
//! intended failure mode is one pack too many, never one file too few.
//!
//! `mods/` is deliberately out of scope (decision D6) and keeps using
//! [`crate::instance_mods::clear_instance_mods_directory`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::path_safety::validate_path_component;

/// Manifest schema version. A manifest written by a newer launcher is treated
/// as unreadable, which means "remove nothing".
const MANIFEST_VERSION: u32 = 1;

/// What the launcher linked into one instance subdirectory on the last launch.
///
/// `origins` was added without bumping [`MANIFEST_VERSION`]: it is optional on
/// the way in, and a launcher that predates it ignores an unknown key, so the
/// two can read each other's manifests. A manifest without it is an ordinary
/// state — every one written before precheck-3 — and reads as "origin unknown".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedContentManifest {
    pub version: u32,
    pub category: String,
    pub files: Vec<String>,
    /// `file name → which Modrinth entry and version it is`, for the files that
    /// came from Modrinth. Local packs have no entry here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub origins: BTreeMap<String, ManagedFileOrigin>,
    /// The names the launcher put here as a **real directory**: an unpacked
    /// local pack it had to copy because the platform refused a link to a
    /// folder, or because a world may hold no link (D144). Only these are
    /// folders the launcher may delete (todo.md:1689): a folder the user made
    /// under a managed name is theirs, and it stays.
    ///
    /// Added like `origins`, without bumping [`MANIFEST_VERSION`]. `None` is a
    /// manifest written before the field, which did not say what shape it
    /// left; see [`PreviousContent::owns_directory`] for how that reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_dirs: Option<Vec<String>>,
}

/// Where one managed file came from (precheck-3, contentpack-4, contentpack-7).
///
/// Recorded when the file is linked, because afterwards it is readable nowhere
/// else that works everywhere: the version id is in the symlink's target, and
/// on Windows without Developer Mode the link is a hard link with no target to
/// read. The entry id is what ties an old file name to its replacement when an
/// update renames the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedFileOrigin {
    pub entry_id: String,
    pub version_id: String,
}

/// What one category directory held after the last launch, as far as the
/// launcher can tell: the manifest, or — for an instance that never had one —
/// what [`previous_content`] could adopt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviousContent {
    pub files: Vec<String>,
    pub origins: BTreeMap<String, ManagedFileOrigin>,
    /// A manifest file is on disk, readable or not. Without one and without
    /// anything adopted, a sync with nothing to install writes nothing.
    pub manifest_exists: bool,
    /// The manifest's `copied_dirs`: `None` when it predates the field.
    pub copied_dirs: Option<Vec<String>>,
}

impl PreviousContent {
    /// Whether the last launch put `name` here: the question every link asks
    /// before it replaces a real file (D1).
    pub fn owns(&self, name: &str) -> bool {
        self.files.iter().any(|listed| listed == name)
    }

    /// Whether a **real directory** under `name` is the launcher's copy, the
    /// only kind of folder it may delete (todo.md:1689).
    ///
    /// A manifest that records `copied_dirs` is the whole answer. One written
    /// before it only lists names, and until then a listed name was ownership
    /// whatever its shape — the rule this keeps for that one launch, so a
    /// copied folder pack of an older launcher is still replaced and not
    /// skipped forever — except for a Modrinth file: Modrinth ships files, so
    /// a folder under one of their names was never the launcher's.
    pub fn owns_directory(&self, name: &str) -> bool {
        match &self.copied_dirs {
            Some(copied) => copied.iter().any(|listed| listed == name),
            None => self.owns(name) && !self.origins.contains_key(name),
        }
    }

    /// [`Self::owns`] or [`Self::owns_directory`], by what is on disk at
    /// `path` now: the ownership a link must have before it replaces it.
    pub fn owns_target(&self, name: &str, path: &Path) -> bool {
        if is_real_dir(path) {
            self.owns_directory(name)
        } else {
            self.owns(name)
        }
    }
}

/// What this launch linked into one category directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledContent {
    pub files: Vec<String>,
    pub origins: BTreeMap<String, ManagedFileOrigin>,
    /// The names this launch placed as a real directory (a copied folder).
    pub copied_dirs: Vec<String>,
}

impl InstalledContent {
    /// Record one linked name, once. `origin` is `None` for a local pack.
    pub fn add(&mut self, name: &str, origin: Option<ManagedFileOrigin>) {
        if !self.files.iter().any(|listed| listed == name) {
            self.files.push(name.to_string());
        }
        if let Some(origin) = origin {
            self.origins.insert(name.to_string(), origin);
        }
    }

    /// Record one local pack this launch copied in as a real directory.
    pub fn add_copied_directory(&mut self, name: &str) {
        self.add(name, None);
        if !self.copied_dirs.iter().any(|listed| listed == name) {
            self.copied_dirs.push(name.to_string());
        }
    }
}

/// What [`sync_managed_content`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentSyncOutcome {
    pub removed: Vec<String>,
    /// `(name, error)` for every stale entry that would not go: a file held
    /// open on Windows, a permission. A warning for the launch log, never a
    /// launch that stops (contentpack-10); the name stays in the manifest, so
    /// the next launch tries again.
    pub failed_removals: Vec<(String, String)>,
    /// Managed names that are now a real folder the launcher did not create:
    /// kept, never deleted, and out of the manifest from now on — the folder
    /// is the user's (todo.md:1689). For a warning in the launch log.
    pub kept_foreign_dirs: Vec<String>,
    /// What this launch would have removed, had every lookup answered: the
    /// packs a failed lookup keeps in the game although the list dropped them
    /// (todo.md:1691). Empty on a complete launch.
    pub frozen: Vec<String>,
    /// `(old name, new name)` for every entry whose file name changed since
    /// the last launch: what the game's own settings still call by the old
    /// name (D145).
    pub renamed: Vec<(String, String)>,
}

/// One entry observed inside the instance subdirectory at sync time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentEntry {
    pub name: String,
    /// `true` for a real directory, as opposed to a symlink pointing at one.
    /// Removing the two takes different calls.
    pub is_real_dir: bool,
}

/// Path of the manifest for `category` (`resourcepacks`, `shaderpacks`,
/// `datapacks`). It lives in the instance's own `.cubic/` directory (decision
/// D39): hidden by the leading dot, outside the pack folders so it never shows
/// up as a bogus pack, and in one place instead of three dotfiles in the
/// instance root.
pub fn manifest_path(instance_root: &Path, category: &str) -> PathBuf {
    instance_root
        .join(".cubic")
        .join(format!("managed-{category}.json"))
}

/// Decide which names to remove, given the previous manifest, the set we just
/// installed, and what is actually in the directory.
///
/// A name is removed only when all three hold: it was in the previous manifest,
/// it is not in the new set, and it is on disk. Anything else — unknown files,
/// unknown directories, entries the user already deleted — is left alone.
///
/// The manifest is the only thing that grants ownership, and that is what makes
/// it safe to remove a **real directory**: local packs can be unpacked folders,
/// and on a platform where a directory cannot be linked the launcher copies one
/// in, so "it is a directory, therefore not mine" stopped being true. A
/// directory the manifest does not name is still never touched, and one it
/// names goes only if it is a copy the launcher made: the sync sets the
/// others aside before removing anything (`copied_dirs`, todo.md:1689).
pub fn plan_stale_removals(
    previous: &[String],
    installed: &[String],
    present: &[PresentEntry],
) -> Vec<String> {
    let keep: BTreeSet<&str> = installed.iter().map(String::as_str).collect();
    let mut planned: BTreeSet<&str> = BTreeSet::new();
    let mut removals = Vec::new();

    for name in previous {
        if keep.contains(name.as_str()) || !planned.insert(name.as_str()) {
            continue;
        }
        if present.iter().any(|entry| entry.name == *name) {
            removals.push(name.clone());
        }
    }

    removals
}

/// Read the file list of a manifest. Any problem — missing file, unreadable
/// bytes, malformed JSON, unknown schema version — yields an empty list, which
/// means "remove nothing". Entries that are not a safe single path component
/// are dropped: a corrupt manifest must not be able to delete outside the
/// directory it describes.
pub fn read_manifest_files(manifest_path: &Path) -> Vec<String> {
    read_manifest(manifest_path)
        .map(|manifest| manifest.files)
        .unwrap_or_default()
}

/// The whole manifest, with the same sanitising as [`read_manifest_files`]:
/// `None` for every problem, and an origin survives only for a name the file
/// list keeps and with a version id that is a safe path component (it names a
/// cache directory).
pub fn read_manifest(manifest_path: &Path) -> Option<ManagedContentManifest> {
    let raw = fs::read_to_string(manifest_path).ok()?;
    let mut manifest: ManagedContentManifest = serde_json::from_str(&raw).ok()?;
    if manifest.version != MANIFEST_VERSION {
        return None;
    }

    manifest
        .files
        .retain(|name| validate_path_component(name).is_ok());
    let files = &manifest.files;
    manifest.origins.retain(|name, origin| {
        files.iter().any(|listed| listed == name)
            && validate_path_component(&origin.version_id).is_ok()
    });
    if let Some(copied) = manifest.copied_dirs.as_mut() {
        copied.retain(|name| files.iter().any(|listed| listed == name));
    }
    Some(manifest)
}

#[cfg(test)]
fn write_manifest(manifest_path: &Path, category: &str, files: &[String]) -> Result<()> {
    write_manifest_with_origins(manifest_path, category, files, &BTreeMap::new(), &[])
}

fn write_manifest_with_origins(
    manifest_path: &Path,
    category: &str,
    files: &[String],
    origins: &BTreeMap<String, ManagedFileOrigin>,
    copied_dirs: &[String],
) -> Result<()> {
    let manifest = ManagedContentManifest {
        version: MANIFEST_VERSION,
        category: category.to_string(),
        files: files.to_vec(),
        origins: origins
            .iter()
            .filter(|(name, _)| files.contains(name))
            .map(|(name, origin)| (name.clone(), origin.clone()))
            .collect(),
        copied_dirs: Some(
            copied_dirs
                .iter()
                .filter(|name| files.contains(name))
                .cloned()
                .collect(),
        ),
    };
    let body = serde_json::to_string_pretty(&manifest)
        .context("failed to serialize managed content manifest")?;

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    crate::atomic_write::write_atomically(manifest_path, body).with_context(|| {
        format!(
            "failed to write managed content manifest at {}",
            manifest_path.display()
        )
    })
}

fn present_entries(instance_dir: &Path) -> Result<Vec<PresentEntry>> {
    if !instance_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(instance_dir)
        .with_context(|| format!("failed to read {}", instance_dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!("failed to inspect entry inside {}", instance_dir.display())
        })?;
        let path = entry.path();
        // A name we cannot represent as UTF-8 can never match a manifest entry,
        // so it is reported as-is only if it round-trips; otherwise it is
        // skipped and therefore never removed.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let file_type = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?
            .file_type();

        entries.push(PresentEntry {
            name,
            is_real_dir: file_type.is_dir() && !file_type.is_symlink(),
        });
    }

    Ok(entries)
}

/// Names to keep in the manifest when the installed set cannot be trusted to be
/// complete: everything the previous manifest listed, plus what we did install.
///
/// `ContentEntry` stores only an id, never a filename, so a failed version
/// lookup cannot be mapped back to the file it produced last time. The only
/// conservative answer is to carry the whole previous list forward.
pub fn carry_forward(previous: &[String], installed: &[String]) -> Vec<String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut merged = Vec::with_capacity(previous.len() + installed.len());

    for name in previous.iter().chain(installed.iter()) {
        if seen.insert(name.as_str()) {
            merged.push(name.clone());
        }
    }

    merged
}

/// Remove the links we installed last time and no longer want, then rewrite the
/// manifest. Returns the names actually removed.
///
/// `instance_dir` is the category directory inside `instance_root`; it may not
/// exist yet. Nothing is written when there is no manifest and nothing was
/// installed, so instances without content packs stay clean.
///
/// `installed_is_complete` is `false` when at least one entry's version lookup
/// failed — a Modrinth error, not an answer. `installed` is then an incomplete
/// picture of what should be linked, so nothing is removed and the manifest
/// keeps the previous names alongside the new ones. A transient API failure
/// must not delete a pack the user still wants; it leaves one too many instead.
///
/// The manifest-only form of [`sync_managed_content`]: no adoption, no
/// origins. The launch uses the full one.
pub fn sync_managed_content_dir(
    instance_root: &Path,
    category: &str,
    instance_dir: &Path,
    installed: &[String],
    installed_is_complete: bool,
) -> Result<Vec<String>> {
    let previous = previous_from_manifest(instance_root, category);
    let installed = InstalledContent {
        files: installed.to_vec(),
        ..InstalledContent::default()
    };
    sync_managed_content(
        instance_root,
        category,
        instance_dir,
        &previous,
        &installed,
        installed_is_complete,
    )
    .map(|outcome| outcome.removed)
}

/// [`sync_managed_content_dir`] with what the caller already knows: the
/// previous state it linked against (possibly adopted, see
/// [`previous_content`]) and the origin of every Modrinth file it linked.
///
/// `manifest_root` is the directory whose `.cubic/` holds the manifest: the
/// instance root for `resourcepacks/` and `shaderpacks/`, the world directory
/// for a world's `datapacks/` (D144).
pub fn sync_managed_content(
    manifest_root: &Path,
    category: &str,
    instance_dir: &Path,
    previous: &PreviousContent,
    installed: &InstalledContent,
    installed_is_complete: bool,
) -> Result<ContentSyncOutcome> {
    let manifest_path = manifest_path(manifest_root, category);
    let renamed = renamed_packs(previous, installed);

    if installed.files.is_empty() && previous.files.is_empty() && !previous.manifest_exists {
        return Ok(ContentSyncOutcome::default());
    }

    if !installed_is_complete {
        // Reading the directory only feeds the warning: a failure here costs
        // the list of what stays, not the launch.
        let present = present_entries(instance_dir).unwrap_or_default();
        let frozen = plan_stale_removals(&previous.files, &installed.files, &present);
        let files = carry_forward(&previous.files, &installed.files);
        let mut origins = previous.origins.clone();
        origins.extend(
            installed
                .origins
                .iter()
                .map(|(name, origin)| (name.clone(), origin.clone())),
        );
        let copied_dirs = carried_copied_dirs(previous, &installed.copied_dirs, &present);
        write_manifest_with_origins(&manifest_path, category, &files, &origins, &copied_dirs)?;
        return Ok(ContentSyncOutcome {
            renamed,
            frozen,
            ..ContentSyncOutcome::default()
        });
    }

    let present = present_entries(instance_dir)?;
    let removals = plan_stale_removals(&previous.files, &installed.files, &present);
    let (removals, kept_foreign_dirs) = set_aside_foreign_dirs(previous, removals, &present);

    let mut removed = Vec::new();
    let mut failed_removals = Vec::new();
    for name in removals {
        match remove_managed_entry(&instance_dir.join(&name)) {
            Ok(()) => removed.push(name),
            Err(error) => failed_removals.push((name, error.to_string())),
        }
    }

    // What would not go is still ours: it stays listed, with its origin and
    // its shape.
    let mut files = installed.files.clone();
    let mut origins = installed.origins.clone();
    let mut copied_dirs = installed.copied_dirs.clone();
    for (name, _) in &failed_removals {
        files.push(name.clone());
        if let Some(origin) = previous.origins.get(name) {
            origins.insert(name.clone(), origin.clone());
        }
        if previous.owns_directory(name) {
            copied_dirs.push(name.clone());
        }
    }
    write_manifest_with_origins(&manifest_path, category, &files, &origins, &copied_dirs)?;

    Ok(ContentSyncOutcome {
        removed,
        failed_removals,
        kept_foreign_dirs,
        renamed,
        frozen: Vec::new(),
    })
}

/// Split planned removals into what may go and the real folders the launcher
/// did not create (todo.md:1689). The manifest names both — a folder the user
/// put under the name of a pack we linked — but only our own copy is ours to
/// delete; the other is kept, and reported.
fn set_aside_foreign_dirs(
    previous: &PreviousContent,
    removals: Vec<String>,
    present: &[PresentEntry],
) -> (Vec<String>, Vec<String>) {
    removals.into_iter().partition(|name| {
        !present
            .iter()
            .any(|entry| entry.name == *name && entry.is_real_dir)
            || previous.owns_directory(name)
    })
}

/// The `copied_dirs` of a manifest that carries the previous names forward:
/// this launch's copies, and the previous ones still on disk as a folder.
fn carried_copied_dirs(
    previous: &PreviousContent,
    installed_copied: &[String],
    present: &[PresentEntry],
) -> Vec<String> {
    let mut copied = installed_copied.to_vec();
    for name in &previous.files {
        let still_a_folder = present
            .iter()
            .any(|entry| entry.name == *name && entry.is_real_dir);
        if still_a_folder && previous.owns_directory(name) && !copied.contains(name) {
            copied.push(name.clone());
        }
    }
    copied
}

/// The previous state as the manifest alone tells it.
pub fn previous_from_manifest(manifest_root: &Path, category: &str) -> PreviousContent {
    let path = manifest_path(manifest_root, category);
    let manifest_exists = fs::symlink_metadata(&path).is_ok();
    match read_manifest(&path) {
        Some(manifest) => PreviousContent {
            files: manifest.files,
            origins: manifest.origins,
            manifest_exists,
            copied_dirs: manifest.copied_dirs,
        },
        None => PreviousContent {
            manifest_exists,
            ..PreviousContent::default()
        },
    }
}

/// The previous state of one category directory, adopting what an instance
/// without a manifest provably got from the launcher (`r2-aggiornamento-da-0.1.2-1`).
///
/// A manifest on disk — readable or not — is the whole answer, as before. With
/// none, and `adopt_from_cache` given, the names adopted are the ones only the
/// launcher can have put there: a symlink into the content-pack cache, or a
/// real file whose bytes equal a cache file of the same name (the hard-link
/// fallback on Windows, or a copy). Up to v0.1.2 the launcher wiped and
/// relinked these directories without writing a manifest, so without this
/// step their packs were never updated nor removed again. Anything else — a
/// zip the user dropped in, a folder, a link elsewhere — stays foreign.
pub fn previous_content(
    manifest_root: &Path,
    category: &str,
    instance_dir: &Path,
    adopt_from_cache: Option<&Path>,
) -> PreviousContent {
    let previous = previous_from_manifest(manifest_root, category);
    if previous.manifest_exists {
        return previous;
    }
    let Some(cache_dir) = adopt_from_cache else {
        return previous;
    };

    PreviousContent {
        files: adoptable_cache_files(instance_dir, cache_dir),
        ..previous
    }
}

/// The names in `instance_dir` that are, byte for byte or by link, a file of
/// the content-pack cache. Sorted, for a reproducible manifest.
fn adoptable_cache_files(instance_dir: &Path, cache_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(instance_dir) else {
        return Vec::new();
    };
    let canonical_cache = fs::canonicalize(cache_dir).ok();
    let mut adopted = Vec::new();

    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if validate_path_component(&name).is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();

        let ours = if file_type.is_symlink() {
            fs::read_link(&path).is_ok_and(|target| {
                let target = if target.is_relative() {
                    instance_dir.join(target)
                } else {
                    target
                };
                // A dangling link into the cache is still ours: it is what
                // clearing the cache leaves behind.
                target.starts_with(cache_dir)
                    || canonical_cache.as_ref().is_some_and(|canonical| {
                        target.starts_with(canonical)
                            || fs::canonicalize(&target)
                                .is_ok_and(|resolved| resolved.starts_with(canonical))
                    })
            })
        } else if file_type.is_file() {
            matches_a_cache_file(&path, &name, metadata.len(), cache_dir)
        } else {
            false
        };

        if ours {
            adopted.push(name);
        }
    }

    adopted.sort();
    adopted
}

/// Whether a cache file with the same name has the same bytes: in the flat
/// layout of v0.1.x (`<cache>/<file>`) or in the per-version one of C5
/// (`<cache>/<version id>/<file>`). Same name first, then same size, then the
/// bytes: the comparison is rare and short.
fn matches_a_cache_file(path: &Path, name: &str, len: u64, cache_dir: &Path) -> bool {
    let mut candidates = vec![cache_dir.join(name)];
    if let Ok(entries) = fs::read_dir(cache_dir) {
        candidates.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|dir| dir.is_dir())
                .map(|dir| dir.join(name)),
        );
    }

    candidates.iter().any(|candidate| {
        fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file() && metadata.len() == len)
            && same_bytes(path, candidate).unwrap_or(false)
    })
}

fn same_bytes(left: &Path, right: &Path) -> std::io::Result<bool> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = vec![0u8; 64 * 1024];
    let mut right_buffer = vec![0u8; 64 * 1024];

    loop {
        let read = left.read(&mut left_buffer)?;
        if read == 0 {
            // The sizes were equal, so the right side is at its end too.
            return Ok(true);
        }
        right.read_exact(&mut right_buffer[..read])?;
        if left_buffer[..read] != right_buffer[..read] {
            return Ok(false);
        }
    }
}

/// `(old name, new name)` for every entry whose file changed name between the
/// two launches: the old name had an origin, the new one has the same entry.
///
/// Only names with an origin take part. A local pack keeps its name for good,
/// and a file adopted from an instance without manifest has no known entry.
pub fn renamed_packs(
    previous: &PreviousContent,
    installed: &InstalledContent,
) -> Vec<(String, String)> {
    let mut renamed = Vec::new();
    for (old_name, old_origin) in &previous.origins {
        if installed.files.iter().any(|name| name == old_name) {
            continue;
        }
        if let Some((new_name, _)) = installed.origins.iter().find(|(new_name, origin)| {
            origin.entry_id == old_origin.entry_id && *new_name != old_name
        }) {
            renamed.push((old_name.clone(), new_name.clone()));
        }
    }
    renamed
}

/// Clean up a category directory the launcher no longer links into, and
/// return what it removed.
///
/// Data packs went to `<instance>/datapacks/` until D144, a folder Minecraft
/// never reads. What the manifest names there — or, without a manifest, what
/// is provably the launcher's (see [`previous_content`]) — is removed, then the
/// manifest itself, then the folder if that left it empty. Anything else in it
/// stays where it is.
pub fn retire_legacy_content_dir(
    instance_root: &Path,
    category: &str,
    cache_dir: &Path,
) -> Result<Vec<String>> {
    let instance_dir = instance_root.join(category);
    let previous = previous_content(instance_root, category, &instance_dir, Some(cache_dir));
    if previous.files.is_empty() && !previous.manifest_exists {
        return Ok(Vec::new());
    }

    let present = present_entries(&instance_dir)?;
    let removals = plan_stale_removals(&previous.files, &[], &present);
    // A folder the user made there is not ours, and it keeps its place.
    let (removals, _kept) = set_aside_foreign_dirs(&previous, removals, &present);
    for name in &removals {
        let path = instance_dir.join(name);
        remove_managed_entry(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }

    let manifest = manifest_path(instance_root, category);
    match fs::remove_file(&manifest) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", manifest.display()))
        }
    }
    // `remove_dir` refuses a directory that still holds something: whatever is
    // left in it is not ours, and it keeps its folder.
    let _ = fs::remove_dir(&instance_dir);

    Ok(removals)
}

/// The worlds of one instance: every entry of its `saves/` that is a world
/// directory (it has a `level.dat`), a shared world's link included (D144).
///
/// The returned path is the one inside `saves/`, so the launch log names the
/// world as the instance sees it; two entries that resolve to one directory
/// are listed once. A missing `saves/` is an instance that never ran.
pub fn instance_world_dirs(instance_root: &Path) -> Result<Vec<PathBuf>> {
    let saves_dir = instance_root.join("saves");
    let entries = match fs::read_dir(&saves_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", saves_dir.display()))
        }
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to inspect {}", saves_dir.display()))?;
        candidates.push(entry.path());
    }
    // Sorted before deduplicating, so which of two names for one world is
    // kept does not depend on the order the directory is read in.
    candidates.sort();

    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    Ok(candidates
        .into_iter()
        // `is_file` follows the link, which is the point: a shared world is a
        // link to a directory, and a dangling one is no world at all.
        .filter(|path| path.join("level.dat").is_file())
        .filter(|path| seen.insert(fs::canonicalize(path).unwrap_or_else(|_| path.clone())))
        .collect())
}

/// One file's rewrite: the new text, or `None` when nothing in it changed.
type RewriteFn = fn(&str, &[(String, String)]) -> Option<String>;

/// Point the game's own settings at the new name of every renamed pack (D145).
///
/// Minecraft remembers the enabled resource packs, and Iris or OptiFine the
/// chosen shader, **by file name**. An update ships a new name, and without
/// this the pack comes back disabled. Only the instance's files are touched,
/// and only the values that name an old file: every other line stays as it
/// was, byte for byte. Returns the files rewritten; a file that does not exist
/// is not an error, the game has simply not written it yet.
pub fn rewrite_renamed_pack_references(
    instance_root: &Path,
    category: &str,
    renamed: &[(String, String)],
) -> Result<Vec<PathBuf>> {
    if renamed.is_empty() {
        return Ok(Vec::new());
    }

    let targets: Vec<(PathBuf, RewriteFn)> = match category {
        "resourcepacks" => vec![(
            instance_root.join("options.txt"),
            rewrite_options_pack_lists,
        )],
        "shaderpacks" => vec![
            (
                instance_root.join("config").join("iris.properties"),
                rewrite_shader_pack_property,
            ),
            // Oculus, the Forge port of Iris, keeps the same key in its own file.
            (
                instance_root.join("config").join("oculus.properties"),
                rewrite_shader_pack_property,
            ),
            (
                instance_root.join("optionsshaders.txt"),
                rewrite_shader_pack_property,
            ),
        ],
        _ => Vec::new(),
    };

    let mut rewritten = Vec::new();
    for (path, rewrite) in targets {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        let Some(updated) = rewrite(&text, renamed) else {
            continue;
        };
        crate::atomic_write::write_atomically(&path, updated)
            .with_context(|| format!("failed to write {}", path.display()))?;
        rewritten.push(path);
    }

    Ok(rewritten)
}

/// The two `options.txt` keys that list resource packs, as the game writes
/// them: `resourcePacks:["vanilla","file/Pack.zip"]`. `None` when nothing
/// names an old file.
fn rewrite_options_pack_lists(text: &str, renamed: &[(String, String)]) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());

    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let rewritten = body.split_once(':').and_then(|(key, value)| {
            if key != "resourcePacks" && key != "incompatibleResourcePacks" {
                return None;
            }
            let packs: Vec<String> = serde_json::from_str(value).ok()?;
            let packs = rename_pack_ids(packs, renamed)?;
            Some(format!("{key}:{}", serde_json::to_string(&packs).ok()?))
        });

        match rewritten {
            Some(rewritten) => {
                changed = true;
                out.push_str(&rewritten);
                out.push_str(ending);
            }
            None => out.push_str(line),
        }
    }

    changed.then_some(out)
}

/// `file/<old>` → `file/<new>` in one pack list, or `None` if nothing moved.
/// When the list already holds the new name, the old entry is dropped instead
/// of becoming a duplicate.
fn rename_pack_ids(packs: Vec<String>, renamed: &[(String, String)]) -> Option<Vec<String>> {
    let mut changed = false;
    let mut out: Vec<String> = Vec::with_capacity(packs.len());

    for pack in &packs {
        let new_name = pack
            .strip_prefix("file/")
            .and_then(|name| renamed.iter().find(|(old, _)| old == name))
            .map(|(_, new)| format!("file/{new}"));
        match new_name {
            Some(new_id) => {
                changed = true;
                if !packs.contains(&new_id) && !out.contains(&new_id) {
                    out.push(new_id);
                }
            }
            None => out.push(pack.clone()),
        }
    }

    changed.then_some(out)
}

/// The `shaderPack` key of `iris.properties` / `optionsshaders.txt`, in the
/// `java.util.Properties` format both are read with. The line is rewritten as
/// `shaderPack=<new>`, escaped so that `Properties.load` reads back exactly the
/// new name.
fn rewrite_shader_pack_property(text: &str, renamed: &[(String, String)]) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());

    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let new_name = property_value(body, "shaderPack").and_then(|value| {
            renamed
                .iter()
                .find(|(old, _)| *old == value)
                .map(|(_, new)| new.clone())
        });

        match new_name {
            Some(new_name) => {
                changed = true;
                out.push_str("shaderPack=");
                out.push_str(&escape_property_value(&new_name));
                out.push_str(ending);
            }
            None => out.push_str(line),
        }
    }

    changed.then_some(out)
}

/// The unescaped value of `key` on one properties line, if the line sets it.
fn property_value(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(key)?;
    let mut chars = rest.chars().peekable();
    // After the key: optional blanks, at most one `=` or `:`, optional blanks.
    while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
        chars.next();
    }
    match chars.peek() {
        Some('=') | Some(':') => {
            chars.next();
        }
        // `shaderPackFoo=…` is another key.
        Some(_) if !rest.starts_with([' ', '\t']) => return None,
        _ => {}
    }
    while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
        chars.next();
    }
    Some(unescape_property_value(&chars.collect::<String>()))
}

fn unescape_property_value(raw: &str) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0u16; 2];
            units.extend_from_slice(c.encode_utf16(&mut buffer));
            continue;
        }
        let Some(escaped) = chars.next() else {
            break;
        };
        let decoded = match escaped {
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            'f' => '\u{0c}',
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(unit) = u16::from_str_radix(&hex, 16) {
                    units.push(unit);
                }
                continue;
            }
            other => other,
        };
        let mut buffer = [0u16; 2];
        units.extend_from_slice(decoded.encode_utf16(&mut buffer));
    }
    String::from_utf16_lossy(&units)
}

/// `Properties.store` escaping for a value: the separators, the comment marks,
/// a leading blank, the control characters, and everything outside printable
/// ASCII as `\uXXXX` — Iris writes the file as ISO-8859-1.
fn escape_property_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (index, c) in value.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            ' ' if index == 0 => out.push_str("\\ "),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0c}' => out.push_str("\\f"),
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => {
                let mut buffer = [0u16; 2];
                for unit in c.encode_utf16(&mut buffer) {
                    out.push_str(&format!("\\u{unit:04X}"));
                }
            }
        }
    }
    out
}

/// Remove one managed entry, whatever shape it has on disk.
///
/// Three shapes exist and they want three different calls: a symlink to a file,
/// a symlink to a directory, and a real directory the launcher copied in
/// because the platform refused to link one. Using the wrong call either fails
/// outright or, worse, succeeds on the wrong thing.
fn remove_managed_entry(path: &Path) -> std::io::Result<()> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() {
        remove_symlink(path)
    } else if file_type.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// On Windows a symlink to a directory — or a junction — is removed with
/// `remove_dir`, not `remove_file`. Which kind of link it is comes from the
/// link itself (`symlink_metadata`), never from what it points at
/// (contentpack-10): `Path::is_dir` follows the link, and on a directory link
/// whose folder is gone it answered "not a directory", `remove_file` failed,
/// and every launch stopped on «failed to remove stale content pack» until the
/// user deleted the link by hand. Shared by every caller that removes a link.
#[cfg(target_family = "windows")]
pub(crate) fn remove_symlink(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    if fs::symlink_metadata(path)?.file_type().is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// On unix `unlink` removes a symlink regardless of what it points at.
#[cfg(target_family = "unix")]
pub(crate) fn remove_symlink(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

/// What [`link_content_file`] did under one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLinkOutcome {
    Linked,
    /// Nothing was done: a real file sits under that name and the previous
    /// manifest does not claim it, so it is not ours to replace.
    SkippedForeignFile,
    /// Nothing was done: a real folder sits under that name and it is not a
    /// copy the launcher made (todo.md:1558). It used to stop the launch with
    /// «failed to link content pack»; it is the user's, and it stays.
    SkippedForeignDirectory,
}

/// Link a downloaded pack under `target`, applying the manifest rule on the way
/// in as [`plan_stale_removals`] applies it on the way out (D1).
///
/// `target_is_ours` says whether the previous manifest names this file. A link
/// under that name is always replaced — ours or not, dangling or not, it holds
/// no bytes of its own. A real file is replaced only when the manifest names
/// it: that is the hard-link fallback of our own last launch on Windows. A real
/// file the manifest does not name is a pack the user dropped in, and it is
/// left exactly as it is; the caller says so in the launch log and keeps the
/// name out of the new manifest, so the sync does not remove it either.
pub fn link_content_file(
    source: &Path,
    target: &Path,
    target_is_ours: bool,
) -> Result<ContentLinkOutcome> {
    link_content_file_as(source, target, target_is_ours, LinkKind::Symlink)
}

/// How a pack reaches its directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A symlink, falling back to a hard link: the instance's own pack folders.
    Symlink,
    /// Never a symlink — a hard link, falling back to a copy: a world's
    /// `datapacks/` (D144). Since 1.20 Minecraft walks a world's folder before
    /// opening it and refuses the whole world, with its «symbolic links»
    /// warning screen, when anything inside is a symlink whose target
    /// `allowed_symlinks.txt` does not allow (`DirectoryValidator`, measured
    /// on the 1.20.1 client jar). A hard link or a copy is an ordinary file,
    /// and the world stays openable inside the launcher and out of it.
    NoSymlink,
}

/// [`link_content_file`] with the shape chosen by the caller.
pub fn link_content_file_as(
    source: &Path,
    target: &Path,
    target_is_ours: bool,
    kind: LinkKind,
) -> Result<ContentLinkOutcome> {
    if is_real_file(target) {
        if !target_is_ours {
            return Ok(ContentLinkOutcome::SkippedForeignFile);
        }
        fs::remove_file(target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
    } else if is_real_dir(target) {
        // `target_is_ours` for a folder is the manifest's `copied_dirs`
        // (`PreviousContent::owns_target`): our own copy gives way.
        if !target_is_ours {
            return Ok(ContentLinkOutcome::SkippedForeignDirectory);
        }
        fs::remove_dir_all(target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
    }

    match kind {
        LinkKind::Symlink => crate::instance_mods::create_file_link(source, target)?,
        LinkKind::NoSymlink => place_file_without_symlink(source, target)?,
    }
    Ok(ContentLinkOutcome::Linked)
}

/// A hard link to `source` at `target`, or a copy where the two are on
/// different filesystems. A symlink already under the name — it holds no bytes
/// of its own — is removed first; a real file must have been cleared by the
/// caller, who holds the manifest.
pub fn place_file_without_symlink(source: &Path, target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        remove_symlink(target)
            .with_context(|| format!("failed to remove existing link {}", target.display()))?;
    }

    fs::hard_link(source, target).or_else(|hard_link_error| {
        fs::copy(source, target).map(|_| ()).map_err(|copy_error| {
            // A half-written copy would be a real file no manifest names:
            // the next launch would take it for the user's and skip the pack
            // for good (contentpack-6).
            let _ = fs::remove_file(target);
            anyhow::anyhow!(copy_error).context(format!(
                "failed to copy {} to {} after the hard link failed: {hard_link_error}",
                source.display(),
                target.display()
            ))
        })
    })
}

/// A regular file, and not a link pointing at one: the shape the launcher
/// cannot tell from a user's file without the manifest.
pub fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

/// A directory, and not a link pointing at one: the shape the launcher cannot
/// tell from a user's folder without the manifest's `copied_dirs`.
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| {
            let file_type = metadata.file_type();
            file_type.is_dir() && !file_type.is_symlink()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        carry_forward, escape_property_value, instance_world_dirs, link_content_file,
        manifest_path, plan_stale_removals, previous_content, read_manifest, read_manifest_files,
        renamed_packs, retire_legacy_content_dir, rewrite_renamed_pack_references,
        sync_managed_content, sync_managed_content_dir, unescape_property_value, write_manifest,
        ContentLinkOutcome, InstalledContent, ManagedContentManifest, ManagedFileOrigin,
        PresentEntry,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-instance-content-test-{timestamp}"))
    }

    fn file(name: &str) -> PresentEntry {
        PresentEntry {
            name: name.to_string(),
            is_real_dir: false,
        }
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn fixture_instance(root: &Path, category: &str) -> PathBuf {
        let instance_dir = root.join(category);
        fs::create_dir_all(&instance_dir).expect("instance category dir should be created");
        instance_dir
    }

    #[test]
    fn missing_manifest_removes_nothing() {
        let removals = plan_stale_removals(
            &[],
            &names(&["fresh.zip"]),
            &[file("fresh.zip"), file("user-pack.zip")],
        );

        assert!(
            removals.is_empty(),
            "with no manifest nothing is known to be ours"
        );
    }

    #[test]
    fn manifest_entry_dropped_from_new_set_is_removed() {
        let removals = plan_stale_removals(
            &names(&["kept.zip", "dropped.zip"]),
            &names(&["kept.zip"]),
            &[file("kept.zip"), file("dropped.zip")],
        );

        assert_eq!(removals, names(&["dropped.zip"]));
    }

    #[test]
    fn foreign_files_survive() {
        let removals = plan_stale_removals(
            &names(&["managed.zip"]),
            &names(&["managed.zip"]),
            &[
                file("managed.zip"),
                file("hand-placed.zip"),
                file("iris.properties.txt"),
            ],
        );

        assert!(removals.is_empty());
    }

    #[test]
    fn empty_new_set_removes_the_whole_manifest_but_nothing_else() {
        let removals = plan_stale_removals(
            &names(&["a.zip", "b.zip"]),
            &[],
            &[file("a.zip"), file("b.zip"), file("optionsshaders.txt")],
        );

        assert_eq!(removals, names(&["a.zip", "b.zip"]));
    }

    #[test]
    fn manifest_entry_already_deleted_by_hand_is_not_removed_again() {
        let removals = plan_stale_removals(
            &names(&["gone.zip", "still-here.zip"]),
            &[],
            &[file("still-here.zip")],
        );

        assert_eq!(removals, names(&["still-here.zip"]));
    }

    /// C1 had the opposite assertion here — a real directory was "the user's",
    /// because the launcher only ever linked files. Local packs can be unpacked
    /// folders, so the rule moved to where it always belonged: ownership comes
    /// from the manifest, and nothing else.
    #[test]
    fn manifest_entry_that_is_a_real_directory_is_removed() {
        let removals = plan_stale_removals(
            &names(&["pack"]),
            &[],
            &[PresentEntry {
                name: "pack".to_string(),
                is_real_dir: true,
            }],
        );

        assert_eq!(
            removals,
            names(&["pack"]),
            "a directory the manifest names is one we put there"
        );
    }

    #[test]
    fn a_real_directory_the_manifest_does_not_name_is_never_removed() {
        let removals = plan_stale_removals(
            &names(&["ours.zip"]),
            &[],
            &[
                PresentEntry {
                    name: "ours.zip".to_string(),
                    is_real_dir: false,
                },
                PresentEntry {
                    name: "user-unpacked-pack".to_string(),
                    is_real_dir: true,
                },
            ],
        );

        assert_eq!(
            removals,
            names(&["ours.zip"]),
            "the unlisted folder stays, the listed link goes"
        );
    }

    #[test]
    fn manifest_round_trips_and_rejects_traversal_entries() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let escaped = root_dir.join("escaped.zip");
        fs::write(&escaped, b"outside").expect("outside fixture should exist");
        fs::write(instance_dir.join("managed.zip"), b"managed").expect("managed link should exist");

        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["managed.zip"]),
            true,
        )
        .expect("first sync should succeed");

        let path = manifest_path(&root_dir, "resourcepacks");
        assert_eq!(read_manifest_files(&path), names(&["managed.zip"]));

        let poisoned = ManagedContentManifest {
            version: 1,
            category: "resourcepacks".to_string(),
            files: names(&["../escaped.zip", "managed.zip"]),
            origins: Default::default(),
            copied_dirs: None,
        };
        fs::write(
            &path,
            serde_json::to_string(&poisoned).expect("manifest should serialize"),
        )
        .expect("poisoned manifest should be written");

        assert_eq!(read_manifest_files(&path), names(&["managed.zip"]));

        sync_managed_content_dir(&root_dir, "resourcepacks", &instance_dir, &[], true)
            .expect("second sync should succeed");

        assert!(escaped.exists(), "traversal entry must not delete anything");
        assert!(!instance_dir.join("managed.zip").exists());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn unreadable_manifest_keeps_every_file() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "shaderpacks");
        fs::write(instance_dir.join("shader.zip"), b"shader").expect("shader fixture should exist");
        fs::write(instance_dir.join("iris.txt"), b"config").expect("config fixture should exist");
        let manifest = manifest_path(&root_dir, "shaderpacks");
        fs::create_dir_all(manifest.parent().expect("manifest has a parent"))
            .expect("manifest directory should be created");
        fs::write(&manifest, b"{ not json").expect("corrupt manifest should be written");

        let removals = sync_managed_content_dir(&root_dir, "shaderpacks", &instance_dir, &[], true)
            .expect("sync should succeed despite the corrupt manifest");

        assert!(removals.is_empty());
        assert!(instance_dir.join("shader.zip").exists());
        assert!(instance_dir.join("iris.txt").exists());
        assert_eq!(
            read_manifest_files(&manifest_path(&root_dir, "shaderpacks")),
            Vec::<String>::new()
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn sync_without_manifest_and_without_installs_writes_nothing() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "datapacks");
        fs::write(instance_dir.join("user.zip"), b"user").expect("user fixture should exist");

        let removals = sync_managed_content_dir(&root_dir, "datapacks", &instance_dir, &[], true)
            .expect("sync should succeed");

        assert!(removals.is_empty());
        assert!(instance_dir.join("user.zip").exists());
        assert!(
            !manifest_path(&root_dir, "datapacks").exists(),
            "an instance with no managed packs gets no manifest"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn sync_removes_only_the_dropped_link_on_disk() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        for name in ["kept.zip", "dropped.zip", "hand-placed.zip"] {
            fs::write(instance_dir.join(name), name.as_bytes()).expect("fixture should exist");
        }

        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["kept.zip", "dropped.zip"]),
            true,
        )
        .expect("first sync should succeed");

        let removals = sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["kept.zip"]),
            true,
        )
        .expect("second sync should succeed");

        assert_eq!(removals, names(&["dropped.zip"]));
        assert!(instance_dir.join("kept.zip").exists());
        assert!(!instance_dir.join("dropped.zip").exists());
        assert!(instance_dir.join("hand-placed.zip").exists());
        assert_eq!(
            read_manifest_files(&manifest_path(&root_dir, "resourcepacks")),
            names(&["kept.zip"])
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn failed_version_lookup_removes_nothing_and_keeps_the_old_names() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        for name in ["still-wanted.zip", "lookup-failed.zip"] {
            fs::write(instance_dir.join(name), name.as_bytes()).expect("fixture should exist");
        }

        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["still-wanted.zip", "lookup-failed.zip"]),
            true,
        )
        .expect("first sync should succeed");

        // Second launch: Modrinth answered for one entry and errored for the
        // other, so `installed` is missing a file that is still wanted.
        let removals = sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["still-wanted.zip"]),
            false,
        )
        .expect("second sync should succeed");

        assert!(removals.is_empty(), "a lookup failure must not delete");
        assert!(instance_dir.join("lookup-failed.zip").exists());
        assert_eq!(
            read_manifest_files(&manifest_path(&root_dir, "resourcepacks")),
            names(&["still-wanted.zip", "lookup-failed.zip"]),
            "the name we could not resolve stays in the manifest"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn carry_forward_merges_without_duplicates_and_keeps_order() {
        assert_eq!(
            carry_forward(&names(&["a.zip", "b.zip"]), &names(&["b.zip", "c.zip"])),
            names(&["a.zip", "b.zip", "c.zip"])
        );
    }

    // ── Linking under a name: the manifest rule on the way in (D1) ──────────

    /// The launch's question, asked the way the launch asks it: does the
    /// previous manifest on disk name this file?
    fn named_by_manifest(instance_root: &Path, category: &str, name: &str) -> bool {
        read_manifest_files(&manifest_path(instance_root, category))
            .iter()
            .any(|listed| listed == name)
    }

    /// `<cache>/<version id>/<file>`, the C5 layout the launch links from.
    fn cached_pack(root: &Path, version_id: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = root.join("cache").join(version_id);
        fs::create_dir_all(&dir).expect("cache entry dir should be created");
        let path = dir.join(name);
        fs::write(&path, bytes).expect("cache entry should be written");
        path
    }

    fn is_symlink(path: &Path) -> bool {
        fs::symlink_metadata(path)
            .expect("the name should be there")
            .file_type()
            .is_symlink()
    }

    /// The defect: a zip the user dropped into `resourcepacks/` under the name
    /// of a managed pack was deleted by the launch and replaced by a link into
    /// the cache, without a word.
    #[test]
    fn a_file_the_user_dropped_in_under_a_pack_name_survives_the_link() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let source = cached_pack(&root_dir, "AbCd1234", "Faithful 32x.zip", b"from modrinth");
        write_manifest(
            &manifest_path(&root_dir, "resourcepacks"),
            "resourcepacks",
            &names(&["Some other pack.zip"]),
        )
        .expect("previous manifest should be written");
        let target = instance_dir.join("Faithful 32x.zip");
        fs::write(&target, b"the user's own edit").expect("the user's file should exist");

        let outcome = link_content_file(
            &source,
            &target,
            named_by_manifest(&root_dir, "resourcepacks", "Faithful 32x.zip"),
        );

        assert_eq!(
            fs::read(&target).expect("the user's file should be readable"),
            b"the user's own edit",
            "byte for byte what the user put there"
        );
        assert!(
            !is_symlink(&target),
            "the user's file must not have become a link"
        );
        assert_eq!(
            outcome.expect("a foreign file is not an error"),
            ContentLinkOutcome::SkippedForeignFile
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// The normal update: the manifest names the file, our link from the last
    /// launch sits there, and it is relinked to the new version.
    #[test]
    fn a_name_the_manifest_grants_is_relinked_over_our_own_link() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let old = cached_pack(&root_dir, "OldVer01", "pack.zip", b"v1");
        let new = cached_pack(&root_dir, "NewVer02", "pack.zip", b"v2");
        let target = instance_dir.join("pack.zip");
        crate::instance_mods::create_file_link(&old, &target).expect("last launch's link");
        write_manifest(
            &manifest_path(&root_dir, "resourcepacks"),
            "resourcepacks",
            &names(&["pack.zip"]),
        )
        .expect("previous manifest should be written");

        let outcome = link_content_file(
            &new,
            &target,
            named_by_manifest(&root_dir, "resourcepacks", "pack.zip"),
        )
        .expect("our own link gives way");

        assert_eq!(outcome, ContentLinkOutcome::Linked);
        assert_eq!(fs::read_link(&target).expect("still a link"), new);
        assert_eq!(
            fs::read(&old).expect("the old cache entry is untouched"),
            b"v1"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// `fix/content-pack-cache-key`: a link whose cache entry was deleted is
    /// still a link and still gives way, manifest or not. With no manifest at
    /// all — the "I do not know what I put here" state — the answer is the same.
    #[test]
    fn a_dangling_link_is_replaced_even_without_a_manifest() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "shaderpacks");
        let gone = cached_pack(&root_dir, "OldVer01", "shader.zip", b"v1");
        let new = cached_pack(&root_dir, "NewVer02", "shader.zip", b"v2");
        let target = instance_dir.join("shader.zip");
        crate::instance_mods::create_file_link(&gone, &target).expect("last launch's link");
        fs::remove_file(&gone).expect("the cache entry should be deletable");
        assert!(!target.exists(), "precondition: the link dangles");

        let outcome = link_content_file(
            &new,
            &target,
            named_by_manifest(&root_dir, "shaderpacks", "shader.zip"),
        )
        .expect("a dangling link must not stop the relink");

        assert_eq!(outcome, ContentLinkOutcome::Linked);
        assert_eq!(fs::read(&target).expect("relinked"), b"v2");

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// The state the Windows fallback of `create_file_link` leaves: a hard
    /// link, i.e. a real file, indistinguishable from a user's file except by
    /// the manifest. Built here with `fs::hard_link` on the same filesystem as
    /// the cache; the symlink failure that leads to it on Windows is not.
    #[test]
    fn our_hard_link_is_replaced_when_the_manifest_names_it() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "datapacks");
        let old = cached_pack(&root_dir, "OldVer01", "data.zip", b"v1");
        let new = cached_pack(&root_dir, "NewVer02", "data.zip", b"v2");
        let target = instance_dir.join("data.zip");
        fs::hard_link(&old, &target).expect("the fallback's hard link");
        assert!(
            !is_symlink(&target),
            "precondition: a real file, not a link"
        );
        write_manifest(
            &manifest_path(&root_dir, "datapacks"),
            "datapacks",
            &names(&["data.zip"]),
        )
        .expect("previous manifest should be written");

        let outcome = link_content_file(
            &new,
            &target,
            named_by_manifest(&root_dir, "datapacks", "data.zip"),
        )
        .expect("our own hard link gives way");

        assert_eq!(outcome, ContentLinkOutcome::Linked);
        assert_eq!(fs::read(&target).expect("relinked"), b"v2");
        assert_eq!(
            fs::read(&old).expect("removing the hard link leaves the cache entry"),
            b"v1"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    // ── precheck-3: the manifest records where a Modrinth file came from ────

    fn origin(entry_id: &str, version_id: &str) -> Option<ManagedFileOrigin> {
        Some(ManagedFileOrigin {
            entry_id: entry_id.to_string(),
            version_id: version_id.to_string(),
        })
    }

    /// Link `name` from a cache entry and record it, the way the launch does.
    fn launch_once(
        root: &Path,
        category: &str,
        packs: &[(&str, Option<ManagedFileOrigin>)],
    ) -> super::ContentSyncOutcome {
        let instance_dir = root.join(category);
        let previous = previous_content(root, category, &instance_dir, None);
        let mut installed = InstalledContent::default();
        for (name, origin) in packs {
            let version = origin
                .as_ref()
                .map_or("local", |origin| origin.version_id.as_str());
            let source = cached_pack(root, version, name, version.as_bytes());
            fs::create_dir_all(&instance_dir).expect("category dir");
            link_content_file(&source, &instance_dir.join(name), previous.owns(name))
                .expect("link");
            installed.add(name, origin.clone());
        }
        sync_managed_content(root, category, &instance_dir, &previous, &installed, true)
            .expect("sync")
    }

    #[test]
    fn the_origin_of_each_modrinth_file_is_written_and_read_back() {
        let root_dir = unique_test_root();
        launch_once(
            &root_dir,
            "resourcepacks",
            &[
                ("Fresh.zip", origin("fresh-animations", "xN57JJts")),
                ("Local.zip", None),
            ],
        );

        let manifest = read_manifest(&manifest_path(&root_dir, "resourcepacks"))
            .expect("the manifest is readable");
        assert_eq!(manifest.version, 1, "the schema version did not move");
        assert_eq!(
            manifest.files,
            names(&["Fresh.zip", "Local.zip"]),
            "the file list is what an older launcher reads"
        );
        assert_eq!(
            manifest.origins.get("Fresh.zip"),
            origin("fresh-animations", "xN57JJts").as_ref()
        );
        assert!(!manifest.origins.contains_key("Local.zip"));

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn a_manifest_written_before_origins_still_reads() {
        let root_dir = unique_test_root();
        let path = manifest_path(&root_dir, "shaderpacks");
        fs::create_dir_all(path.parent().expect("parent")).expect("manifest dir");
        fs::write(
            &path,
            r#"{"version":1,"category":"shaderpacks","files":["a.zip"]}"#,
        )
        .expect("old manifest");

        let manifest = read_manifest(&path).expect("an old manifest is still a manifest");
        assert_eq!(manifest.files, names(&["a.zip"]));
        assert!(manifest.origins.is_empty());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    // ── D145: an update renames the file, the game's settings follow ────────

    #[test]
    fn an_update_that_renames_a_resource_pack_moves_it_in_options_txt() {
        let root_dir = unique_test_root();
        launch_once(
            &root_dir,
            "resourcepacks",
            &[
                (
                    "FreshAnimations_v1.10.4.zip",
                    origin("fresh-animations", "xN57JJts"),
                ),
                ("Other.zip", origin("other", "Other001")),
            ],
        );
        let options = root_dir.join("options.txt");
        fs::write(
            &options,
            "version:3465\nresourcePacks:[\"vanilla\",\"file/FreshAnimations_v1.10.4.zip\",\"file/Other.zip\"]\nincompatibleResourcePacks:[\"file/FreshAnimations_v1.10.4.zip\"]\nlang:it_it\n",
        )
        .expect("the game's options");

        let outcome = launch_once(
            &root_dir,
            "resourcepacks",
            &[
                (
                    "FreshAnimations_v1.11.0.zip",
                    origin("fresh-animations", "NewVer11"),
                ),
                ("Other.zip", origin("other", "Other001")),
            ],
        );
        assert_eq!(
            outcome.renamed,
            vec![(
                "FreshAnimations_v1.10.4.zip".to_string(),
                "FreshAnimations_v1.11.0.zip".to_string()
            )]
        );
        assert_eq!(outcome.removed, names(&["FreshAnimations_v1.10.4.zip"]));

        let rewritten =
            rewrite_renamed_pack_references(&root_dir, "resourcepacks", &outcome.renamed)
                .expect("rewrite");

        assert_eq!(rewritten, vec![options.clone()]);
        assert_eq!(
            fs::read_to_string(&options).expect("options"),
            "version:3465\nresourcePacks:[\"vanilla\",\"file/FreshAnimations_v1.11.0.zip\",\"file/Other.zip\"]\nincompatibleResourcePacks:[\"file/FreshAnimations_v1.11.0.zip\"]\nlang:it_it\n",
            "the position in the list is kept and every other line is untouched"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn a_renamed_shader_stays_selected_for_iris_oculus_and_optifine() {
        let root_dir = unique_test_root();
        fs::create_dir_all(root_dir.join("config")).expect("config dir");
        let iris = root_dir.join("config/iris.properties");
        let oculus = root_dir.join("config/oculus.properties");
        let optifine = root_dir.join("optionsshaders.txt");
        fs::write(
            &iris,
            "#This file stores configuration options for Iris\nenableShaders=true\nshaderPack=ComplementaryReimagined_r5.1.zip\n",
        )
        .expect("iris");
        fs::write(&oculus, "shaderPack = ComplementaryReimagined_r5.1.zip\r\n").expect("oculus");
        fs::write(&optifine, "shaderPack=Unrelated.zip\nantialiasingLevel=0\n").expect("optifine");

        let renamed = vec![(
            "ComplementaryReimagined_r5.1.zip".to_string(),
            "ComplementaryReimagined_r5.2.zip".to_string(),
        )];
        let rewritten =
            rewrite_renamed_pack_references(&root_dir, "shaderpacks", &renamed).expect("rewrite");

        assert_eq!(rewritten, vec![iris.clone(), oculus.clone()]);
        assert_eq!(
            fs::read_to_string(&iris).expect("iris"),
            "#This file stores configuration options for Iris\nenableShaders=true\nshaderPack=ComplementaryReimagined_r5.2.zip\n"
        );
        assert_eq!(
            fs::read_to_string(&oculus).expect("oculus"),
            "shaderPack=ComplementaryReimagined_r5.2.zip\r\n"
        );
        assert_eq!(
            fs::read_to_string(&optifine).expect("optifine"),
            "shaderPack=Unrelated.zip\nantialiasingLevel=0\n",
            "a shader that was not renamed is not touched"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// `Properties.load` has to read back exactly the new name, whatever it
    /// holds: blanks, separators, non-ASCII.
    #[test]
    fn a_shader_name_survives_the_properties_escaping() {
        for name in [
            "BSL_v8.2.09.zip",
            "Shader: the = one #1!.zip",
            " leading space.zip",
            "Sombras ñ 影.zip",
            r"back\slash.zip",
        ] {
            assert_eq!(unescape_property_value(&escape_property_value(name)), name);
        }
        assert_eq!(unescape_property_value(r"Sombras ñ.zip"), "Sombras ñ.zip");
    }

    #[test]
    fn a_local_pack_and_an_unchanged_name_are_no_rename() {
        let previous = super::PreviousContent {
            files: names(&["Same.zip", "Local.zip"]),
            origins: [(
                "Same.zip".to_string(),
                origin("same", "V1").expect("origin"),
            )]
            .into_iter()
            .collect(),
            manifest_exists: true,
            copied_dirs: None,
        };
        let mut installed = InstalledContent::default();
        installed.add("Same.zip", origin("same", "V2"));
        installed.add("Local2.zip", None);

        assert!(renamed_packs(&previous, &installed).is_empty());
    }

    // ── r2-aggiornamento-da-0.1.2-1: adopting an instance without manifest ──

    /// v0.1.2 wiped and relinked `resourcepacks/` without a manifest: a
    /// symlink into the cache, or — on Windows without Developer Mode — a hard
    /// link, i.e. a file with the cache's bytes. Both are adopted; a zip the
    /// user dropped in, even under a cached name with other bytes, is not.
    #[cfg(unix)]
    #[test]
    fn the_first_sync_without_manifest_adopts_what_only_the_launcher_made() {
        let root_dir = unique_test_root();
        let cache = root_dir.join("cache");
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");

        // The v0.1.x flat cache, and the C5 per-version one.
        fs::create_dir_all(&cache).expect("cache");
        fs::write(cache.join("FreshAnimations_v1.10.4.zip"), b"fresh").expect("flat");
        let versioned = cached_pack(&root_dir, "xN57JJts", "Nature X.zip", b"nature");
        cached_pack(&root_dir, "Other001", "Same name.zip", b"cache bytes");

        std::os::unix::fs::symlink(
            cache.join("FreshAnimations_v1.10.4.zip"),
            instance_dir.join("FreshAnimations_v1.10.4.zip"),
        )
        .expect("old symlink");
        fs::hard_link(&versioned, instance_dir.join("Nature X.zip")).expect("old hard link");
        fs::write(instance_dir.join("Same name.zip"), b"the user's").expect("user file");
        fs::write(instance_dir.join("hand-placed.zip"), b"user").expect("user file");

        let previous = previous_content(&root_dir, "resourcepacks", &instance_dir, Some(&cache));
        assert_eq!(
            previous.files,
            names(&["FreshAnimations_v1.10.4.zip", "Nature X.zip"])
        );
        assert!(!previous.manifest_exists);

        // The pack list no longer has either: both go, the user's stay.
        let outcome = sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &previous,
            &InstalledContent::default(),
            true,
        )
        .expect("sync");
        assert_eq!(
            outcome.removed,
            names(&["FreshAnimations_v1.10.4.zip", "Nature X.zip"])
        );
        assert_eq!(
            fs::read(instance_dir.join("Same name.zip")).expect("user file"),
            b"the user's"
        );
        assert!(instance_dir.join("hand-placed.zip").exists());
        assert!(
            manifest_path(&root_dir, "resourcepacks").exists(),
            "from now on the manifest answers"
        );

        // With a manifest on disk, adoption never runs again.
        std::os::unix::fs::symlink(&versioned, instance_dir.join("Nature X.zip"))
            .expect("a link made by hand later");
        let later = previous_content(&root_dir, "resourcepacks", &instance_dir, Some(&cache));
        assert!(later.files.is_empty());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    // ── D144: the old `<instance>/datapacks/` and the worlds ────────────────

    #[test]
    fn the_old_datapacks_folder_loses_only_what_the_launcher_put_there() {
        let root_dir = unique_test_root();
        let cache = root_dir.join("cache");
        fs::create_dir_all(&cache).expect("cache");
        let legacy = fixture_instance(&root_dir, "datapacks");
        fs::write(legacy.join("ours.zip"), b"ours").expect("ours");
        fs::write(legacy.join("open-loader-pack.zip"), b"user").expect("user");
        write_manifest(
            &manifest_path(&root_dir, "datapacks"),
            "datapacks",
            &names(&["ours.zip"]),
        )
        .expect("old manifest");

        let removed = retire_legacy_content_dir(&root_dir, "datapacks", &cache).expect("retire");

        assert_eq!(removed, names(&["ours.zip"]));
        assert!(legacy.join("open-loader-pack.zip").exists());
        assert!(!manifest_path(&root_dir, "datapacks").exists());

        // Nothing left to do on the next launch, and an emptied folder goes.
        fs::remove_file(legacy.join("open-loader-pack.zip")).expect("user cleans up");
        write_manifest(&manifest_path(&root_dir, "datapacks"), "datapacks", &[])
            .expect("empty manifest");
        retire_legacy_content_dir(&root_dir, "datapacks", &cache).expect("retire again");
        assert!(!legacy.exists());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[cfg(unix)]
    #[test]
    fn the_worlds_of_an_instance_are_its_world_folders_and_shared_links() {
        let root_dir = unique_test_root();
        let saves = root_dir.join("instance/saves");
        for world in ["A", "B"] {
            fs::create_dir_all(saves.join(world)).expect("world");
            fs::write(saves.join(world).join("level.dat"), b"nbt").expect("level.dat");
        }
        let shared = root_dir.join("worlds/Shared");
        fs::create_dir_all(&shared).expect("shared world");
        fs::write(shared.join("level.dat"), b"nbt").expect("level.dat");
        std::os::unix::fs::symlink(&shared, saves.join("Shared")).expect("link");
        // The same world twice, a dangling link, a folder that is no world.
        std::os::unix::fs::symlink(&shared, saves.join("Shared (2)")).expect("second link");
        std::os::unix::fs::symlink(root_dir.join("gone"), saves.join("Gone")).expect("dangling");
        fs::create_dir_all(saves.join("screenshots-by-mistake")).expect("stray");

        let worlds = instance_world_dirs(&root_dir.join("instance")).expect("worlds");

        assert_eq!(
            worlds,
            vec![saves.join("A"), saves.join("B"), saves.join("Shared")]
        );
        assert!(instance_world_dirs(&root_dir.join("never-ran"))
            .expect("no saves is no world")
            .is_empty());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// contentpack-10: a link to a folder that is gone is still a link, and it
    /// goes like any other. On Windows the kind of link is read from the link
    /// itself (`is_symlink_dir`); this exercises the unix side of the same
    /// promise — no Windows runner executes tests yet.
    #[test]
    #[cfg(target_family = "unix")]
    fn a_dangling_directory_link_is_removed_and_does_not_stop_the_launch() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let folder = root_dir.join("MyPack");
        fs::create_dir_all(&folder).expect("pack folder should be created");
        std::os::unix::fs::symlink(&folder, instance_dir.join("MyPack"))
            .expect("directory link should be created");
        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["MyPack"]),
            true,
        )
        .expect("first sync should succeed");
        fs::remove_dir_all(&folder).expect("the pack folder is deleted");

        let removed =
            sync_managed_content_dir(&root_dir, "resourcepacks", &instance_dir, &[], true)
                .expect("a dangling link does not stop the launch");

        assert_eq!(removed, names(&["MyPack"]));
        assert!(fs::symlink_metadata(instance_dir.join("MyPack")).is_err());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// contentpack-10: an entry that will not go is a warning, and it stays in
    /// the manifest so that the next launch, which can, removes it.
    #[test]
    #[cfg(target_family = "unix")]
    fn a_removal_that_fails_is_reported_and_retried_by_the_next_launch() {
        use std::os::unix::fs::PermissionsExt;

        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let cached = root_dir.join("old.zip");
        fs::write(&cached, b"pack").expect("cached pack should exist");
        std::os::unix::fs::symlink(&cached, instance_dir.join("old.zip"))
            .expect("link should be created");
        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["old.zip"]),
            true,
        )
        .expect("first sync should succeed");

        // A directory nobody may delete from: the unix stand-in for a file
        // Windows holds open.
        fs::set_permissions(&instance_dir, fs::Permissions::from_mode(0o555))
            .expect("the directory should become read-only");
        let previous = super::previous_from_manifest(&root_dir, "resourcepacks");
        let outcome = sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &previous,
            &InstalledContent::default(),
            true,
        );
        fs::set_permissions(&instance_dir, fs::Permissions::from_mode(0o755))
            .expect("the directory should become writable again");

        let outcome = outcome.expect("a failed removal does not stop the launch");
        assert!(outcome.removed.is_empty());
        assert_eq!(
            outcome
                .failed_removals
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            names(&["old.zip"])
        );
        assert_eq!(
            read_manifest_files(&manifest_path(&root_dir, "resourcepacks")),
            names(&["old.zip"]),
            "still ours, still listed"
        );

        let removed =
            sync_managed_content_dir(&root_dir, "resourcepacks", &instance_dir, &[], true)
                .expect("the next sync should succeed");
        assert_eq!(removed, names(&["old.zip"]));

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    // ── todo.md:1689, :1558, :1691 ───────────────────────────────────────────

    /// A folder the user put under the name of a pack we linked is theirs:
    /// kept, reported, and out of the manifest from then on.
    #[test]
    #[cfg(target_family = "unix")]
    fn a_users_folder_under_a_managed_name_is_kept_and_leaves_the_manifest() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let cached = root_dir.join("Pack.zip");
        fs::write(&cached, b"pack").expect("cached pack should exist");
        std::os::unix::fs::symlink(&cached, instance_dir.join("Pack.zip"))
            .expect("link should be created");
        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["Pack.zip"]),
            true,
        )
        .expect("first sync should succeed");

        // The user swaps our link for a folder of their own.
        fs::remove_file(instance_dir.join("Pack.zip")).expect("link removed");
        fs::create_dir_all(instance_dir.join("Pack.zip").join("assets")).expect("user folder");

        let previous = super::previous_from_manifest(&root_dir, "resourcepacks");
        let outcome = sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &previous,
            &InstalledContent::default(),
            true,
        )
        .expect("sync should succeed");

        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.kept_foreign_dirs, names(&["Pack.zip"]));
        assert!(instance_dir.join("Pack.zip").join("assets").is_dir());
        assert!(read_manifest_files(&manifest_path(&root_dir, "resourcepacks")).is_empty());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// The folder the launcher copied in itself is still its own to remove.
    #[test]
    fn our_own_copied_folder_is_removed_when_the_list_drops_it() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        fs::create_dir_all(instance_dir.join("Folder Pack").join("assets")).expect("copy");
        let mut installed = InstalledContent::default();
        installed.add_copied_directory("Folder Pack");
        sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &super::PreviousContent::default(),
            &installed,
            true,
        )
        .expect("first sync should succeed");
        let manifest = read_manifest(&manifest_path(&root_dir, "resourcepacks")).expect("manifest");
        assert_eq!(manifest.copied_dirs, Some(names(&["Folder Pack"])));

        let removed =
            sync_managed_content_dir(&root_dir, "resourcepacks", &instance_dir, &[], true)
                .expect("second sync should succeed");

        assert_eq!(removed, names(&["Folder Pack"]));
        assert!(!instance_dir.join("Folder Pack").exists());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// A manifest from before `copied_dirs` keeps its old word for a local
    /// name — a copied folder pack is still replaced, not skipped forever —
    /// but a folder under a Modrinth file's name was never the launcher's.
    #[test]
    fn a_manifest_before_copied_dirs_owns_a_local_folder_but_never_a_modrinth_one() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        fs::create_dir_all(instance_dir.join("Local Folder")).expect("old copy");
        fs::create_dir_all(instance_dir.join("Modrinth.zip")).expect("user folder");
        let path = manifest_path(&root_dir, "resourcepacks");
        fs::create_dir_all(path.parent().expect("manifest dir")).expect("manifest dir");
        fs::write(
            &path,
            r#"{"version":1,"category":"resourcepacks","files":["Local Folder","Modrinth.zip"],
               "origins":{"Modrinth.zip":{"entryId":"m","versionId":"V1"}}}"#,
        )
        .expect("old manifest");

        let previous = super::previous_from_manifest(&root_dir, "resourcepacks");
        assert_eq!(previous.copied_dirs, None);
        let outcome = sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &previous,
            &InstalledContent::default(),
            true,
        )
        .expect("sync should succeed");

        assert_eq!(outcome.removed, names(&["Local Folder"]));
        assert_eq!(outcome.kept_foreign_dirs, names(&["Modrinth.zip"]));
        assert!(instance_dir.join("Modrinth.zip").is_dir());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// todo.md:1558: a real folder under a Modrinth pack's name is skipped,
    /// not a launch that fails; and it is left exactly as it was.
    #[test]
    fn a_real_folder_under_a_pack_name_is_skipped_not_an_error() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        let source = root_dir.join("cache-Pack.zip");
        fs::write(&source, b"pack").expect("cached pack should exist");
        let target = instance_dir.join("Pack.zip");
        fs::create_dir_all(target.join("theirs")).expect("user folder");

        let outcome = link_content_file(&source, &target, false)
            .expect("a folder under the name is not an error");

        assert_eq!(outcome, ContentLinkOutcome::SkippedForeignDirectory);
        assert!(target.join("theirs").is_dir());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// todo.md:1691: an incomplete pass says what it would have removed.
    #[test]
    #[cfg(target_family = "unix")]
    fn an_incomplete_sync_reports_what_it_holds_back() {
        let root_dir = unique_test_root();
        let instance_dir = fixture_instance(&root_dir, "resourcepacks");
        for name in ["a.zip", "b.zip"] {
            let cached = root_dir.join(name);
            fs::write(&cached, b"pack").expect("cached pack should exist");
            std::os::unix::fs::symlink(&cached, instance_dir.join(name))
                .expect("link should be created");
        }
        sync_managed_content_dir(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &names(&["a.zip", "b.zip"]),
            true,
        )
        .expect("first sync should succeed");

        let previous = super::previous_from_manifest(&root_dir, "resourcepacks");
        let mut installed = InstalledContent::default();
        installed.add("a.zip", None);
        let outcome = sync_managed_content(
            &root_dir,
            "resourcepacks",
            &instance_dir,
            &previous,
            &installed,
            false,
        )
        .expect("sync should succeed");

        assert_eq!(outcome.frozen, names(&["b.zip"]));
        assert!(outcome.removed.is_empty());
        assert!(fs::symlink_metadata(instance_dir.join("b.zip")).is_ok());

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
}
