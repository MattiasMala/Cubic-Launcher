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
/// directory the manifest does not name is still never touched.
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
        remove_managed_entry(&path)
            .with_context(|| format!("failed to remove stale content pack {}", path.display()))?;
    }

    write_manifest(&manifest_path, category, installed)?;

    Ok(removals)
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

/// On Windows a symlink to a directory is removed with `remove_dir`, not
/// `remove_file`; `Path::is_dir` follows the link and tells the two apart.
#[cfg(target_family = "windows")]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// On unix `unlink` removes a symlink regardless of what it points at.
#[cfg(target_family = "unix")]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

/// What [`link_content_file`] did under one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLinkOutcome {
    Linked,
    /// Nothing was done: a real file sits under that name and the previous
    /// manifest does not claim it, so it is not ours to replace.
    SkippedForeignFile,
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
    if is_real_file(target) {
        if !target_is_ours {
            return Ok(ContentLinkOutcome::SkippedForeignFile);
        }
        fs::remove_file(target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
    }

    crate::instance_mods::create_file_link(source, target)?;
    Ok(ContentLinkOutcome::Linked)
}

/// A regular file, and not a link pointing at one: the shape the launcher
/// cannot tell from a user's file without the manifest.
pub fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        carry_forward, link_content_file, manifest_path, plan_stale_removals,
        read_manifest_files, sync_managed_content_dir, write_manifest, ContentLinkOutcome,
        ManagedContentManifest, PresentEntry,
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
        assert!(!is_symlink(&target), "the user's file must not have become a link");
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
        assert_eq!(fs::read(&old).expect("the old cache entry is untouched"), b"v1");

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
        assert!(!is_symlink(&target), "precondition: a real file, not a link");
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
}
