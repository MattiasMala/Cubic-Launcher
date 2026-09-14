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

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::path_safety::validate_path_component;

/// Manifest schema version. A manifest written by a newer launcher is treated
/// as unreadable, which means "remove nothing".
const MANIFEST_VERSION: u32 = 1;

/// What the launcher linked into one instance subdirectory on the last launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedContentManifest {
    pub version: u32,
    pub category: String,
    pub files: Vec<String>,
}

/// One entry observed inside the instance subdirectory at sync time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentEntry {
    pub name: String,
    /// `true` for a real directory: never something we linked, so never ours.
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

/// Decide which files to remove, given the previous manifest, the set we just
/// installed, and what is actually in the directory.
///
/// A name is removed only when all three hold: it was in the previous manifest,
/// it is not in the new set, and it is on disk as a file or symlink. Anything
/// else — unknown files, entries the user already deleted, entries that are now
/// real directories — is left alone.
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
        let removable = present
            .iter()
            .any(|entry| entry.name == *name && !entry.is_real_dir);
        if removable {
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
    let raw = match fs::read_to_string(manifest_path) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let manifest: ManagedContentManifest = match serde_json::from_str(&raw) {
        Ok(manifest) => manifest,
        Err(_) => return Vec::new(),
    };
    if manifest.version != MANIFEST_VERSION {
        return Vec::new();
    }

    manifest
        .files
        .into_iter()
        .filter(|name| validate_path_component(name).is_ok())
        .collect()
}

fn write_manifest(manifest_path: &Path, category: &str, files: &[String]) -> Result<()> {
    let manifest = ManagedContentManifest {
        version: MANIFEST_VERSION,
        category: category.to_string(),
        files: files.to_vec(),
    };
    let body = serde_json::to_string_pretty(&manifest)
        .context("failed to serialize managed content manifest")?;

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(manifest_path, body).with_context(|| {
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
pub fn sync_managed_content_dir(
    instance_root: &Path,
    category: &str,
    instance_dir: &Path,
    installed: &[String],
    installed_is_complete: bool,
) -> Result<Vec<String>> {
    let manifest_path = manifest_path(instance_root, category);
    let previous = read_manifest_files(&manifest_path);

    if installed.is_empty() && !manifest_path.exists() {
        return Ok(Vec::new());
    }

    if !installed_is_complete {
        write_manifest(
            &manifest_path,
            category,
            &carry_forward(&previous, installed),
        )?;
        return Ok(Vec::new());
    }

    let present = present_entries(instance_dir)?;
    let removals = plan_stale_removals(&previous, installed, &present);

    for name in &removals {
        let path = instance_dir.join(name);
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale content pack {}", path.display()))?;
    }

    write_manifest(&manifest_path, category, installed)?;

    Ok(removals)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        carry_forward, manifest_path, plan_stale_removals, read_manifest_files,
        sync_managed_content_dir, ManagedContentManifest, PresentEntry,
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

    #[test]
    fn manifest_entry_that_is_now_a_real_directory_is_left_alone() {
        let removals = plan_stale_removals(
            &names(&["pack"]),
            &[],
            &[PresentEntry {
                name: "pack".to_string(),
                is_real_dir: true,
            }],
        );

        assert!(
            removals.is_empty(),
            "we only ever link files, so a directory is the user's"
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
}
