use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tauri::{Emitter, State};
use tokio::task::JoinSet;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::contained_join;
use crate::process_streaming::{ProcessLogEvent, ProcessLogStream, MINECRAFT_LOG_EVENT};

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const MC_ASSETS_BASE_URL: &str = "https://resources.download.minecraft.net";

// ── Public output ─────────────────────────────────────────────────────────────

/// Everything the launch pipeline needs from the Minecraft version.
#[derive(Debug, Clone)]
pub struct MinecraftVersionData {
    /// Main class for vanilla (e.g. `net.minecraft.client.main.Main`).
    pub main_class: String,
    /// Shared cached client JAR path.
    pub client_jar_path: PathBuf,
    /// All MC core library JARs (for classpath).
    pub library_paths: Vec<PathBuf>,
    /// Native JARs that must be extracted to the instance natives directory.
    pub native_paths: Vec<PathBuf>,
    /// Shared assets root (`cache/minecraft/assets/`).
    pub assets_dir: PathBuf,
    /// Asset index ID (e.g. `"17"` for MC 1.21.x).
    pub asset_index_id: String,
    /// Standard game arguments with `${placeholder}` tokens still present.
    pub game_arguments: Vec<String>,
    /// JVM arguments (filtered by the current OS), with `${placeholder}` tokens.
    pub jvm_arguments: Vec<String>,
}

// ── Mojang API serde structs ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct VersionManifest {
    versions: Vec<VersionManifestEntry>,
}

#[derive(Deserialize)]
struct VersionManifestEntry {
    id: String,
    url: String,
    #[serde(rename = "type")]
    version_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionJson {
    main_class: String,
    downloads: VersionDownloads,
    libraries: Vec<McLibrary>,
    #[serde(default)]
    arguments: Option<VersionArguments>,
    /// Present in old-format versions (pre-1.13).
    #[serde(default)]
    minecraft_arguments: Option<String>,
    asset_index: AssetIndexRef,
    /// Asset index short ID (e.g. `"17"`).
    assets: String,
}

#[derive(Deserialize)]
struct VersionDownloads {
    client: DownloadArtifact,
}

#[derive(Deserialize)]
struct DownloadArtifact {
    url: String,
    sha1: String,
}

#[derive(Deserialize, Default)]
struct VersionArguments {
    #[serde(default)]
    game: Vec<ArgEntry>,
    #[serde(default)]
    jvm: Vec<ArgEntry>,
}

/// An argument entry is either a plain string or a conditional object.
#[derive(Deserialize)]
#[serde(untagged)]
enum ArgEntry {
    Simple(String),
    Conditional { rules: Vec<OsRule>, value: ArgValue },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArgValue {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct OsRule {
    action: String,
    #[serde(default)]
    os: Option<OsFilter>,
}

#[derive(Deserialize)]
struct OsFilter {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct McLibrary {
    #[serde(default)]
    downloads: Option<McLibraryDownloads>,
    #[serde(default)]
    rules: Option<Vec<OsRule>>,
    #[serde(default)]
    natives: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct McLibraryDownloads {
    #[serde(default)]
    artifact: Option<McArtifact>,
    #[serde(default)]
    classifiers: Option<HashMap<String, McArtifact>>,
}

#[derive(Deserialize, Clone)]
struct McArtifact {
    url: String,
    sha1: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct AssetIndexRef {
    id: String,
    url: String,
    sha1: String,
}

#[derive(Deserialize)]
struct AssetIndexJson {
    objects: HashMap<String, AssetObject>,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
}

// ── OS detection ──────────────────────────────────────────────────────────────

fn current_os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

fn current_os_natives_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

/// Returns `true` if the library's `rules` list allows it on the current OS.
/// When there are no rules, the library is always allowed.
fn library_allowed_on_current_os(rules: &[OsRule]) -> bool {
    if rules.is_empty() {
        return true;
    }

    let os = current_os_name();
    let mut allowed = false;

    for rule in rules {
        let matches_os = rule
            .os
            .as_ref()
            .and_then(|f| f.name.as_deref())
            .map_or(true, |name| name == os);

        if matches_os {
            allowed = rule.action == "allow";
        }
    }

    allowed
}

/// Returns `true` if the conditional argument's rules allow it on the current OS.
fn arg_rule_passes(rules: &[OsRule]) -> bool {
    library_allowed_on_current_os(rules)
}

// ── SHA-1 verification ────────────────────────────────────────────────────────

fn sha1_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read file for SHA1 check: {}", path.display()))?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn file_matches_sha1(path: &Path, expected: &str) -> bool {
    if !path.exists() {
        return false;
    }
    sha1_of_file(path)
        .map(|actual| actual.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

// ── Download helper ───────────────────────────────────────────────────────────

pub(crate) async fn download_file_verified(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha1: &str,
) -> Result<()> {
    if file_matches_sha1(dest, expected_sha1) {
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error downloading {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {url}"))?;

    std::fs::write(dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;

    // Verify after write
    let actual = {
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    if !actual.eq_ignore_ascii_case(expected_sha1) {
        std::fs::remove_file(dest).ok();
        bail!(
            "SHA1 mismatch for {}: expected {expected_sha1}, got {actual}",
            dest.display()
        );
    }

    Ok(())
}

/// Download without SHA1 verification (used for asset index which we re-verify separately).
async fn download_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error downloading {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

// ── Version list ──────────────────────────────────────────────────────────────

/// Returns all Minecraft version IDs from the Mojang manifest, newest first.
pub async fn fetch_all_versions(http_client: &reqwest::Client) -> Result<Vec<String>> {
    let manifest: VersionManifest = http_client
        .get(VERSION_MANIFEST_URL)
        .send()
        .await
        .context("failed to fetch Minecraft version manifest")?
        .error_for_status()
        .context("Minecraft version manifest returned HTTP error")?
        .json()
        .await
        .context("failed to deserialize Minecraft version manifest")?;

    Ok(manifest.versions.into_iter().map(|v| v.id).collect())
}

/// Returns `true` if `client.jar` for the given version is already in the cache.
pub fn is_version_cached(launcher_paths: &LauncherPaths, version: &str) -> bool {
    launcher_paths
        .mc_version_dir(version)
        .join("client.jar")
        .exists()
}

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Payload returned to the frontend with releases and releases+snapshots lists.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftVersionsPayload {
    pub releases: Vec<String>,
    pub with_snapshots: Vec<String>,
}

/// Returns Minecraft versions for the UI dropdown, split into releases and releases+snapshots.
#[tauri::command]
pub async fn fetch_minecraft_versions_command() -> Result<MinecraftVersionsPayload, String> {
    let http_client = reqwest::Client::new();
    let manifest: VersionManifest = http_client
        .get(VERSION_MANIFEST_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let releases: Vec<String> = manifest
        .versions
        .iter()
        .filter(|v| v.version_type == "release")
        .map(|v| v.id.clone())
        .collect();

    let with_snapshots: Vec<String> = manifest
        .versions
        .iter()
        .filter(|v| v.version_type == "release" || v.version_type == "snapshot")
        .map(|v| v.id.clone())
        .collect();

    Ok(MinecraftVersionsPayload {
        releases,
        with_snapshots,
    })
}

/// Spawns a background task that pre-downloads every missing Minecraft release version.
/// Returns immediately; the download runs concurrently with normal launcher use.
#[tauri::command]
pub fn start_minecraft_predownload_command(
    app_handle: tauri::AppHandle,
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<(), String> {
    let launcher_paths = launcher_paths.inner().clone();

    tauri::async_runtime::spawn(async move {
        let http_client = reqwest::Client::new();

        let versions = match fetch_all_versions(&http_client).await {
            Ok(v) => v,
            Err(e) => {
                let _ = app_handle.emit(
                    MINECRAFT_LOG_EVENT,
                    ProcessLogEvent {
                        stream: ProcessLogStream::Stderr,
                        line: format!("[MC Sync] Failed to fetch version list: {e}"),
                    },
                );
                return;
            }
        };

        let missing: Vec<&str> = versions
            .iter()
            .map(|v| v.as_str())
            .filter(|v| !is_version_cached(&launcher_paths, v))
            .collect();

        if missing.is_empty() {
            return;
        }

        let total = missing.len();
        let _ = app_handle.emit(
            MINECRAFT_LOG_EVENT,
            ProcessLogEvent {
                stream: ProcessLogStream::Stdout,
                line: format!("[MC Sync] {total} version(s) to pre-download."),
            },
        );

        let mut completed = 0usize;
        for version in missing {
            let _ = app_handle.emit(
                MINECRAFT_LOG_EVENT,
                ProcessLogEvent {
                    stream: ProcessLogStream::Stdout,
                    line: format!(
                        "[MC Sync] Downloading {version} ({}/{total})…",
                        completed + 1
                    ),
                },
            );

            match ensure_minecraft_version(&http_client, version, &launcher_paths, |_, _| {}).await
            {
                Ok(_) => completed += 1,
                Err(e) => {
                    let _ = app_handle.emit(
                        MINECRAFT_LOG_EVENT,
                        ProcessLogEvent {
                            stream: ProcessLogStream::Stderr,
                            line: format!("[MC Sync] Failed to download {version}: {e}"),
                        },
                    );
                }
            }
        }

        let _ = app_handle.emit(
            MINECRAFT_LOG_EVENT,
            ProcessLogEvent {
                stream: ProcessLogStream::Stdout,
                line: format!("[MC Sync] Pre-download done. {completed}/{total} succeeded."),
            },
        );
    });

    Ok(())
}

// ── Main public function ──────────────────────────────────────────────────────

/// Ensures all Minecraft files for `minecraft_version` are present in the shared
/// cache (`cache/minecraft/`), downloading anything missing.
///
/// Returns a `MinecraftVersionData` describing all paths and arguments needed for
/// a launch. Safe to call on every launch — skips files whose SHA1 already matches.
pub async fn ensure_minecraft_version(
    http_client: &reqwest::Client,
    minecraft_version: &str,
    launcher_paths: &LauncherPaths,
    on_progress: impl Fn(&str, &str),
) -> Result<MinecraftVersionData> {
    let version_dir = launcher_paths.mc_version_dir(minecraft_version);
    let libraries_dir = launcher_paths.mc_libraries_dir();
    let assets_dir = launcher_paths.mc_assets_dir();

    std::fs::create_dir_all(&version_dir)
        .with_context(|| format!("failed to create {}", version_dir.display()))?;
    std::fs::create_dir_all(&libraries_dir)
        .with_context(|| format!("failed to create {}", libraries_dir.display()))?;

    // 1. Fetch version manifest and find the URL for this version.
    on_progress("Fetch version manifest", minecraft_version);
    let version_url = fetch_version_url(http_client, minecraft_version).await?;

    // 2. Fetch (or load from cache) the version JSON.
    on_progress("Fetch version metadata", minecraft_version);
    let version_json = fetch_version_json(http_client, &version_url, &version_dir).await?;

    // 3. Ensure client JAR.
    on_progress("Ensure client JAR", minecraft_version);
    let client_jar_path = ensure_client_jar(http_client, &version_json, &version_dir).await?;

    // 4. Ensure libraries.
    on_progress("Ensure libraries", minecraft_version);
    let (library_paths, native_paths) =
        ensure_libraries(http_client, &version_json, &libraries_dir, &on_progress).await?;

    // 5. Ensure assets.
    on_progress("Ensure assets", minecraft_version);
    ensure_assets(http_client, &version_json, &assets_dir, &on_progress).await?;

    // 6. Extract arguments.
    let (game_arguments, jvm_arguments) = extract_arguments(&version_json);

    Ok(MinecraftVersionData {
        main_class: version_json.main_class,
        client_jar_path,
        library_paths,
        native_paths,
        assets_dir,
        asset_index_id: version_json.assets,
        game_arguments,
        jvm_arguments,
    })
}

// ── Internal steps ────────────────────────────────────────────────────────────

async fn fetch_version_url(client: &reqwest::Client, minecraft_version: &str) -> Result<String> {
    let manifest: VersionManifest = client
        .get(VERSION_MANIFEST_URL)
        .send()
        .await
        .context("failed to fetch Minecraft version manifest")?
        .error_for_status()
        .context("Minecraft version manifest returned HTTP error")?
        .json()
        .await
        .context("failed to deserialize Minecraft version manifest")?;

    manifest
        .versions
        .into_iter()
        .find(|v| v.id == minecraft_version)
        .map(|v| v.url)
        .with_context(|| {
            format!(
                "Minecraft version '{}' not found in the version manifest",
                minecraft_version
            )
        })
}

async fn fetch_version_json(
    client: &reqwest::Client,
    version_url: &str,
    version_dir: &Path,
) -> Result<VersionJson> {
    let cached_path = version_dir.join("version.json");

    // Use the cached copy if it already exists.
    if cached_path.exists() {
        let contents = std::fs::read_to_string(&cached_path)
            .with_context(|| format!("failed to read {}", cached_path.display()))?;
        return serde_json::from_str::<VersionJson>(&contents)
            .with_context(|| format!("failed to parse cached {}", cached_path.display()));
    }

    let bytes = client
        .get(version_url)
        .send()
        .await
        .with_context(|| format!("failed to fetch version JSON from {version_url}"))?
        .error_for_status()
        .context("Minecraft version JSON returned HTTP error")?
        .bytes()
        .await
        .context("failed to read version JSON body")?;

    std::fs::write(&cached_path, &bytes)
        .with_context(|| format!("failed to cache version JSON at {}", cached_path.display()))?;

    serde_json::from_slice::<VersionJson>(&bytes)
        .with_context(|| format!("failed to parse version JSON from {version_url}"))
}

async fn ensure_client_jar(
    client: &reqwest::Client,
    version_json: &VersionJson,
    version_dir: &Path,
) -> Result<PathBuf> {
    let dest = version_dir.join("client.jar");
    let dl = &version_json.downloads.client;
    download_file_verified(client, &dl.url, &dest, &dl.sha1).await?;
    Ok(dest)
}

async fn ensure_libraries(
    client: &reqwest::Client,
    version_json: &VersionJson,
    libraries_dir: &Path,
    _on_progress: &impl Fn(&str, &str),
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let os_key = current_os_natives_key();
    let mut library_paths = Vec::new();
    let mut native_paths = Vec::new();

    for library in &version_json.libraries {
        let rules = library.rules.as_deref().unwrap_or(&[]);
        if !library_allowed_on_current_os(rules) {
            continue;
        }

        let downloads = match &library.downloads {
            Some(d) => d,
            None => continue,
        };

        // Download the main artifact (non-native).
        if let Some(artifact) = &downloads.artifact {
            if let Some(path) = &artifact.path {
                let dest = contained_join(libraries_dir, path)?;
                download_file_verified(client, &artifact.url, &dest, &artifact.sha1).await?;
                library_paths.push(dest);
            }
        }

        // Download native classifier if present for this OS.
        if let Some(native_key) = library.natives.as_ref().and_then(|n| n.get(os_key)) {
            if let Some(classifiers) = &downloads.classifiers {
                if let Some(native_artifact) = classifiers.get(native_key.as_str()) {
                    if let Some(path) = &native_artifact.path {
                        let dest = contained_join(libraries_dir, path)?;
                        download_file_verified(
                            client,
                            &native_artifact.url,
                            &dest,
                            &native_artifact.sha1,
                        )
                        .await?;
                        native_paths.push(dest);
                    }
                }
            }
        }
    }

    Ok((library_paths, native_paths))
}

async fn ensure_assets(
    client: &reqwest::Client,
    version_json: &VersionJson,
    assets_dir: &Path,
    on_progress: &impl Fn(&str, &str),
) -> Result<()> {
    let indexes_dir = assets_dir.join("indexes");
    let objects_dir = assets_dir.join("objects");
    std::fs::create_dir_all(&indexes_dir)
        .with_context(|| format!("failed to create {}", indexes_dir.display()))?;
    std::fs::create_dir_all(&objects_dir)
        .with_context(|| format!("failed to create {}", objects_dir.display()))?;

    // Download asset index JSON.
    let index_ref = &version_json.asset_index;
    let index_path = indexes_dir.join(format!("{}.json", index_ref.id));
    download_file_verified(client, &index_ref.url, &index_path, &index_ref.sha1).await?;

    // Parse asset index.
    let index_contents = std::fs::read_to_string(&index_path)
        .with_context(|| format!("failed to read asset index {}", index_path.display()))?;
    let asset_index: AssetIndexJson = serde_json::from_str(&index_contents)
        .with_context(|| format!("failed to parse asset index {}", index_path.display()))?;

    // Count how many assets need downloading.
    let to_download: Vec<String> = asset_index
        .objects
        .values()
        .map(|obj| {
            let hash = &obj.hash;
            let prefix = &hash[..2];
            let dest = contained_join(objects_dir.as_path(), &format!("{prefix}/{hash}"))?;
            Ok((hash.clone(), dest))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(hash, dest)| (!dest.exists()).then_some(hash))
        .collect();

    if to_download.is_empty() {
        return Ok(());
    }

    on_progress(
        "Downloading assets",
        &format!("{} objects", to_download.len()),
    );

    // Download missing assets in parallel (up to 32 concurrent).
    const CONCURRENCY: usize = 32;
    let chunks: Vec<&[String]> = to_download.chunks(CONCURRENCY).collect();

    for chunk in chunks {
        let mut join_set: JoinSet<Result<()>> = JoinSet::new();

        for hash in chunk {
            let hash = hash.clone();
            let client = client.clone();
            let objects_dir = objects_dir.clone();
            let base_url = MC_ASSETS_BASE_URL;

            join_set.spawn(async move {
                let prefix = &hash[..2];
                let dest =
                    contained_join(objects_dir.as_path(), &format!("{prefix}/{hash}"))?;
                if dest.exists() {
                    return Ok(());
                }
                let url = format!("{}/{}/{}", base_url, prefix, hash);
                download_file(&client, &url, &dest).await
            });
        }

        while let Some(result) = join_set.join_next().await {
            result
                .context("asset download task panicked")?
                .context("asset download failed")?;
        }
    }

    Ok(())
}

fn extract_arguments(version_json: &VersionJson) -> (Vec<String>, Vec<String>) {
    if let Some(arguments) = &version_json.arguments {
        let game = flatten_args(&arguments.game, false);
        let jvm = flatten_args(&arguments.jvm, true);
        (game, jvm)
    } else if let Some(mc_args) = &version_json.minecraft_arguments {
        // Old pre-1.13 format: a single space-separated string.
        let game = mc_args.split_whitespace().map(|s| s.to_string()).collect();
        (game, Vec::new())
    } else {
        (Vec::new(), Vec::new())
    }
}

/// Flatten `ArgEntry` list into plain strings, filtering by OS rules.
fn flatten_args(entries: &[ArgEntry], filter_by_os: bool) -> Vec<String> {
    let mut result = Vec::new();
    for entry in entries {
        match entry {
            ArgEntry::Simple(s) => result.push(s.clone()),
            ArgEntry::Conditional { rules, value } => {
                if !filter_by_os || arg_rule_passes(rules) {
                    match value {
                        ArgValue::One(s) => result.push(s.clone()),
                        ArgValue::Many(v) => result.extend(v.iter().cloned()),
                    }
                }
            }
        }
    }
    result
}

/// Extract native JARs into the instance `natives/` directory.
pub fn extract_natives(native_paths: &[PathBuf], natives_dir: &Path) -> Result<()> {
    use std::io::Read;

    std::fs::create_dir_all(natives_dir)
        .with_context(|| format!("failed to create natives dir {}", natives_dir.display()))?;

    for jar_path in native_paths {
        let file = std::fs::File::open(jar_path)
            .with_context(|| format!("failed to open native JAR {}", jar_path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read ZIP {}", jar_path.display()))?;

        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .with_context(|| format!("failed to read ZIP entry {i}"))?;
            let name = entry.name().to_string();

            // Skip directories and META-INF entries.
            if name.ends_with('/') || name.contains("META-INF") {
                continue;
            }

            // Flatten: only use the file name, not any directory structure.
            let file_name = std::path::Path::new(&name)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(name.clone());

            let dest = natives_dir.join(&file_name);
            if dest.exists() {
                continue;
            }

            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .with_context(|| format!("failed to read entry '{name}' from ZIP"))?;
            std::fs::write(&dest, &buf)
                .with_context(|| format!("failed to write native {}", dest.display()))?;
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_allowed_with_no_rules() {
        assert!(library_allowed_on_current_os(&[]));
    }

    #[test]
    fn library_path_traversal_is_contained() {
        let libraries_dir = Path::new("/cache/libraries");

        assert!(contained_join(libraries_dir, "../../outside.jar").is_err());
        assert_eq!(
            contained_join(libraries_dir, "org/example/library/1.0/library-1.0.jar").unwrap(),
            libraries_dir.join("org/example/library/1.0/library-1.0.jar")
        );
    }

    #[test]
    fn library_blocked_by_wrong_os_allow_rule() {
        // An "allow windows" rule should block on Linux/macOS and allow on Windows.
        let rules = vec![OsRule {
            action: "allow".to_string(),
            os: Some(OsFilter {
                name: Some("windows".to_string()),
            }),
        }];
        let result = library_allowed_on_current_os(&rules);
        if cfg!(target_os = "windows") {
            assert!(result);
        } else {
            assert!(!result);
        }
    }

    #[test]
    fn extracts_simple_game_arguments() {
        let entries = vec![
            ArgEntry::Simple("--username".to_string()),
            ArgEntry::Simple("${auth_player_name}".to_string()),
        ];
        let result = flatten_args(&entries, false);
        assert_eq!(result, vec!["--username", "${auth_player_name}"]);
    }

    #[test]
    fn conditional_jvm_args_filtered_by_current_os() {
        let entries = vec![
            ArgEntry::Conditional {
                rules: vec![OsRule {
                    action: "allow".to_string(),
                    os: Some(OsFilter {
                        name: Some("windows".to_string()),
                    }),
                }],
                value: ArgValue::One("-XX:HeapDumpPath=windows.hprof".to_string()),
            },
            ArgEntry::Simple("-Xss1M".to_string()),
        ];
        let result = flatten_args(&entries, true);
        if cfg!(target_os = "windows") {
            assert_eq!(result.len(), 2);
        } else {
            // The conditional windows arg is filtered out; only the simple one remains.
            assert_eq!(result, vec!["-Xss1M"]);
        }
    }
}
