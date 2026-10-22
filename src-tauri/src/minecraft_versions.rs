//! The Minecraft versions the launch panel's dropdown offers.
//!
//! It used to be one request to Mojang's manifest, awaited before anything
//! else at startup, with a client without a timeout; when it failed the
//! dropdown fell back to four fixed versions, so a version already on disk
//! could not be chosen again without a network (stato-frontend-7,
//! r2-primo-avvio-ambiente-degradato-2). Now:
//!
//! - the request has a 20 s timeout, and the frontend does not wait for it;
//! - every manifest fetched is saved under `cache/minecraft/`, and is what
//!   the list is built from when Mojang can't be asked;
//! - the versions already downloaded (`cache/minecraft/<id>/version.json`)
//!   are always in the list;
//! - with the active account playing offline Mojang is not asked at all
//!   (D140);
//! - the answer says where the list came from, so the interface can say
//!   when it may be missing versions.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::atomic_write::write_atomically;
use crate::launcher_paths::LauncherPaths;

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// The whole request, body included: the manifest is about 200 KB.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The last manifest fetched, next to the version folders it describes.
const SAVED_MANIFEST_FILENAME: &str = "version_manifest_v2.json";

#[derive(Deserialize)]
struct Manifest {
    versions: Vec<VersionEntry>,
}

/// One version as the manifest and a `version.json` both describe it.
#[derive(Debug, Clone, Deserialize)]
struct VersionEntry {
    id: String,
    #[serde(rename = "type")]
    version_type: String,
    /// ISO 8601 with the same offset everywhere Mojang writes it, so it
    /// orders as a string.
    #[serde(rename = "releaseTime", default)]
    release_time: String,
}

/// Where the list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VersionListSource {
    /// Mojang's manifest, fetched now.
    Mojang,
    /// The manifest saved the last time Mojang answered.
    Saved,
    /// Nothing but the versions already downloaded.
    Installed,
}

/// Payload returned to the frontend with releases and releases+snapshots lists.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftVersionsPayload {
    pub releases: Vec<String>,
    pub with_snapshots: Vec<String>,
    pub source: VersionListSource,
    /// Mojang was not asked because the active account plays offline.
    pub plays_offline: bool,
}

fn saved_manifest_path(launcher_paths: &LauncherPaths) -> PathBuf {
    launcher_paths.mc_cache_dir().join(SAVED_MANIFEST_FILENAME)
}

fn parse_manifest(bytes: &[u8]) -> Result<Vec<VersionEntry>> {
    Ok(serde_json::from_slice::<Manifest>(bytes)
        .context("failed to parse Mojang's version manifest")?
        .versions)
}

fn read_saved_manifest(launcher_paths: &LauncherPaths) -> Option<Vec<VersionEntry>> {
    let path = saved_manifest_path(launcher_paths);
    let bytes = fs::read(&path).ok()?;
    match parse_manifest(&bytes) {
        Ok(versions) => Some(versions),
        Err(error) => {
            eprintln!(
                "[Versions] Ignoring the saved manifest {}: {error:#}",
                path.display()
            );
            None
        }
    }
}

/// The versions whose `version.json` is in the cache, read from the files
/// themselves. A folder whose file does not name the folder (`libraries`,
/// `assets`, anything half-written) is not a version.
fn installed_versions(launcher_paths: &LauncherPaths) -> Vec<VersionEntry> {
    let Ok(entries) = fs::read_dir(launcher_paths.mc_cache_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let folder = entry.file_name().to_str()?.to_string();
            let bytes = fs::read(entry.path().join("version.json")).ok()?;
            let version = serde_json::from_slice::<VersionEntry>(&bytes).ok()?;
            (version.id == folder).then_some(version)
        })
        .collect()
}

/// The manifest's versions plus the installed ones it does not list, newest
/// first, and the two lists the dropdown shows.
fn build_payload(
    listed: Vec<VersionEntry>,
    installed: Vec<VersionEntry>,
    source: VersionListSource,
    plays_offline: bool,
) -> MinecraftVersionsPayload {
    let mut by_id: HashMap<String, VersionEntry> = HashMap::new();
    let mut versions = Vec::new();
    for version in listed.into_iter().chain(installed) {
        if by_id.contains_key(&version.id) {
            continue;
        }
        by_id.insert(version.id.clone(), version.clone());
        versions.push(version);
    }
    // Stable: the manifest's own order stands between equal times.
    versions.sort_by(|left, right| right.release_time.cmp(&left.release_time));

    let releases = versions
        .iter()
        .filter(|version| version.version_type == "release")
        .map(|version| version.id.clone())
        .collect();
    let with_snapshots = versions
        .iter()
        .filter(|version| version.version_type == "release" || version.version_type == "snapshot")
        .map(|version| version.id.clone())
        .collect();

    MinecraftVersionsPayload {
        releases,
        with_snapshots,
        source,
        plays_offline,
    }
}

/// The dropdown's versions. `fetch_manifest` is asked only when the active
/// account plays online; whatever it answers, the list falls back to the
/// saved manifest and the installed versions instead of failing.
async fn load_minecraft_versions<F, Fut>(
    launcher_paths: &LauncherPaths,
    plays_offline: bool,
    fetch_manifest: F,
) -> MinecraftVersionsPayload
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let installed = installed_versions(launcher_paths);

    if !plays_offline {
        let fetched = fetch_manifest()
            .await
            .and_then(|bytes| parse_manifest(&bytes).map(|versions| (bytes, versions)));
        match fetched {
            Ok((bytes, versions)) => {
                let path = saved_manifest_path(launcher_paths);
                let saved = fs::create_dir_all(launcher_paths.mc_cache_dir())
                    .and_then(|()| write_atomically(&path, &bytes));
                if let Err(error) = saved {
                    eprintln!(
                        "[Versions] Could not save the manifest to {}: {error}",
                        path.display()
                    );
                }
                return build_payload(versions, installed, VersionListSource::Mojang, false);
            }
            Err(error) => {
                eprintln!("[Versions] Mojang's version list is unavailable, using what is on disk: {error:#}");
            }
        }
    }

    match read_saved_manifest(launcher_paths) {
        Some(versions) => {
            build_payload(versions, installed, VersionListSource::Saved, plays_offline)
        }
        None => build_payload(
            Vec::new(),
            installed,
            VersionListSource::Installed,
            plays_offline,
        ),
    }
}

async fn fetch_manifest_bytes() -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(MANIFEST_TIMEOUT)
        .build()
        .context("failed to build the HTTP client")?;
    let bytes = client
        .get(VERSION_MANIFEST_URL)
        .send()
        .await
        .context("failed to fetch Mojang's version manifest")?
        .error_for_status()
        .context("Mojang's version manifest request failed")?
        .bytes()
        .await
        .context("failed to read Mojang's version manifest")?;
    Ok(bytes.to_vec())
}

/// Returns Minecraft versions for the UI dropdown, split into releases and
/// releases+snapshots. It does not fail: without Mojang the list comes from
/// disk, and `source` says so.
#[tauri::command]
pub async fn fetch_minecraft_versions_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<MinecraftVersionsPayload, String> {
    let plays_offline = crate::app_shell::active_account_plays_offline(&launcher_paths)
        .map_err(|error| error.to_string())?;
    Ok(load_minecraft_versions(&launcher_paths, plays_offline, fetch_manifest_bytes).await)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::anyhow;

    use super::*;

    fn scratch_paths(label: &str) -> LauncherPaths {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let paths = LauncherPaths::new(
            env::temp_dir().join(format!("cubic-minecraft-versions-{label}-{stamp}")),
        );
        fs::create_dir_all(paths.mc_cache_dir()).expect("cache dir should exist");
        paths
    }

    fn install(paths: &LauncherPaths, id: &str, version_type: &str, release_time: &str) {
        let dir = paths.mc_version_dir(id);
        fs::create_dir_all(&dir).expect("version dir should exist");
        fs::write(
            dir.join("version.json"),
            format!(r#"{{"id":"{id}","type":"{version_type}","releaseTime":"{release_time}","mainClass":"x"}}"#),
        )
        .expect("version.json should write");
    }

    fn manifest(entries: &[(&str, &str, &str)]) -> Vec<u8> {
        let versions: Vec<String> = entries
            .iter()
            .map(|(id, version_type, release_time)| {
                format!(r#"{{"id":"{id}","type":"{version_type}","url":"https://x/{id}.json","releaseTime":"{release_time}"}}"#)
            })
            .collect();
        format!(r#"{{"latest":{{}},"versions":[{}]}}"#, versions.join(",")).into_bytes()
    }

    fn cleanup(paths: &LauncherPaths) {
        let _ = fs::remove_dir_all(paths.root_dir());
    }

    fn saved_manifest_exists(paths: &LauncherPaths) -> bool {
        Path::new(&saved_manifest_path(paths)).exists()
    }

    #[tokio::test]
    async fn a_fetched_manifest_is_saved_and_merged_with_the_installed_versions() {
        let paths = scratch_paths("fetched");
        install(&paths, "1.20.1", "release", "2023-06-12T13:25:51+00:00");
        // Not in the manifest below: it must stay choosable.
        install(&paths, "b1.7.3", "old_beta", "2011-07-07T22:00:00+00:00");
        fs::create_dir_all(paths.mc_libraries_dir()).expect("libraries dir");

        let body = manifest(&[
            ("24w14a", "snapshot", "2024-04-03T12:00:00+00:00"),
            ("1.20.4", "release", "2023-12-07T12:56:20+00:00"),
            ("1.20.1", "release", "2023-06-12T13:25:51+00:00"),
        ]);
        let payload = load_minecraft_versions(&paths, false, || async { Ok(body.clone()) }).await;

        assert_eq!(payload.source, VersionListSource::Mojang);
        assert!(!payload.plays_offline);
        assert_eq!(payload.releases, vec!["1.20.4", "1.20.1"]);
        assert_eq!(payload.with_snapshots, vec!["24w14a", "1.20.4", "1.20.1"]);
        assert!(saved_manifest_exists(&paths));
        cleanup(&paths);
    }

    #[tokio::test]
    async fn without_mojang_the_saved_manifest_and_the_installed_versions_answer() {
        let paths = scratch_paths("saved");
        fs::write(
            saved_manifest_path(&paths),
            manifest(&[
                ("1.21.1", "release", "2024-08-08T12:24:45+00:00"),
                ("1.20.1", "release", "2023-06-12T13:25:51+00:00"),
            ]),
        )
        .expect("saved manifest should write");
        // Newer than the saved manifest: downloaded after it was fetched.
        install(&paths, "26.3", "release", "2026-09-15T11:23:02+00:00");

        let payload =
            load_minecraft_versions(&paths, false, || async { Err(anyhow!("no route to host")) })
                .await;

        assert_eq!(payload.source, VersionListSource::Saved);
        assert_eq!(payload.releases, vec!["26.3", "1.21.1", "1.20.1"]);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn with_nothing_saved_the_installed_versions_are_the_list() {
        let paths = scratch_paths("installed");
        install(&paths, "1.20.1", "release", "2023-06-12T13:25:51+00:00");
        install(&paths, "26.3", "release", "2026-09-15T11:23:02+00:00");
        // A folder whose version.json names another version is not one.
        let stray = paths.mc_version_dir("not-a-version");
        fs::create_dir_all(&stray).expect("stray dir");
        fs::write(
            stray.join("version.json"),
            r#"{"id":"1.8.9","type":"release","releaseTime":"2015-12-03T09:24:39+00:00"}"#,
        )
        .expect("stray version.json");

        let payload =
            load_minecraft_versions(&paths, false, || async { Err(anyhow!("timed out")) }).await;

        assert_eq!(payload.source, VersionListSource::Installed);
        assert_eq!(payload.releases, vec!["26.3", "1.20.1"]);
        assert_eq!(payload.with_snapshots, vec!["26.3", "1.20.1"]);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn an_account_that_plays_offline_never_asks_mojang() {
        let paths = scratch_paths("offline");
        install(&paths, "1.20.1", "release", "2023-06-12T13:25:51+00:00");
        let asked = Cell::new(false);

        let payload = load_minecraft_versions(&paths, true, || {
            asked.set(true);
            async {
                Ok(manifest(&[(
                    "1.21.1",
                    "release",
                    "2024-08-08T12:24:45+00:00",
                )]))
            }
        })
        .await;

        assert!(!asked.get(), "Mojang must not be asked");
        assert!(payload.plays_offline);
        assert_eq!(payload.source, VersionListSource::Installed);
        assert_eq!(payload.releases, vec!["1.20.1"]);
        assert!(!saved_manifest_exists(&paths));
        cleanup(&paths);
    }

    #[tokio::test]
    async fn a_manifest_that_does_not_parse_is_neither_used_nor_saved() {
        let paths = scratch_paths("garbage");
        install(&paths, "1.20.1", "release", "2023-06-12T13:25:51+00:00");

        let payload = load_minecraft_versions(&paths, false, || async {
            Ok(b"<html>captive portal</html>".to_vec())
        })
        .await;

        assert_eq!(payload.source, VersionListSource::Installed);
        assert_eq!(payload.releases, vec!["1.20.1"]);
        assert!(!saved_manifest_exists(&paths));
        cleanup(&paths);
    }
}
