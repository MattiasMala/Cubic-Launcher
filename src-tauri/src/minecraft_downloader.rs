use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tokio::task::JoinSet;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::{contained_join, validate_path_component};

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
    /// True for legacy "virtual" asset indexes (<=1.7.2): assets are
    /// materialized under `assets/virtual/<id>/` and `--assetsDir` points there.
    pub is_virtual_assets: bool,
    /// Standard game arguments with `${placeholder}` tokens still present.
    pub game_arguments: Vec<String>,
    /// JVM arguments (filtered by the current OS), with `${placeholder}` tokens.
    pub jvm_arguments: Vec<String>,
    /// Whether this client's own manifest declares the Quick Play
    /// singleplayer option, i.e. whether `arguments.game` carries the
    /// `--quickPlaySingleplayer` entry behind its feature rule.
    ///
    /// This is the support criterion for the "jump into the world" Play
    /// (E10): the client about to run says what its own argument parser
    /// accepts, which no comparison of `1.20.1`, `26.3` and `23w14a` can do.
    /// Pre-1.13 versions, which carry a flat `minecraft_arguments` string,
    /// come out `false` — Quick Play arrived in 1.20.
    pub supports_quick_play_singleplayer: bool,
    /// The Java major the version.json declares (`javaVersion.majorVersion`),
    /// when it declares one. It is the only source that also knows snapshots,
    /// pre-releases and release candidates (download-3).
    pub java_major_version: Option<u32>,
}

/// Whether [`ensure_minecraft_version`] may download what the cache lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinecraftCachePolicy {
    /// Download whatever is missing or does not match its hash.
    Download,
    /// D142: the active account plays offline. Everything must already be in
    /// the cache, and nothing is requested (D132); what is missing stops the
    /// launch with an error that says so.
    CacheOnly,
}

/// A file a cache-only launch needs and does not find (D142).
#[derive(Debug)]
struct MissingFromCache(String);

impl std::fmt::Display for MissingFromCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is not in the cache", self.0)
    }
}

impl std::error::Error for MissingFromCache {}

// ── Mojang API serde structs ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct VersionManifest {
    versions: Vec<VersionManifestEntry>,
}

#[derive(Deserialize)]
struct VersionManifestEntry {
    id: String,
    url: String,
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
    /// The Java the version runs on. Mojang declares it for every version,
    /// snapshots included; a hand-made version.json may not.
    #[serde(default)]
    java_version: Option<JavaVersionRef>,
    /// The log4j configuration of the client (download-1).
    #[serde(default)]
    logging: Option<VersionLogging>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaVersionRef {
    major_version: u32,
}

#[derive(Deserialize, Default)]
struct VersionLogging {
    #[serde(default)]
    client: Option<LoggingClient>,
}

/// `logging.client`: the JVM argument (`-Dlog4j.configurationFile=${path}`)
/// and the file it points to. It is how Mojang fixed Log4Shell for the
/// versions with a log4j older than 2.15 (1.7–1.18.0): the file filters
/// `${...}` lookups out of every message.
#[derive(Deserialize)]
struct LoggingClient {
    argument: String,
    file: LoggingFile,
}

#[derive(Deserialize)]
struct LoggingFile {
    id: String,
    sha1: String,
    url: String,
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
    #[serde(default, rename = "virtual")]
    is_virtual: bool,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
}

fn asset_hash_prefix(hash: &str) -> Result<&str> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid asset SHA-1 '{hash}'");
    }
    Ok(&hash[..2])
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
            .is_none_or(|name| name == os);

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
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open file for SHA1 check: {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read file for SHA1 check: {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
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

    let actual = {
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    if !actual.eq_ignore_ascii_case(expected_sha1) {
        bail!(
            "SHA1 mismatch for {}: expected {expected_sha1}, got {actual}",
            dest.display()
        );
    }

    crate::atomic_write::write_atomically(dest, &bytes)
        .with_context(|| format!("failed to publish {}", dest.display()))?;

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
    crate::atomic_write::write_atomically(dest, &bytes)
        .with_context(|| format!("failed to publish {}", dest.display()))?;
    Ok(())
}

// ── Version list ──────────────────────────────────────────────────────────────

/// Returns `true` if `client.jar` for the given version is already in the cache.
pub fn is_version_cached(launcher_paths: &LauncherPaths, version: &str) -> bool {
    launcher_paths
        .mc_version_dir(version)
        .join("client.jar")
        .exists()
}

// ── Main public function ──────────────────────────────────────────────────────

/// Ensures all Minecraft files for `minecraft_version` are present in the shared
/// cache (`cache/minecraft/`), downloading anything missing.
///
/// Returns a `MinecraftVersionData` describing all paths and arguments needed for
/// a launch. Safe to call on every launch — skips files whose SHA1 already matches.
///
/// With [`MinecraftCachePolicy::CacheOnly`] nothing is requested: a file that
/// is missing or does not match its hash stops the launch with an error that
/// names the version and the cause (D142).
pub async fn ensure_minecraft_version(
    http_client: &reqwest::Client,
    minecraft_version: &str,
    launcher_paths: &LauncherPaths,
    policy: MinecraftCachePolicy,
    on_progress: impl Fn(&str, &str),
) -> Result<MinecraftVersionData> {
    let result = ensure_minecraft_version_files(
        http_client,
        minecraft_version,
        launcher_paths,
        policy,
        &on_progress,
    )
    .await;
    match result {
        Err(error) if policy == MinecraftCachePolicy::CacheOnly => {
            match error.downcast_ref::<MissingFromCache>() {
                Some(missing) => Err(anyhow::anyhow!(
                    "Minecraft {minecraft_version} has never been downloaded completely on this computer ({missing}), and the active account plays offline, so nothing is downloaded. Play this version once with the account online (switch the account to online for one launch); after that it plays offline too."
                )),
                None => Err(error),
            }
        }
        result => result,
    }
}

async fn ensure_minecraft_version_files(
    http_client: &reqwest::Client,
    minecraft_version: &str,
    launcher_paths: &LauncherPaths,
    policy: MinecraftCachePolicy,
    on_progress: &impl Fn(&str, &str),
) -> Result<MinecraftVersionData> {
    let version_dir = launcher_paths.mc_version_dir(minecraft_version);
    let libraries_dir = launcher_paths.mc_libraries_dir();
    let assets_dir = launcher_paths.mc_assets_dir();

    std::fs::create_dir_all(&version_dir)
        .with_context(|| format!("failed to create {}", version_dir.display()))?;
    std::fs::create_dir_all(&libraries_dir)
        .with_context(|| format!("failed to create {}", libraries_dir.display()))?;

    // 1+2. The version JSON, from the cache when it is there.
    on_progress("Fetch version metadata", minecraft_version);
    let version_json = load_version_json(
        http_client,
        minecraft_version,
        &version_dir,
        policy,
        on_progress,
    )
    .await?;

    // 3. Ensure client JAR.
    on_progress("Ensure client JAR", minecraft_version);
    let client_jar_path =
        ensure_client_jar(http_client, &version_json, &version_dir, policy).await?;

    // 4. Ensure libraries.
    on_progress("Ensure libraries", minecraft_version);
    let (library_paths, native_paths) =
        ensure_libraries(http_client, &version_json, &libraries_dir, policy).await?;

    // 5. Ensure assets. Legacy indexes (<=1.7.2) are "virtual": the game reads
    //    real filenames from a materialized tree rather than the hashed object
    //    store, so we must copy them out and point --assetsDir there.
    on_progress("Ensure assets", minecraft_version);
    let is_virtual_assets = ensure_assets(
        http_client,
        &version_json,
        &assets_dir,
        &version_json.assets,
        policy,
        on_progress,
    )
    .await?;

    // 6. The log4j configuration of `logging.client` (download-1).
    let logging_argument =
        ensure_logging_config(http_client, &version_json, &assets_dir, policy, on_progress).await?;

    // 7. Extract arguments. `game_arguments` is unfiltered on purpose (the
    //    launch strips the feature-gated entries later), which is also what
    //    makes it the honest place to read Quick Play support from.
    let (game_arguments, mut jvm_arguments) = extract_arguments(&version_json);
    jvm_arguments.extend(logging_argument);
    let supports_quick_play_singleplayer = declares_quick_play_singleplayer(&game_arguments);

    Ok(MinecraftVersionData {
        main_class: version_json.main_class,
        client_jar_path,
        library_paths,
        native_paths,
        assets_dir,
        asset_index_id: version_json.assets,
        is_virtual_assets,
        game_arguments,
        jvm_arguments,
        supports_quick_play_singleplayer,
        java_major_version: version_json
            .java_version
            .map(|java_version| java_version.major_version),
    })
}

// ── Internal steps ────────────────────────────────────────────────────────────

/// `dest` with the hash `expected_sha1`: downloaded when the policy allows
/// it, otherwise it must already be there.
async fn ensure_verified_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha1: &str,
    policy: MinecraftCachePolicy,
    description: &str,
) -> Result<()> {
    match policy {
        MinecraftCachePolicy::Download => {
            download_file_verified(client, url, dest, expected_sha1).await
        }
        MinecraftCachePolicy::CacheOnly if file_matches_sha1(dest, expected_sha1) => Ok(()),
        MinecraftCachePolicy::CacheOnly => {
            Err(MissingFromCache(format!("{description} {}", dest.display())).into())
        }
    }
}

/// Il `version.json` di `minecraft_version`, da `version_dir` quando è in cache.
///
/// Il manifest si chiede solo se la cache non ha `version.json`: nel lancio il
/// suo unico prodotto è l'URL di quel file, quindi con il file sul disco la
/// richiesta veniva fatta e la risposta buttata, e l'unico effetto osservabile
/// era un lancio che muore quando la rete non risponde — ogni lancio offline,
/// misurato (report 098, S1). Una strada sola per tutti gli account, non un
/// ramo offline: il lancio online perde una richiesta di cui non leggeva il
/// risultato.
///
/// Un `version.json` in cache che non si legge si riscarica (069 M2): prima
/// l'errore di parse tornava a ogni lancio, e quella versione non partiva più
/// finché qualcuno non cancellava il file a mano. Offline (D142) lo stesso
/// caso è un errore che lo dice, senza richieste.
async fn load_version_json(
    http_client: &reqwest::Client,
    minecraft_version: &str,
    version_dir: &Path,
    policy: MinecraftCachePolicy,
    on_progress: &impl Fn(&str, &str),
) -> Result<VersionJson> {
    let cached_path = version_dir.join("version.json");
    if cached_path.exists() {
        let damage = match std::fs::read_to_string(&cached_path) {
            Ok(contents) => match serde_json::from_str::<VersionJson>(&contents) {
                Ok(version_json) => return Ok(version_json),
                Err(error) => error.to_string(),
            },
            Err(error) => error.to_string(),
        };
        if policy == MinecraftCachePolicy::CacheOnly {
            return Err(MissingFromCache(format!(
                "a readable version.json (the cached {} is damaged: {damage})",
                cached_path.display()
            ))
            .into());
        }
        on_progress(
            "Cached version metadata is damaged, downloading it again",
            &format!("{}: {damage}", cached_path.display()),
        );
    } else if policy == MinecraftCachePolicy::CacheOnly {
        return Err(MissingFromCache(format!("{}", cached_path.display())).into());
    }

    let version_url = fetch_version_url(http_client, minecraft_version).await?;
    fetch_version_json(http_client, &version_url, &cached_path).await
}

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
    cached_path: &Path,
) -> Result<VersionJson> {
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

    crate::atomic_write::write_atomically(cached_path, &bytes)
        .with_context(|| format!("failed to cache version JSON at {}", cached_path.display()))?;

    serde_json::from_slice::<VersionJson>(&bytes)
        .with_context(|| format!("failed to parse version JSON from {version_url}"))
}

async fn ensure_client_jar(
    client: &reqwest::Client,
    version_json: &VersionJson,
    version_dir: &Path,
    policy: MinecraftCachePolicy,
) -> Result<PathBuf> {
    let dest = version_dir.join("client.jar");
    let dl = &version_json.downloads.client;
    ensure_verified_file(client, &dl.url, &dest, &dl.sha1, policy, "the client jar").await?;
    Ok(dest)
}

async fn ensure_libraries(
    client: &reqwest::Client,
    version_json: &VersionJson,
    libraries_dir: &Path,
    policy: MinecraftCachePolicy,
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
                ensure_verified_file(
                    client,
                    &artifact.url,
                    &dest,
                    &artifact.sha1,
                    policy,
                    "the library",
                )
                .await?;
                library_paths.push(dest);
            }
        }

        // Download native classifier if present for this OS.
        if let Some(native_key) = library.natives.as_ref().and_then(|n| n.get(os_key)) {
            if let Some(classifiers) = &downloads.classifiers {
                if let Some(native_artifact) = classifiers.get(native_key.as_str()) {
                    if let Some(path) = &native_artifact.path {
                        let dest = contained_join(libraries_dir, path)?;
                        ensure_verified_file(
                            client,
                            &native_artifact.url,
                            &dest,
                            &native_artifact.sha1,
                            policy,
                            "the native library",
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
    asset_index_id: &str,
    policy: MinecraftCachePolicy,
    on_progress: &impl Fn(&str, &str),
) -> Result<bool> {
    let indexes_dir = assets_dir.join("indexes");
    let objects_dir = assets_dir.join("objects");
    std::fs::create_dir_all(&indexes_dir)
        .with_context(|| format!("failed to create {}", indexes_dir.display()))?;
    std::fs::create_dir_all(&objects_dir)
        .with_context(|| format!("failed to create {}", objects_dir.display()))?;

    // Download asset index JSON.
    let index_ref = &version_json.asset_index;
    let index_path = indexes_dir.join(format!("{}.json", index_ref.id));
    ensure_verified_file(
        client,
        &index_ref.url,
        &index_path,
        &index_ref.sha1,
        policy,
        "the asset index",
    )
    .await?;

    // Parse asset index.
    let index_contents = std::fs::read_to_string(&index_path)
        .with_context(|| format!("failed to read asset index {}", index_path.display()))?;
    let asset_index: AssetIndexJson = serde_json::from_str(&index_contents)
        .with_context(|| format!("failed to parse asset index {}", index_path.display()))?;
    let is_virtual = asset_index.is_virtual;

    let to_download = missing_asset_hashes(&asset_index, &objects_dir)?;
    if policy == MinecraftCachePolicy::CacheOnly && !to_download.is_empty() {
        return Err(MissingFromCache(format!(
            "{} of the {} asset objects of index '{}'",
            to_download.len(),
            asset_index.objects.len(),
            index_ref.id
        ))
        .into());
    }

    if !to_download.is_empty() {
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
                    let prefix = asset_hash_prefix(&hash)?;
                    let dest = contained_join(objects_dir.as_path(), &format!("{prefix}/{hash}"))?;
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
    }

    // Legacy virtual indexes: materialize real filenames under
    // assets/virtual/<id>/ so the game (which ignores the hashed store) can read
    // them via --assetsDir. Copies are skipped when already present.
    if is_virtual {
        materialize_virtual_assets(&asset_index, &objects_dir, assets_dir, asset_index_id)?;
    }

    Ok(is_virtual)
}

/// The asset objects not yet on disk, each hash once. An index can list the
/// same object under several names (indexes 5, 8 and 17 do), and two
/// concurrent downloads of one object would share its `.part`: the second
/// rename would find the partial already moved and fail the launch.
fn missing_asset_hashes(asset_index: &AssetIndexJson, objects_dir: &Path) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for object in asset_index.objects.values() {
        let prefix = asset_hash_prefix(&object.hash)?;
        let dest = contained_join(objects_dir, &format!("{prefix}/{}", object.hash))?;
        if !dest.exists() {
            missing.push(object.hash.clone());
        }
    }
    missing.sort_unstable();
    missing.dedup();
    Ok(missing)
}

/// Copy hashed asset objects into `assets/virtual/<id>/<real/path>` for legacy
/// ("virtual") indexes. Idempotent: existing up-to-date files are left alone.
fn materialize_virtual_assets(
    asset_index: &AssetIndexJson,
    objects_dir: &Path,
    assets_dir: &Path,
    asset_index_id: &str,
) -> Result<()> {
    validate_path_component(asset_index_id)?;
    let virtual_root = contained_join(assets_dir.join("virtual").as_path(), asset_index_id)?;

    for (logical_path, object) in &asset_index.objects {
        let hash = &object.hash;
        let prefix = asset_hash_prefix(hash)?;
        let source = contained_join(objects_dir, &format!("{prefix}/{hash}"))?;
        let dest = contained_join(virtual_root.as_path(), logical_path)?;

        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::copy(&source, &dest).with_context(|| {
            format!(
                "failed to materialize virtual asset {} -> {}",
                source.display(),
                dest.display()
            )
        })?;
    }

    Ok(())
}

/// The JVM argument of `logging.client`, with its configuration file in the
/// cache (download-1), or `None` when the version declares none.
///
/// The file goes to `assets/log_configs/<id>`, shared by every version that
/// names the same id (the official launcher's layout), downloaded with its
/// SHA-1. Offline (D142) a file that is not there yet is not requested: the
/// launch goes on without it and says so, because a version played before
/// this file was handled would otherwise stop playing offline at all.
async fn ensure_logging_config(
    client: &reqwest::Client,
    version_json: &VersionJson,
    assets_dir: &Path,
    policy: MinecraftCachePolicy,
    on_progress: &impl Fn(&str, &str),
) -> Result<Option<String>> {
    let Some(logging) = version_json
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
    else {
        return Ok(None);
    };
    let config_path = log_config_path(assets_dir, &logging.file.id)?;

    match policy {
        MinecraftCachePolicy::Download => {
            on_progress("Ensure logging configuration", &logging.file.id);
            download_file_verified(client, &logging.file.url, &config_path, &logging.file.sha1)
                .await
                .with_context(|| {
                    format!(
                        "failed to download the log4j configuration '{}'",
                        logging.file.id
                    )
                })?;
        }
        MinecraftCachePolicy::CacheOnly => {
            if !file_matches_sha1(&config_path, &logging.file.sha1) {
                on_progress(
                    "Logging configuration not in the cache",
                    &format!(
                        "'{}' is not downloaded while the account plays offline; this launch runs without Mojang's log4j configuration (the Log4Shell filter) until the next online launch",
                        logging.file.id
                    ),
                );
                return Ok(None);
            }
        }
    }

    Ok(Some(logging_jvm_argument(&logging.argument, &config_path)))
}

/// Where the configuration file `file_id` of `logging.client` lives.
fn log_config_path(assets_dir: &Path, file_id: &str) -> Result<PathBuf> {
    validate_path_component(file_id)
        .with_context(|| format!("invalid log4j configuration id '{file_id}'"))?;
    Ok(assets_dir.join("log_configs").join(file_id))
}

/// `logging.client.argument` with `${path}` replaced by the file. One argv
/// entry: the path is never split or quoted, spaces included.
fn logging_jvm_argument(argument: &str, config_path: &Path) -> String {
    argument.replace("${path}", &config_path.to_string_lossy())
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

/// Whether the version's game arguments declare `--quickPlaySingleplayer`.
///
/// The entry lives behind a `is_quick_play_singleplayer` feature rule, which
/// `flatten_args` keeps (features are not OS rules and are not filtered
/// here), so its presence in the flattened list is exactly "this client
/// understands the option". Verified against the five version manifests in
/// the shared cache: 1.20.1, 1.20.2, 1.21.1, 26.1 and 26.3 all declare it.
fn declares_quick_play_singleplayer(game_arguments: &[String]) -> bool {
    game_arguments
        .iter()
        .any(|argument| argument == "--quickPlaySingleplayer")
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
///
/// Every file is checked against the jar on every launch: one whose bytes
/// differ (a kill during the extraction left it truncated, or another
/// version of the library wrote it) is written again, atomically, so a kill
/// in the middle never leaves a half native that a later launch would keep
/// (todo.md:1519). Within one extraction the first jar that carries a name
/// wins, as before.
pub fn extract_natives(native_paths: &[PathBuf], natives_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(natives_dir)
        .with_context(|| format!("failed to create natives dir {}", natives_dir.display()))?;

    let mut extracted = std::collections::HashSet::new();
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
            if !extracted.insert(file_name.clone()) {
                continue;
            }

            let dest = natives_dir.join(&file_name);
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .with_context(|| format!("failed to read entry '{name}' from ZIP"))?;
            if native_is_current(&dest, &buf) {
                continue;
            }
            crate::atomic_write::write_atomically(&dest, &buf)
                .with_context(|| format!("failed to write native {}", dest.display()))?;
        }
    }

    Ok(())
}

/// Whether `dest` already holds exactly `expected`: the size first, which
/// settles a truncated file without reading it.
fn native_is_current(dest: &Path, expected: &[u8]) -> bool {
    match std::fs::metadata(dest) {
        Ok(metadata) if metadata.is_file() && metadata.len() == expected.len() as u64 => {
            std::fs::read(dest).is_ok_and(|current| current == expected)
        }
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("download-tests")
            .join(format!("minecraft-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch root should be created");
        root
    }

    fn fixed_server(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("server address").port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request should arrive");
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("header");
            stream.write_all(body).expect("body");
            stream.flush().expect("flush");
        });
        format!("http://127.0.0.1:{port}/download")
    }

    fn partial_path(path: &Path) -> PathBuf {
        let mut name = path.file_name().expect("target file name").to_os_string();
        name.push(".part");
        path.with_file_name(name)
    }

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
    fn asset_hash_prefix_requires_a_sha1() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(asset_hash_prefix(sha1).unwrap(), "01");
        assert!(asset_hash_prefix("a").is_err());
        assert!(asset_hash_prefix("../../outside").is_err());
        assert!(asset_hash_prefix("g123456789abcdef0123456789abcdef01234567").is_err());
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

    #[test]
    fn quick_play_support_is_read_from_the_version_json() {
        // The shape Mojang ships: the option is an argument entry behind a
        // feature rule, not an OS rule.
        let modern = r#"{
          "mainClass": "net.minecraft.client.main.Main",
          "downloads": { "client": { "url": "https://example.invalid/client.jar", "sha1": "0123456789abcdef0123456789abcdef01234567" } },
          "libraries": [],
          "assetIndex": { "id": "17", "url": "https://example.invalid/17.json", "sha1": "0123456789abcdef0123456789abcdef01234567" },
          "assets": "17",
          "arguments": {
            "game": [
              "--username", "${auth_player_name}",
              { "rules": [{ "action": "allow", "features": { "is_quick_play_singleplayer": true } }],
                "value": ["--quickPlaySingleplayer", "${quickPlaySingleplayer}"] }
            ],
            "jvm": []
          }
        }"#;
        let modern: VersionJson =
            serde_json::from_str(modern).expect("the version json should deserialize");
        let (game_arguments, _) = extract_arguments(&modern);
        assert!(declares_quick_play_singleplayer(&game_arguments));

        // A pre-1.13 version: one flat argument string, and no Quick Play.
        let legacy = r#"{
          "mainClass": "net.minecraft.client.main.Main",
          "downloads": { "client": { "url": "https://example.invalid/client.jar", "sha1": "0123456789abcdef0123456789abcdef01234567" } },
          "libraries": [],
          "assetIndex": { "id": "legacy", "url": "https://example.invalid/legacy.json", "sha1": "0123456789abcdef0123456789abcdef01234567" },
          "assets": "legacy",
          "minecraftArguments": "--username ${auth_player_name} --gameDir ${game_directory}"
        }"#;
        let legacy: VersionJson =
            serde_json::from_str(legacy).expect("the version json should deserialize");
        let (game_arguments, _) = extract_arguments(&legacy);
        assert!(!declares_quick_play_singleplayer(&game_arguments));
    }
    #[tokio::test]
    async fn unverified_asset_download_replaces_a_stale_partial_and_lands_whole() {
        let root = scratch_root("asset");
        let destination = root.join("asset");
        fs::write(partial_path(&destination), b"stale partial").expect("stale partial");
        let url = fixed_server(b"complete asset");

        download_file(&reqwest::Client::new(), &url, &destination)
            .await
            .expect("asset download should complete");

        assert_eq!(fs::read(&destination).expect("asset"), b"complete asset");
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn verified_hash_mismatch_leaves_the_previous_file_untouched() {
        let root = scratch_root("wrong-hash");
        let destination = root.join("library.jar");
        fs::write(&destination, b"previous bytes").expect("previous library");
        let url = fixed_server(b"different bytes");

        let result =
            download_file_verified(&reqwest::Client::new(), &url, &destination, &"0".repeat(40))
                .await;

        assert!(result.is_err(), "hash mismatch must be reported");
        assert_eq!(
            fs::read(&destination).expect("previous library remains"),
            b"previous bytes"
        );
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[test]
    fn an_object_listed_under_two_names_is_downloaded_once() {
        let root = scratch_root("duplicate-assets");
        let shared = "a".repeat(40);
        let present = "b".repeat(40);
        let index: AssetIndexJson = serde_json::from_str(&format!(
            r#"{{ "objects": {{
                "minecraft/sounds/one.ogg": {{ "hash": "{shared}" }},
                "minecraft/sounds/two.ogg": {{ "hash": "{shared}" }},
                "minecraft/lang/present.json": {{ "hash": "{present}" }}
            }} }}"#
        ))
        .expect("asset index should deserialize");
        let present_path = root.join("bb").join(&present);
        fs::create_dir_all(present_path.parent().expect("object folder")).expect("object folder");
        fs::write(&present_path, b"already here").expect("present object");

        let missing = missing_asset_hashes(&index, &root).expect("hashes are well formed");

        assert_eq!(missing, vec![shared]);
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    /// D-a (onda 8a): con `version.json` in cache il manifest non si chiede. Il
    /// client manda `piston-meta.mojang.com` su 127.0.0.1, dove nessuno serve
    /// quel certificato: qualunque richiesta al manifest fallisce, quindi un
    /// `Ok` vuol dire che non è stata fatta. Sul codice di prima il manifest
    /// veniva chiesto prima di guardare la cache, e il lancio moriva lì.
    #[tokio::test]
    async fn a_cached_version_json_is_read_without_asking_the_manifest() {
        let root = scratch_root("cached-version-json");
        fs::write(
            root.join("version.json"),
            r#"{
                "mainClass": "net.minecraft.client.main.Main",
                "downloads": { "client": { "url": "https://example.invalid/client.jar", "sha1": "00" } },
                "libraries": [],
                "assetIndex": { "id": "5", "url": "https://example.invalid/5.json", "sha1": "00" },
                "assets": "5"
            }"#,
        )
        .expect("cached version.json");
        let unreachable_manifest = reqwest::Client::builder()
            .resolve("piston-meta.mojang.com", ([127, 0, 0, 1], 443).into())
            .build()
            .expect("client");

        let version_json = load_version_json(
            &unreachable_manifest,
            "1.20.1",
            &root,
            MinecraftCachePolicy::Download,
            &|_, _| {},
        )
        .await
        .expect("the cached version.json must be enough, with no manifest");

        assert_eq!(version_json.main_class, "net.minecraft.client.main.Main");
        assert_eq!(version_json.assets, "5");
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// A client whose every request goes to a listener nobody answers, and
    /// the listener itself: after the call, `accept` says whether anyone
    /// knocked. Zero requests is then something measured, not assumed.
    fn counting_offline_client() -> (reqwest::Client, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).expect("proxy"))
            .build()
            .expect("client");
        (client, listener)
    }

    fn no_request_reached(listener: &TcpListener) -> bool {
        matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        )
    }

    /// The shape of 1.12.2's version.json, cut to what the launch reads.
    fn version_json_1_12_2(client_sha1: &str, index_sha1: &str, log_sha1: &str) -> String {
        format!(
            r#"{{
              "mainClass": "net.minecraft.client.main.Main",
              "downloads": {{ "client": {{ "url": "https://example.invalid/client.jar", "sha1": "{client_sha1}" }} }},
              "libraries": [],
              "assetIndex": {{ "id": "1.12", "url": "https://example.invalid/1.12.json", "sha1": "{index_sha1}" }},
              "assets": "1.12",
              "javaVersion": {{ "component": "jre-legacy", "majorVersion": 8 }},
              "logging": {{ "client": {{
                "argument": "-Dlog4j.configurationFile=${{path}}",
                "file": {{ "id": "client-1.12.xml", "sha1": "{log_sha1}", "size": 888,
                          "url": "https://example.invalid/client-1.12.xml" }},
                "type": "log4j2-xml" }} }},
              "minecraftArguments": "--username ${{auth_player_name}} --version ${{version_name}}"
            }}"#
        )
    }

    /// download-1: `logging.client` si legge, e l'argomento JVM porta il
    /// percorso del file in cache al posto di `${path}`; download-3: anche
    /// `javaVersion.majorVersion`.
    #[test]
    fn logging_block_and_java_major_are_read_from_the_version_json() {
        let version_json: VersionJson = serde_json::from_str(&version_json_1_12_2(
            "00",
            "00",
            "bd65e7d2e3c237be76cfbef4c2405033d7f91521",
        ))
        .expect("the 1.12.2 shape should deserialize");
        let logging = version_json
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            .expect("logging.client");
        assert_eq!(logging.file.id, "client-1.12.xml");
        assert_eq!(
            version_json
                .java_version
                .as_ref()
                .map(|java| java.major_version),
            Some(8)
        );

        let assets_dir = Path::new("/launcher/cache/minecraft/assets");
        let config_path = log_config_path(assets_dir, &logging.file.id).expect("a plain id");
        assert_eq!(
            config_path,
            assets_dir.join("log_configs").join("client-1.12.xml")
        );
        assert_eq!(
            logging_jvm_argument(&logging.argument, &config_path),
            format!("-Dlog4j.configurationFile={}", config_path.display())
        );
        // A path with spaces stays one argument.
        assert_eq!(
            logging_jvm_argument(&logging.argument, Path::new("/My Games/client-1.12.xml")),
            "-Dlog4j.configurationFile=/My Games/client-1.12.xml"
        );

        // Versions with no block, or an empty one, get no argument.
        let bare: VersionJson = serde_json::from_str(
            r#"{
              "mainClass": "net.minecraft.client.Minecraft",
              "downloads": { "client": { "url": "https://example.invalid/client.jar", "sha1": "00" } },
              "libraries": [],
              "assetIndex": { "id": "legacy", "url": "https://example.invalid/legacy.json", "sha1": "00" },
              "assets": "legacy",
              "logging": {}
            }"#,
        )
        .expect("an empty logging block should deserialize");
        assert!(bare.logging.and_then(|logging| logging.client).is_none());
        assert!(bare.java_version.is_none());
    }

    #[test]
    fn a_log_config_id_cannot_leave_the_log_configs_folder() {
        let assets_dir = Path::new("/launcher/cache/minecraft/assets");
        assert!(log_config_path(assets_dir, "../../evil.xml").is_err());
        assert!(log_config_path(assets_dir, "nested/evil.xml").is_err());
    }

    /// Lays out a complete cache for the 1.12.2-shaped version: version.json,
    /// client jar, asset index with one object, and (optionally) the log
    /// configuration.
    fn complete_cache(root: &Path, with_log_config: bool) -> LauncherPaths {
        let paths = LauncherPaths::new(root);
        let version_dir = paths.mc_version_dir("1.12.2");
        let assets_dir = paths.mc_assets_dir();
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(assets_dir.join("indexes")).expect("indexes");

        let client_jar = b"client jar bytes";
        fs::write(version_dir.join("client.jar"), client_jar).expect("client jar");
        let object = b"an asset";
        let object_hash = sha1_hex(object);
        let object_path = assets_dir
            .join("objects")
            .join(&object_hash[..2])
            .join(&object_hash);
        fs::create_dir_all(object_path.parent().expect("object folder")).expect("object folder");
        fs::write(&object_path, object).expect("object");
        let index = format!(
            r#"{{ "objects": {{ "minecraft/sounds/a.ogg": {{ "hash": "{object_hash}" }} }} }}"#
        );
        fs::write(assets_dir.join("indexes").join("1.12.json"), &index).expect("index");
        let log_config = b"<Configuration/>";
        if with_log_config {
            fs::create_dir_all(assets_dir.join("log_configs")).expect("log configs");
            fs::write(
                assets_dir.join("log_configs").join("client-1.12.xml"),
                log_config,
            )
            .expect("log config");
        }
        fs::write(
            version_dir.join("version.json"),
            version_json_1_12_2(
                &sha1_hex(client_jar),
                &sha1_hex(index.as_bytes()),
                &sha1_hex(log_config),
            ),
        )
        .expect("version.json");
        paths
    }

    /// D142: offline, una versione tutta in cache parte senza una richiesta, e
    /// porta l'argomento di log4j (download-1).
    #[tokio::test]
    async fn cache_only_launches_a_complete_cache_with_zero_requests() {
        let root = scratch_root("cache-only-complete");
        let paths = complete_cache(&root, true);
        let (client, listener) = counting_offline_client();

        let data = ensure_minecraft_version(
            &client,
            "1.12.2",
            &paths,
            MinecraftCachePolicy::CacheOnly,
            |_, _| {},
        )
        .await
        .expect("everything is in the cache");

        assert!(
            no_request_reached(&listener),
            "a cache-only launch made a request"
        );
        assert_eq!(data.java_major_version, Some(8));
        let config_path = paths
            .mc_assets_dir()
            .join("log_configs")
            .join("client-1.12.xml");
        assert!(data.jvm_arguments.contains(&format!(
            "-Dlog4j.configurationFile={}",
            config_path.display()
        )));
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    /// Offline, la configurazione di log4j che manca non si scarica e non
    /// ferma il lancio: si parte senza, e il log lo dice.
    #[tokio::test]
    async fn cache_only_without_the_log_config_goes_on_and_says_so() {
        let root = scratch_root("cache-only-no-log-config");
        let paths = complete_cache(&root, false);
        let (client, listener) = counting_offline_client();
        let notes = std::sync::Mutex::new(Vec::<String>::new());

        let data = ensure_minecraft_version(
            &client,
            "1.12.2",
            &paths,
            MinecraftCachePolicy::CacheOnly,
            |label, detail| notes.lock().unwrap().push(format!("{label}: {detail}")),
        )
        .await
        .expect("the log config is not required offline");

        assert!(no_request_reached(&listener));
        assert!(!data
            .jvm_arguments
            .iter()
            .any(|argument| argument.starts_with("-Dlog4j.configurationFile=")));
        assert!(notes
            .lock()
            .unwrap()
            .iter()
            .any(|note| note.contains("client-1.12.xml") && note.contains("offline")));
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    /// D142: offline, una versione mai scaricata (o a metà) è un errore chiaro
    /// nello stile di D-d, e nessuna richiesta parte.
    #[tokio::test]
    async fn cache_only_with_something_missing_is_a_clear_error_and_no_request() {
        let root = scratch_root("cache-only-missing");
        let (client, listener) = counting_offline_client();

        // Mai scaricata: niente version.json.
        let never = LauncherPaths::new(root.join("never"));
        let message = ensure_minecraft_version(
            &client,
            "1.12.2",
            &never,
            MinecraftCachePolicy::CacheOnly,
            |_, _| {},
        )
        .await
        .expect_err("nothing is cached")
        .to_string();
        assert!(
            message.contains("Minecraft 1.12.2 has never been downloaded"),
            "{message}"
        );
        assert!(message.contains("plays offline"), "{message}");

        // A metà: un oggetto degli asset manca.
        let half = complete_cache(&root.join("half"), true);
        fs::remove_dir_all(half.mc_assets_dir().join("objects")).expect("drop objects");
        let message = ensure_minecraft_version(
            &client,
            "1.12.2",
            &half,
            MinecraftCachePolicy::CacheOnly,
            |_, _| {},
        )
        .await
        .expect_err("an asset object is missing")
        .to_string();
        assert!(message.contains("asset objects"), "{message}");
        assert!(message.contains("plays offline"), "{message}");

        // Client jar che non corrisponde al suo hash.
        let damaged = complete_cache(&root.join("damaged"), true);
        fs::write(
            damaged.mc_version_dir("1.12.2").join("client.jar"),
            b"truncated",
        )
        .expect("damage the jar");
        let message = ensure_minecraft_version(
            &client,
            "1.12.2",
            &damaged,
            MinecraftCachePolicy::CacheOnly,
            |_, _| {},
        )
        .await
        .expect_err("the client jar does not match")
        .to_string();
        assert!(message.contains("client jar"), "{message}");

        assert!(
            no_request_reached(&listener),
            "a cache-only launch made a request"
        );
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    /// 069 M2: un version.json in cache che non si legge non ferma più quella
    /// versione per sempre. Online si torna al manifest per riscaricarlo (qui
    /// il manifest non risponde, e l'errore è quello del manifest, non il parse
    /// di prima); offline è un errore che dice che il file è rovinato.
    #[tokio::test]
    async fn a_damaged_cached_version_json_is_downloaded_again_online_only() {
        let root = scratch_root("damaged-version-json");
        fs::write(root.join("version.json"), b"{ truncated").expect("damaged version.json");
        let (client, listener) = counting_offline_client();

        let offline = load_version_json(
            &client,
            "1.20.1",
            &root,
            MinecraftCachePolicy::CacheOnly,
            &|_, _| {},
        )
        .await
        .err()
        .expect("offline, a damaged version.json is an error");
        assert!(offline.downcast_ref::<MissingFromCache>().is_some());
        assert!(offline.to_string().contains("damaged"), "{offline}");
        assert!(no_request_reached(&listener));

        // Online the request is wanted: a proxy port that refuses it at once,
        // so the test does not wait on a listener that never answers.
        let refusing_port = TcpListener::bind("127.0.0.1:0")
            .expect("port")
            .local_addr()
            .expect("address")
            .port();
        let refused = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{refusing_port}")).expect("proxy"))
            .build()
            .expect("client");
        let online = load_version_json(
            &refused,
            "1.20.1",
            &root,
            MinecraftCachePolicy::Download,
            &|_, _| {},
        )
        .await
        .err()
        .expect("the manifest does not answer here");
        assert!(
            format!("{online:#}").contains("version manifest"),
            "online must go back to the manifest, got: {online:#}"
        );
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    fn native_jar(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).expect("native jar");
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::FileOptions::default())
                .expect("entry");
            zip.write_all(bytes).expect("entry bytes");
        }
        zip.finish().expect("jar");
    }

    /// todo.md:1519: un nativo troncato da un kill si riestrae, invece di
    /// restare troncato per sempre perché il nome esiste già; il primo jar
    /// che porta un nome vince ancora.
    #[test]
    fn a_truncated_native_is_extracted_again() {
        let root = scratch_root("natives");
        let natives_dir = root.join("natives");
        fs::create_dir_all(&natives_dir).expect("natives dir");
        let first = root.join("lwjgl-natives.jar");
        let second = root.join("other-natives.jar");
        native_jar(
            &first,
            &[
                ("liblwjgl.so", b"the whole native library"),
                ("META-INF/MANIFEST.MF", b"manifest"),
                ("libopenal.so", b"openal"),
            ],
        );
        native_jar(&second, &[("liblwjgl.so", b"another version")]);
        fs::write(natives_dir.join("liblwjgl.so"), b"the whole").expect("truncated native");
        fs::write(natives_dir.join("libopenal.so"), b"openal").expect("current native");

        extract_natives(&[first, second], &natives_dir).expect("extraction");

        assert_eq!(
            fs::read(natives_dir.join("liblwjgl.so")).expect("native"),
            b"the whole native library"
        );
        assert_eq!(
            fs::read(natives_dir.join("libopenal.so")).expect("native"),
            b"openal"
        );
        assert!(!natives_dir.join("MANIFEST.MF").exists());
        assert!(!natives_dir.join("liblwjgl.so.part").exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }
}
