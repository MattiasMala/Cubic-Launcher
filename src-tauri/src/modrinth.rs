use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::resolver::{ModLoader, ResolutionTarget};

const MODRINTH_API_BASE_URL: &str = "https://api.modrinth.com/v2";

/// Check whether a game-version string from Modrinth matches a concrete MC
/// version.  Handles wildcard patterns such as `"1.21.x"` / `"1.21.X"` where
/// the last segment is a case-insensitive `x` meaning "any patch".
pub fn mc_version_matches(pattern: &str, concrete: &str) -> bool {
    if pattern == concrete {
        return true;
    }
    // Check for trailing `.x` / `.X` wildcard
    let Some(prefix) = pattern
        .strip_suffix(".x")
        .or_else(|| pattern.strip_suffix(".X"))
    else {
        return false;
    };
    // `concrete` must start with the prefix followed by a dot and at least one
    // more character (the actual patch number).
    // e.g. pattern "1.21.x" → prefix "1.21", concrete "1.21.1" ✓
    concrete.starts_with(prefix) && concrete.as_bytes().get(prefix.len()) == Some(&b'.')
}

fn compare_minecraft_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse = |value: &str| -> Option<Vec<u64>> {
        value
            .split('.')
            .map(|segment| {
                let numeric = segment
                    .trim()
                    .split(|ch: char| !ch.is_ascii_digit())
                    .next()
                    .unwrap_or("");
                if numeric.is_empty() {
                    None
                } else {
                    numeric.parse::<u64>().ok()
                }
            })
            .collect()
    };

    let mut left_parts = parse(left)?;
    let mut right_parts = parse(right)?;
    let max_len = left_parts.len().max(right_parts.len());
    left_parts.resize(max_len, 0);
    right_parts.resize(max_len, 0);
    Some(left_parts.cmp(&right_parts))
}

fn extract_embedded_minecraft_versions(text: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            current.push(ch);
            continue;
        }

        if current.starts_with("1.") && current.matches('.').count() >= 1 {
            versions.push(current.clone());
        }
        current.clear();
    }

    if current.starts_with("1.") && current.matches('.').count() >= 1 {
        versions.push(current);
    }

    versions
}

fn explicit_version_affinity(version: &ModrinthVersion, target: &ResolutionTarget) -> i32 {
    let mut explicit_versions = extract_embedded_minecraft_versions(&version.version_number);
    if let Some(file) = version.primary_file() {
        explicit_versions.extend(extract_embedded_minecraft_versions(&file.filename));
    }

    if explicit_versions.is_empty() {
        return 0;
    }

    if explicit_versions
        .iter()
        .any(|candidate| candidate == &target.minecraft_version)
    {
        return 3;
    }

    let target_prefix = format!(
        "{}.",
        target
            .minecraft_version
            .rsplit_once('.')
            .map(|(prefix, _)| prefix)
            .unwrap_or(&target.minecraft_version)
    );

    if explicit_versions
        .iter()
        .all(|candidate| candidate.starts_with(&target_prefix))
        && explicit_versions.iter().any(|candidate| {
            compare_minecraft_versions(candidate, &target.minecraft_version)
                .is_some_and(|ordering| ordering != std::cmp::Ordering::Greater)
        })
    {
        return 2;
    }

    0
}

fn game_version_affinity(version: &ModrinthVersion, target: &ResolutionTarget) -> i32 {
    if version
        .game_versions
        .iter()
        .any(|game_version| game_version == &target.minecraft_version)
    {
        return 4;
    }

    if version.game_versions.iter().any(|game_version| {
        game_version
            .strip_suffix(".x")
            .or_else(|| game_version.strip_suffix(".X"))
            .is_some_and(|prefix| target.minecraft_version.starts_with(&format!("{prefix}.")))
    }) {
        return 3;
    }

    if version
        .game_versions
        .iter()
        .any(|game_version| mc_version_matches(game_version, &target.minecraft_version))
    {
        return 2;
    }

    0
}

pub fn sort_versions_by_target_preference(
    versions: &mut [ModrinthVersion],
    target: &ResolutionTarget,
) {
    versions.sort_by(|left, right| {
        // Target compatibility stays ahead of channel so a release for the wrong target cannot win.
        let left_key = (
            game_version_affinity(left, target),
            explicit_version_affinity(left, target),
            left.channel_rank(),
            &left.date_published,
        );
        let right_key = (
            game_version_affinity(right, target),
            explicit_version_affinity(right, target),
            right.channel_rank(),
            &right.date_published,
        );
        right_key.cmp(&left_key)
    });
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REQUEST_ATTEMPTS: u32 = 4;

/// Channel cascade for `version_files/update`, in the same order as
/// `ModrinthVersion::channel_rank` (release > beta > alpha). The endpoint
/// treats `version_types` as a set filter and then always prefers the most
/// recent version inside that set, so the ranking has to be rebuilt by asking
/// one channel at a time.
const VERSION_TYPE_CASCADE: [&str; 3] = ["release", "beta", "alpha"];

/// Guard against a pathological hash set, not the normal case: the endpoint has
/// no cap on the number of hashes, only a 2 MiB request-body limit (measured
/// 2026-09-07: 47 660 sha1 hashes are accepted, 47 661 return `400
/// request_error`). A 300-mod modlist is ~13 KB; 1 000 hashes is ~44 KB, two
/// orders of magnitude below the limit, so this never splits a real modlist.
const MAX_HASHES_PER_UPDATE_REQUEST: usize = 1_000;

/// Same shape of guard as `MAX_HASHES_PER_UPDATE_REQUEST`, for `GET /versions`.
/// That endpoint puts the id list in the query string, so the wall is the URL
/// length, not the count (measured 2026-09-07: 3 000 ids / ~33 KB answer
/// `200`, 3 500 / ~38 KB answer `500`, 4 000 / ~44 KB are refused before a
/// response). 1 000 ids is ~11 KB, and a 300-mod modlist is ~3,3 KB, so this
/// never splits a real modlist either.
const MAX_IDS_PER_VERSIONS_REQUEST: usize = 1_000;

pub(crate) fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("cubic-launcher/0.1.0 (https://github.com/arius-c/Cubic-Launcher)")
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

// Retry on HTTP 429 to be polite to Modrinth's rate limiter, and on transient
// server/transport failures. `build_request` is invoked fresh on every attempt
// because `RequestBuilder` is not `Clone` once a body is attached, which is the
// case for the POST endpoints.
async fn send_with_retry<F>(build_request: F) -> reqwest::Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut backoff = Duration::from_secs(2);

    for attempt in 1..=MAX_REQUEST_ATTEMPTS {
        match build_request().send().await {
            Ok(response) => {
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    if attempt == MAX_REQUEST_ATTEMPTS {
                        return Ok(response);
                    }

                    let retry_after_secs = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(backoff.as_secs().max(1))
                        .min(20);
                    tokio::time::sleep(Duration::from_secs(retry_after_secs)).await;
                    backoff = (backoff * 2).min(Duration::from_secs(20));
                    continue;
                }

                if response.status().is_server_error() && attempt < MAX_REQUEST_ATTEMPTS {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(20));
                    continue;
                }

                return Ok(response);
            }
            Err(error)
                if (error.is_timeout() || error.is_connect() || error.is_request())
                    && attempt < MAX_REQUEST_ATTEMPTS =>
            {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(20));
            }
            Err(error) => return Err(error),
        }
    }

    build_request().send().await
}

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    http_client: reqwest::Client,
    base_url: String,
}

impl ModrinthClient {
    pub fn new() -> Self {
        Self {
            http_client: build_http_client(),
            base_url: MODRINTH_API_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http_client: build_http_client(),
            base_url: base_url.into(),
        }
    }

    pub async fn fetch_project_versions(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Vec<ModrinthVersion>> {
        if project_id.trim().is_empty() {
            bail!("project_id cannot be empty");
        }

        let url = build_project_versions_url(&self.base_url, project_id, target)?;
        let response = send_with_retry(|| self.http_client.get(url.clone()))
            .await
            .with_context(|| {
                format!("failed to query Modrinth versions for project '{project_id}'")
            })?
            .error_for_status()
            .with_context(|| {
                format!("Modrinth returned an error for project '{project_id}' version lookup")
            })?;

        let versions = response
            .json::<Vec<ModrinthVersion>>()
            .await
            .with_context(|| {
                format!("failed to deserialize Modrinth versions for project '{project_id}'")
            })?;

        Ok(filter_compatible_versions(&versions, target))
    }

    pub async fn fetch_latest_compatible_version(
        &self,
        project_id: &str,
        target: &ResolutionTarget,
    ) -> Result<Option<ModrinthVersion>> {
        let versions = self.fetch_project_versions(project_id, target).await?;
        Ok(select_latest_compatible_version(&versions, target))
    }

    /// Fetch versions for a content pack (resource pack, data pack, shader).
    /// Only filters by game version, not by loader.
    pub async fn fetch_content_pack_versions(
        &self,
        project_id: &str,
        minecraft_version: &str,
    ) -> Result<Vec<ModrinthVersion>> {
        if project_id.trim().is_empty() {
            bail!("project_id cannot be empty");
        }
        let sanitized = self.base_url.trim_end_matches('/');
        let mut url = Url::parse(&format!("{sanitized}/project/{project_id}/version"))
            .with_context(|| format!("invalid Modrinth base URL '{}'", self.base_url))?;
        let game_versions_json = serde_json::to_string(&vec![minecraft_version])?;
        url.query_pairs_mut()
            .append_pair("game_versions", &game_versions_json);
        let response = send_with_retry(|| self.http_client.get(url.clone()))
            .await
            .with_context(|| {
                format!("failed to query Modrinth versions for content pack '{project_id}'")
            })?
            .error_for_status()
            .with_context(|| {
                format!("Modrinth returned an error for content pack '{project_id}'")
            })?;
        let versions = response
            .json::<Vec<ModrinthVersion>>()
            .await
            .with_context(|| {
                format!("failed to deserialize Modrinth versions for content pack '{project_id}'")
            })?;
        // Filter to only versions matching the MC version
        Ok(versions
            .into_iter()
            .filter(|v| {
                v.game_versions
                    .iter()
                    .any(|gv| mc_version_matches(gv, minecraft_version))
            })
            .collect())
    }

    /// Version metadata for a set of version ids, in **one** request instead of
    /// one `GET /version/{id}` per mod.
    ///
    /// Keyed by version id, taken from the payload and not from the request:
    /// the response is a JSON array whose order does not follow the ids asked
    /// for (measured 2026-09-07). An id Modrinth does not know is simply
    /// absent, with `200` — same silence as the bulk hash endpoint. An id that
    /// is not valid base62 fails the **whole** request with `400`, which is why
    /// the callers only pass ids Modrinth itself produced.
    pub async fn fetch_versions_by_ids(
        &self,
        version_ids: &[String],
    ) -> Result<HashMap<String, ModrinthVersion>> {
        let ids = normalize_version_ids(version_ids);
        let mut found = HashMap::with_capacity(ids.len());

        for chunk in ids.chunks(MAX_IDS_PER_VERSIONS_REQUEST) {
            for version in self.get_versions(chunk).await? {
                found.insert(version.id.clone(), version);
            }
        }

        Ok(found)
    }

    async fn get_versions(&self, version_ids: &[String]) -> Result<Vec<ModrinthVersion>> {
        let url = build_versions_url(&self.base_url, version_ids)?;
        let response = send_with_retry(|| self.http_client.get(url.clone()))
            .await
            .with_context(|| {
                format!(
                    "failed to query Modrinth metadata for {} versions",
                    version_ids.len()
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Modrinth returned an error for a {}-version metadata lookup",
                    version_ids.len()
                )
            })?;

        response.json::<Vec<ModrinthVersion>>().await.with_context(|| {
            format!(
                "failed to deserialize Modrinth metadata for {} versions",
                version_ids.len()
            )
        })
    }

    /// Latest version per sha1 hash for a whole modlist, in **three** requests
    /// instead of one per project.
    ///
    /// The result is keyed by hash, not by mod id: only the caller knows which
    /// hash belongs to which rule. A hash that Modrinth does not know, or for
    /// which no version exists on this target, is simply absent — the endpoint
    /// reports neither, and the returned map is the only source of truth.
    ///
    /// The channel cascade (`VERSION_TYPE_CASCADE`) reproduces
    /// `select_preferred_version`: a release wins over a newer beta, which a
    /// single request cannot express because `version_types` filters instead of
    /// ranking. Channels are asked one at a time and each stage only carries
    /// the hashes the previous ones left unanswered, so a channel with nothing
    /// left to ask costs no request.
    pub async fn fetch_latest_versions_by_hash(
        &self,
        sha1_hashes: &[String],
        target: &ResolutionTarget,
    ) -> Result<HashMap<String, ModrinthVersion>> {
        resolve_hash_cascade(sha1_hashes, |chunk, version_type| {
            self.post_version_files_update(chunk, target, version_type)
        })
        .await
    }

    /// Looks up whether each SHA-1 has at least one compatible version for the
    /// target.
    ///
    /// This caller only needs that existence boolean, for which the channel
    /// does not matter, so one unfiltered request per chunk is sufficient.
    /// Launching is different: the channel decides which jar is
    /// installed, so `fetch_latest_versions_by_hash` deliberately keeps its
    /// release → beta → alpha cascade. Do not unify these methods: they answer
    /// different questions.
    ///
    /// An omitted hash remains ambiguous: it can mean either that no version
    /// exists for the target or that Modrinth does not know the hash.
    pub async fn fetch_versions_by_hash_any_channel(
        &self,
        sha1_hashes: &[String],
        target: &ResolutionTarget,
    ) -> Result<HashMap<String, ModrinthVersion>> {
        resolve_hashes_any_channel(sha1_hashes, |chunk| {
            self.post_version_files_lookup(chunk, target)
        })
        .await
    }

    async fn post_version_files_lookup(
        &self,
        sha1_hashes: Vec<String>,
        target: &ResolutionTarget,
    ) -> Result<HashMap<String, ModrinthVersion>> {
        let url = build_version_files_update_url(&self.base_url)?;
        let body = build_version_files_lookup_body(&sha1_hashes, target);

        let response = send_with_retry(|| self.http_client.post(url.clone()).json(&body))
            .await
            .with_context(|| {
                format!(
                    "failed to query Modrinth compatible versions for {} hashes",
                    sha1_hashes.len()
                )
            })?
            .error_for_status()
            .with_context(|| {
                "Modrinth returned an error for a bulk compatible-version lookup"
            })?;

        response
            .json::<HashMap<String, ModrinthVersion>>()
            .await
            .with_context(|| {
                "failed to deserialize Modrinth bulk compatible-version lookup"
            })
    }

    async fn post_version_files_update(
        &self,
        sha1_hashes: Vec<String>,
        target: &ResolutionTarget,
        version_type: &'static str,
    ) -> Result<HashMap<String, ModrinthVersion>> {
        let url = build_version_files_update_url(&self.base_url)?;
        let body = build_version_files_update_body(&sha1_hashes, target, version_type);

        let response = send_with_retry(|| self.http_client.post(url.clone()).json(&body))
            .await
            .with_context(|| {
                format!(
                    "failed to query Modrinth {version_type} updates for {} hashes",
                    sha1_hashes.len()
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!("Modrinth returned an error for a {version_type} bulk version lookup")
            })?;

        response
            .json::<HashMap<String, ModrinthVersion>>()
            .await
            .with_context(|| {
                format!("failed to deserialize Modrinth {version_type} bulk version lookup")
            })
    }
}

impl Default for ModrinthClient {
    fn default() -> Self {
        Self::new()
    }
}

fn default_version_type() -> String {
    "alpha".to_string()
}

fn deserialize_version_type<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let version_type = Option::<String>::deserialize(deserializer)?;
    Ok(match version_type {
        Some(value) if matches!(value.as_str(), "release" | "beta" | "alpha") => value,
        _ => default_version_type(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModrinthVersion {
    pub id: String,
    pub project_id: String,
    pub version_number: String,
    pub name: String,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    // Missing or unknown channels default to alpha, keeping unrecognized builds at lowest priority.
    #[serde(
        default = "default_version_type",
        deserialize_with = "deserialize_version_type"
    )]
    pub version_type: String,
    #[serde(default)]
    pub dependencies: Vec<ModrinthDependency>,
    #[serde(default)]
    pub files: Vec<ModrinthFile>,
    pub date_published: String,
}

impl ModrinthVersion {
    pub(crate) fn channel_rank(&self) -> u8 {
        match self.version_type.as_str() {
            "release" => 2,
            "beta" => 1,
            _ => 0,
        }
    }

    pub fn primary_file(&self) -> Option<&ModrinthFile> {
        self.files
            .iter()
            .find(|file| file.primary)
            .or_else(|| self.files.first())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModrinthDependency {
    pub version_id: Option<String>,
    pub project_id: Option<String>,
    #[serde(rename = "dependency_type")]
    pub dependency_type: DependencyType,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModrinthFile {
    pub hashes: HashMap<String, String>,
    pub url: String,
    pub filename: String,
    pub primary: bool,
    pub size: u64,
}

/// Declared dependencies are deserialized but nothing in the launcher reads
/// them: dependencies are the user's to manage. The field stays declared so a
/// payload carrying it keeps deserializing, and an unrecognized
/// `dependency_type` maps to `Unknown` instead of failing the whole response —
/// with a single bulk request a rejected payload would cost the entire modlist,
/// not one mod.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum DependencyType {
    Required,
    Optional,
    Incompatible,
    Embedded,
    Unknown,
}

impl From<String> for DependencyType {
    fn from(value: String) -> Self {
        match value.as_str() {
            "required" => Self::Required,
            "optional" => Self::Optional,
            "incompatible" => Self::Incompatible,
            "embedded" => Self::Embedded,
            _ => Self::Unknown,
        }
    }
}

pub fn build_project_versions_url(
    base_url: &str,
    project_id: &str,
    target: &ResolutionTarget,
) -> Result<Url> {
    let sanitized_base_url = base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!(
        "{sanitized_base_url}/project/{project_id}/version"
    ))
    .with_context(|| format!("invalid Modrinth base URL '{base_url}'"))?;

    let loaders_json = serde_json::to_string(&vec![target.mod_loader.as_modrinth_loader()])?;
    let game_versions_json = serde_json::to_string(&vec![target.minecraft_version.clone()])?;

    url.query_pairs_mut()
        .append_pair("loaders", &loaders_json)
        .append_pair("game_versions", &game_versions_json);

    Ok(url)
}

pub fn build_version_url(base_url: &str, version_id: &str) -> Result<Url> {
    let sanitized_base_url = base_url.trim_end_matches('/');
    Url::parse(&format!("{sanitized_base_url}/version/{version_id}"))
        .with_context(|| format!("invalid Modrinth base URL '{base_url}'"))
}

/// `GET /versions?ids=["a","b"]`. The ids travel as a JSON array in the query
/// string, so this is where the request size comes from — see
/// `MAX_IDS_PER_VERSIONS_REQUEST`.
pub fn build_versions_url(base_url: &str, version_ids: &[String]) -> Result<Url> {
    let sanitized_base_url = base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{sanitized_base_url}/versions"))
        .with_context(|| format!("invalid Modrinth base URL '{base_url}'"))?;
    let ids_json = serde_json::to_string(version_ids)?;

    url.query_pairs_mut().append_pair("ids", &ids_json);

    Ok(url)
}

/// Version ids are base62 and **case-sensitive**, so unlike
/// `normalize_sha1_hashes` this must not change case. Blanks and duplicates are
/// dropped: the endpoint deduplicates anyway (measured 2026-09-07, two copies
/// of one id answer with one object), and a blank id would only lengthen the
/// URL.
pub(crate) fn normalize_version_ids(version_ids: &[String]) -> Vec<String> {
    let mut normalized = Vec::with_capacity(version_ids.len());
    let mut seen = HashSet::with_capacity(version_ids.len());

    for version_id in version_ids {
        let trimmed = version_id.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed) {
            normalized.push(trimmed.to_string());
        }
    }

    normalized
}

pub fn build_version_files_update_url(base_url: &str) -> Result<Url> {
    let sanitized_base_url = base_url.trim_end_matches('/');
    Url::parse(&format!("{sanitized_base_url}/version_files/update"))
        .with_context(|| format!("invalid Modrinth base URL '{base_url}'"))
}

/// Body of `POST /version_files/update`. `loaders` is always populated: an
/// absent or empty array is not "no filter wanted" but "no filter", and the
/// endpoint then answers with builds for other loaders.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct VersionFilesUpdateRequest<'a> {
    hashes: &'a [String],
    algorithm: &'static str,
    loaders: [&'static str; 1],
    game_versions: [&'a str; 1],
    version_types: [&'a str; 1],
}

/// Kept separate from `VersionFilesUpdateRequest` so the installation lookup
/// cannot accidentally lose its required channel filter.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct VersionFilesLookupRequest<'a> {
    hashes: &'a [String],
    algorithm: &'static str,
    loaders: [&'static str; 1],
    game_versions: [&'a str; 1],
}

pub(crate) fn build_version_files_update_body<'a>(
    hashes: &'a [String],
    target: &'a ResolutionTarget,
    version_type: &'a str,
) -> VersionFilesUpdateRequest<'a> {
    VersionFilesUpdateRequest {
        hashes,
        algorithm: "sha1",
        loaders: [target.mod_loader.as_modrinth_loader()],
        game_versions: [target.minecraft_version.as_str()],
        version_types: [version_type],
    }
}

pub(crate) fn build_version_files_lookup_body<'a>(
    hashes: &'a [String],
    target: &'a ResolutionTarget,
) -> VersionFilesLookupRequest<'a> {
    VersionFilesLookupRequest {
        hashes,
        algorithm: "sha1",
        loaders: [target.mod_loader.as_modrinth_loader()],
        game_versions: [target.minecraft_version.as_str()],
    }
}

/// Modrinth matches hashes case-sensitively: a valid sha1 sent uppercase is
/// omitted from the response exactly like an unknown one, with no error. So the
/// caller's input is normalized here instead of being trusted. Duplicates and
/// blanks are dropped because they can only waste body bytes.
pub(crate) fn normalize_sha1_hashes(hashes: &[String]) -> Vec<String> {
    let mut normalized = Vec::with_capacity(hashes.len());
    let mut seen = HashSet::with_capacity(hashes.len());

    for hash in hashes {
        let trimmed = hash.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lowercase = trimmed.to_ascii_lowercase();
        if seen.insert(lowercase.clone()) {
            normalized.push(lowercase);
        }
    }

    normalized
}

pub(crate) fn hash_chunks(hashes: &[String], max_per_request: usize) -> Vec<&[String]> {
    if hashes.is_empty() {
        return Vec::new();
    }
    hashes.chunks(max_per_request.max(1)).collect()
}

/// Fold one cascade stage into the accumulated map and return the hashes still
/// unanswered, in the original order. Earlier stages win, so a hash resolved on
/// `release` is never overwritten by `beta`. Response keys that were not asked
/// for are ignored: the returned map is the only source of truth about what was
/// found, and nothing else validates it.
pub(crate) fn apply_cascade_stage(
    pending: &[String],
    mut stage: HashMap<String, ModrinthVersion>,
    found: &mut HashMap<String, ModrinthVersion>,
) -> Vec<String> {
    let mut still_pending = Vec::new();

    for hash in pending {
        if found.contains_key(hash) {
            continue;
        }
        match stage.remove(hash) {
            Some(version) => {
                found.insert(hash.clone(), version);
            }
            None => still_pending.push(hash.clone()),
        }
    }

    still_pending
}

/// Drives the channel cascade: normalize, then for each channel ask only the
/// hashes still unanswered, chunked as a guard, and let earlier channels win.
/// `fetch_stage` is the only part that talks to the network, which is what
/// makes the sequencing testable: the driver decides how many requests happen
/// and with which hashes, and an empty residual set costs no request.
pub(crate) async fn resolve_hash_cascade<F, Fut>(
    sha1_hashes: &[String],
    mut fetch_stage: F,
) -> Result<HashMap<String, ModrinthVersion>>
where
    F: FnMut(Vec<String>, &'static str) -> Fut,
    Fut: Future<Output = Result<HashMap<String, ModrinthVersion>>>,
{
    let mut pending = normalize_sha1_hashes(sha1_hashes);
    let mut found = HashMap::with_capacity(pending.len());

    for version_type in VERSION_TYPE_CASCADE {
        if pending.is_empty() {
            break;
        }

        let mut stage = HashMap::new();
        for chunk in hash_chunks(&pending, MAX_HASHES_PER_UPDATE_REQUEST) {
            stage.extend(fetch_stage(chunk.to_vec(), version_type).await?);
        }

        pending = apply_cascade_stage(&pending, stage, &mut found);
    }

    Ok(found)
}

/// Drives the availability lookup: normalize once, split only at the request
/// guard, and preserve the endpoint's map as the complete account of which
/// hashes were found.
pub(crate) async fn resolve_hashes_any_channel<F, Fut>(
    sha1_hashes: &[String],
    mut fetch_stage: F,
) -> Result<HashMap<String, ModrinthVersion>>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<HashMap<String, ModrinthVersion>>>,
{
    let normalized = normalize_sha1_hashes(sha1_hashes);
    let mut found = HashMap::with_capacity(normalized.len());

    for chunk in hash_chunks(&normalized, MAX_HASHES_PER_UPDATE_REQUEST) {
        found.extend(fetch_stage(chunk.to_vec()).await?);
    }

    Ok(found)
}

pub fn filter_compatible_versions(
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
) -> Vec<ModrinthVersion> {
    versions
        .iter()
        .filter(|version| is_version_compatible(version, target))
        .cloned()
        .collect()
}

pub fn is_version_compatible(version: &ModrinthVersion, target: &ResolutionTarget) -> bool {
    version
        .game_versions
        .iter()
        .any(|game_version| mc_version_matches(game_version, &target.minecraft_version))
        && version
            .loaders
            .iter()
            .any(|loader| loader == target.mod_loader.as_modrinth_loader())
}

pub fn select_latest_compatible_version(
    versions: &[ModrinthVersion],
    target: &ResolutionTarget,
) -> Option<ModrinthVersion> {
    let mut compatible = filter_compatible_versions(versions, target);
    sort_versions_by_target_preference(&mut compatible, target);
    compatible.into_iter().next()
}

impl ModLoader {
    pub fn as_modrinth_loader(self) -> &'static str {
        match self {
            ModLoader::Fabric => "fabric",
            ModLoader::NeoForge => "neoforge",
            ModLoader::Forge => "forge",
            ModLoader::Quilt => "quilt",
            ModLoader::Vanilla => "vanilla",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::{
        build_project_versions_url, build_version_files_lookup_body,
        build_version_files_update_body, build_version_files_update_url, build_version_url,
        build_versions_url, filter_compatible_versions, normalize_version_ids,
        resolve_hash_cascade, resolve_hashes_any_channel, select_latest_compatible_version,
        sort_versions_by_target_preference, DependencyType, ModrinthVersion,
        MAX_HASHES_PER_UPDATE_REQUEST,
    };
    use crate::resolver::{ModLoader, ResolutionTarget};

    fn target() -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        }
    }

    fn sample_versions_json() -> &'static str {
        r#"[
          {
            "id": "version-old",
            "project_id": "sodium",
            "version_number": "0.5.9",
            "name": "Sodium 0.5.9",
            "version_type": "release",
            "game_versions": ["1.21.1"],
            "loaders": ["fabric"],
            "date_published": "2024-06-01T10:00:00.000Z",
            "dependencies": [
              {
                "version_id": null,
                "project_id": "fabric-api",
                "dependency_type": "required",
                "file_name": null
              }
            ],
            "files": [
              {
                "hashes": { "sha1": "abc" },
                "url": "https://cdn.modrinth.com/data/sodium/version-old.jar",
                "filename": "sodium-old.jar",
                "primary": true,
                "size": 12345
              }
            ]
          },
          {
            "id": "version-new",
            "project_id": "sodium",
            "version_number": "0.6.0",
            "name": "Sodium 0.6.0",
            "version_type": "release",
            "game_versions": ["1.21.1"],
            "loaders": ["fabric"],
            "date_published": "2024-08-01T10:00:00.000Z",
            "dependencies": [],
            "files": [
              {
                "hashes": { "sha1": "def" },
                "url": "https://cdn.modrinth.com/data/sodium/version-new.jar",
                "filename": "sodium-new.jar",
                "primary": false,
                "size": 67890
              },
              {
                "hashes": { "sha1": "ghi" },
                "url": "https://cdn.modrinth.com/data/sodium/version-new-primary.jar",
                "filename": "sodium-new-primary.jar",
                "primary": true,
                "size": 67900
              }
            ]
          },
          {
            "id": "version-wrong-loader",
            "project_id": "sodium",
            "version_number": "0.6.0-neoforge",
            "name": "Sodium NeoForge",
            "version_type": "release",
            "game_versions": ["1.21.1"],
            "loaders": ["neoforge"],
            "date_published": "2024-09-01T10:00:00.000Z",
            "dependencies": [],
            "files": []
          }
        ]"#
    }

    #[test]
    fn builds_modrinth_versions_url_with_expected_filters() {
        let url = build_project_versions_url("https://api.modrinth.com/v2", "sodium", &target())
            .expect("url should build");

        assert_eq!(
            url.as_str(),
            "https://api.modrinth.com/v2/project/sodium/version?loaders=%5B%22fabric%22%5D&game_versions=%5B%221.21.1%22%5D"
        );
    }

    #[test]
    fn builds_modrinth_single_version_url() {
        let url =
            build_version_url("https://api.modrinth.com/v2", "abc123").expect("url should build");

        assert_eq!(url.as_str(), "https://api.modrinth.com/v2/version/abc123");
    }

    #[test]
    fn builds_modrinth_bulk_versions_url_with_the_ids_as_a_json_array() {
        let url = build_versions_url(
            "https://api.modrinth.com/v2",
            &["abc123".to_string(), "def456".to_string()],
        )
        .expect("url should build");

        assert_eq!(
            url.as_str(),
            "https://api.modrinth.com/v2/versions?ids=%5B%22abc123%22%2C%22def456%22%5D"
        );
    }

    #[test]
    fn version_ids_are_normalized_without_changing_their_case() {
        let normalized = normalize_version_ids(&[
            "  AbC123 ".to_string(),
            "abc123".to_string(),
            String::new(),
            "   ".to_string(),
            "AbC123".to_string(),
        ]);

        assert_eq!(normalized, vec!["AbC123".to_string(), "abc123".to_string()]);
    }

    #[test]
    fn deserializes_version_payload_with_dependencies_and_files() {
        let versions: Vec<ModrinthVersion> =
            serde_json::from_str(sample_versions_json()).expect("json should deserialize");

        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].dependencies.len(), 1);
        assert_eq!(
            versions[0].dependencies[0].dependency_type,
            DependencyType::Required
        );
        assert_eq!(versions[0].files[0].filename, "sodium-old.jar");
    }

    #[test]
    fn unknown_or_missing_version_type_defaults_to_alpha() {
        let deserialize = |version_type: Option<&str>| {
            let mut value = serde_json::json!({
                "id": "version",
                "project_id": "project",
                "version_number": "1.0.0",
                "name": "Version",
                "game_versions": ["1.21.1"],
                "loaders": ["fabric"],
                "date_published": "2024-06-01T10:00:00.000Z"
            });
            if let Some(version_type) = version_type {
                value["version_type"] = version_type.into();
            }
            serde_json::from_value::<ModrinthVersion>(value).expect("version should deserialize")
        };

        assert_eq!(deserialize(None).version_type, "alpha");
        assert_eq!(deserialize(Some("snapshot")).version_type, "alpha");
    }

    #[test]
    fn filters_versions_by_target_loader_and_game_version() {
        let versions: Vec<ModrinthVersion> =
            serde_json::from_str(sample_versions_json()).expect("json should deserialize");

        let compatible = filter_compatible_versions(&versions, &target());

        assert_eq!(compatible.len(), 2);
        assert!(compatible
            .iter()
            .all(|version| version.loaders.contains(&"fabric".into())));
    }

    #[test]
    fn selects_most_recent_compatible_version() {
        let versions: Vec<ModrinthVersion> =
            serde_json::from_str(sample_versions_json()).expect("json should deserialize");

        let selected =
            select_latest_compatible_version(&versions, &target()).expect("version should exist");

        assert_eq!(selected.id, "version-new");
        assert_eq!(
            selected
                .primary_file()
                .expect("primary file should exist")
                .filename,
            "sodium-new-primary.jar"
        );
    }

    #[test]
    fn target_preference_uses_release_before_newer_beta() {
        let versions: Vec<ModrinthVersion> =
            serde_json::from_str(sample_versions_json()).expect("json should deserialize");
        let mut compatible = filter_compatible_versions(&versions, &target());
        compatible
            .iter_mut()
            .find(|version| version.id == "version-new")
            .expect("new version should exist")
            .version_type = "beta".into();

        sort_versions_by_target_preference(&mut compatible, &target());

        assert_eq!(compatible[0].id, "version-old");
    }

    #[test]
    fn prefers_exact_target_line_over_newer_patch_line() {
        let target = ResolutionTarget {
            minecraft_version: "1.21.6".into(),
            mod_loader: ModLoader::Fabric,
        };
        let mut versions = vec![
            ModrinthVersion {
                id: "future".into(),
                project_id: "c2me-fabric".into(),
                version_number: "0.3.4.0.0+1.21.8".into(),
                name: "Future line".into(),
                game_versions: vec!["1.21.x".into()],
                loaders: vec!["fabric".into()],
                version_type: "alpha".into(),
                dependencies: Vec::new(),
                files: vec![super::ModrinthFile {
                    hashes: HashMap::new(),
                    url: "https://example.invalid/future.jar".into(),
                    filename: "c2me-fabric-mc1.21.8-0.3.4.0.0.jar".into(),
                    primary: true,
                    size: 1,
                }],
                date_published: "2026-04-01T10:00:00.000Z".into(),
            },
            ModrinthVersion {
                id: "target".into(),
                project_id: "c2me-fabric".into(),
                version_number: "0.3.4+alpha.0.19+1.21.6".into(),
                name: "Target line".into(),
                game_versions: vec!["1.21.x".into()],
                loaders: vec!["fabric".into()],
                version_type: "alpha".into(),
                dependencies: Vec::new(),
                files: vec![super::ModrinthFile {
                    hashes: HashMap::new(),
                    url: "https://example.invalid/target.jar".into(),
                    filename: "c2me-fabric-mc1.21.6-0.3.4.jar".into(),
                    primary: true,
                    size: 1,
                }],
                date_published: "2026-03-01T10:00:00.000Z".into(),
            },
        ];

        sort_versions_by_target_preference(&mut versions, &target);

        assert_eq!(versions[0].id, "target");
    }

    fn bulk_version(id: &str, version_type: &str, date_published: &str) -> ModrinthVersion {
        ModrinthVersion {
            id: id.into(),
            project_id: "project".into(),
            version_number: id.into(),
            name: id.into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            version_type: version_type.into(),
            dependencies: Vec::new(),
            files: Vec::new(),
            date_published: date_published.into(),
        }
    }

    /// Stands in for the network stage: records every request the cascade
    /// issues and answers like the endpoint does, i.e. only for the hashes it
    /// was asked about, omitting everything else without an error.
    struct StageRecorder {
        replies: HashMap<&'static str, Vec<(String, ModrinthVersion)>>,
        calls: RefCell<Vec<(&'static str, Vec<String>)>>,
    }

    impl StageRecorder {
        fn new(replies: Vec<(&'static str, Vec<(String, ModrinthVersion)>)>) -> Self {
            Self {
                replies: replies.into_iter().collect(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn reply(
            &self,
            requested: Vec<String>,
            version_type: &'static str,
        ) -> HashMap<String, ModrinthVersion> {
            self.calls
                .borrow_mut()
                .push((version_type, requested.clone()));

            self.replies
                .get(version_type)
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|(hash, _)| requested.contains(hash))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        }

        fn channels(&self) -> Vec<&'static str> {
            self.calls
                .borrow()
                .iter()
                .map(|(version_type, _)| *version_type)
                .collect()
        }

        fn requested(&self) -> Vec<Vec<String>> {
            self.calls
                .borrow()
                .iter()
                .map(|(_, hashes)| hashes.clone())
                .collect()
        }
    }

    async fn run_cascade(
        hashes: &[String],
        recorder: &StageRecorder,
    ) -> HashMap<String, ModrinthVersion> {
        resolve_hash_cascade(hashes, |chunk, version_type| {
            let stage = recorder.reply(chunk, version_type);
            async move { Ok(stage) }
        })
        .await
        .expect("cascade should succeed")
    }

    #[test]
    fn builds_version_files_update_url() {
        let url = build_version_files_update_url("https://api.modrinth.com/v2/")
            .expect("url should build");

        assert_eq!(
            url.as_str(),
            "https://api.modrinth.com/v2/version_files/update"
        );
    }

    #[test]
    fn update_body_pins_algorithm_loader_and_single_channel() {
        let hashes = vec!["aa".to_string(), "bb".to_string()];
        let target = target();
        let body = build_version_files_update_body(&hashes, &target, "beta");

        assert_eq!(
            serde_json::to_value(&body).expect("body should serialize"),
            serde_json::json!({
                "hashes": ["aa", "bb"],
                "algorithm": "sha1",
                "loaders": ["fabric"],
                "game_versions": ["1.21.1"],
                "version_types": ["beta"]
            })
        );
    }

    #[test]
    fn lookup_body_omits_channel_filter_while_update_body_keeps_it() {
        let hashes = vec!["aa".to_string(), "bb".to_string()];
        let target = target();
        let lookup = serde_json::to_value(build_version_files_lookup_body(&hashes, &target))
            .expect("lookup body should serialize");
        let update =
            serde_json::to_value(build_version_files_update_body(&hashes, &target, "beta"))
                .expect("update body should serialize");

        assert_eq!(
            lookup,
            serde_json::json!({
                "hashes": ["aa", "bb"],
                "algorithm": "sha1",
                "loaders": ["fabric"],
                "game_versions": ["1.21.1"]
            })
        );
        assert!(!lookup.as_object().expect("body should be an object").contains_key("version_types"));
        assert!(update.as_object().expect("body should be an object").contains_key("version_types"));
    }

    #[tokio::test]
    async fn any_channel_uses_one_request_for_all_hashes() {
        let hashes = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        let mut calls = Vec::new();

        resolve_hashes_any_channel(&hashes, |chunk| {
            calls.push(chunk);
            async { Ok(HashMap::new()) }
        })
        .await
        .expect("lookup should succeed");

        assert_eq!(calls, vec![hashes]);
    }

    #[tokio::test]
    async fn any_channel_lowercases_deduplicates_and_discards_blank_hashes() {
        let hashes = vec![
            " AABBCC ".to_string(),
            "aabbcc".to_string(),
            String::new(),
            "   ".to_string(),
        ];
        let mut calls = Vec::new();

        resolve_hashes_any_channel(&hashes, |chunk| {
            calls.push(chunk);
            async { Ok(HashMap::new()) }
        })
        .await
        .expect("lookup should succeed");

        assert_eq!(calls, vec![vec!["aabbcc".to_string()]]);
    }

    #[tokio::test]
    async fn any_channel_issues_no_request_without_usable_hashes() {
        let hashes = vec![String::new(), "   ".to_string()];
        let mut calls = Vec::new();

        let found = resolve_hashes_any_channel(&hashes, |chunk| {
            calls.push(chunk);
            async { Ok(HashMap::new()) }
        })
        .await
        .expect("lookup should succeed");

        assert!(calls.is_empty());
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn any_channel_keeps_omitted_hashes_absent() {
        let hashes = vec!["known".to_string(), "omitted".to_string()];

        let found = resolve_hashes_any_channel(&hashes, |_| async {
            Ok(HashMap::from([(
                "known".to_string(),
                bulk_version("known-version", "alpha", "2025-01-01"),
            )]))
        })
        .await
        .expect("lookup should succeed");

        assert_eq!(found["known"].id, "known-version");
        assert!(!found.contains_key("omitted"));
    }

    #[tokio::test]
    async fn any_channel_splits_a_hash_set_above_the_request_guard() {
        let hashes: Vec<String> = (0..MAX_HASHES_PER_UPDATE_REQUEST + 7)
            .map(|index| format!("{index:040x}"))
            .collect();
        let mut request_sizes = Vec::new();

        resolve_hashes_any_channel(&hashes, |chunk| {
            request_sizes.push(chunk.len());
            async { Ok(HashMap::new()) }
        })
        .await
        .expect("lookup should succeed");

        assert_eq!(
            request_sizes,
            vec![MAX_HASHES_PER_UPDATE_REQUEST, 7]
        );
    }

    #[tokio::test]
    async fn cascade_stops_at_the_first_channel_that_answers_everything() {
        let hashes = vec!["aaa".to_string(), "bbb".to_string()];
        let recorder = StageRecorder::new(vec![(
            "release",
            vec![
                ("aaa".to_string(), bulk_version("a-rel", "release", "2025-01-01")),
                ("bbb".to_string(), bulk_version("b-rel", "release", "2025-01-02")),
            ],
        )]);

        let found = run_cascade(&hashes, &recorder).await;

        assert_eq!(recorder.channels(), vec!["release"]);
        assert_eq!(found["aaa"].id, "a-rel");
        assert_eq!(found["bbb"].id, "b-rel");
    }

    #[tokio::test]
    async fn cascade_keeps_an_older_release_over_a_newer_beta() {
        let hashes = vec!["aaa".to_string()];
        let recorder = StageRecorder::new(vec![
            (
                "release",
                vec![(
                    "aaa".to_string(),
                    bulk_version("old-release", "release", "2025-02-20T00:00:00Z"),
                )],
            ),
            (
                "beta",
                vec![(
                    "aaa".to_string(),
                    bulk_version("new-beta", "beta", "2026-06-13T00:00:00Z"),
                )],
            ),
        ]);

        let found = run_cascade(&hashes, &recorder).await;

        assert_eq!(recorder.channels(), vec!["release"]);
        assert_eq!(found["aaa"].id, "old-release");
    }

    #[tokio::test]
    async fn cascade_asks_later_channels_only_for_the_hashes_still_unanswered() {
        let hashes = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        let recorder = StageRecorder::new(vec![
            (
                "release",
                vec![(
                    "aaa".to_string(),
                    bulk_version("a-rel", "release", "2025-01-01"),
                )],
            ),
            (
                "beta",
                vec![("bbb".to_string(), bulk_version("b-beta", "beta", "2025-02-01"))],
            ),
            (
                "alpha",
                vec![(
                    "ccc".to_string(),
                    bulk_version("c-alpha", "alpha", "2025-03-01"),
                )],
            ),
        ]);

        let found = run_cascade(&hashes, &recorder).await;

        assert_eq!(recorder.channels(), vec!["release", "beta", "alpha"]);
        assert_eq!(
            recorder.requested(),
            vec![
                vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()],
                vec!["bbb".to_string(), "ccc".to_string()],
                vec!["ccc".to_string()],
            ]
        );
        assert_eq!(found["aaa"].id, "a-rel");
        assert_eq!(found["bbb"].id, "b-beta");
        assert_eq!(found["ccc"].id, "c-alpha");
    }

    #[tokio::test]
    async fn cascade_returns_an_empty_map_when_no_channel_knows_the_hash() {
        let hashes = vec!["deadbeef".to_string()];
        let recorder = StageRecorder::new(Vec::new());

        let found = run_cascade(&hashes, &recorder).await;

        assert_eq!(recorder.channels(), vec!["release", "beta", "alpha"]);
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn cascade_issues_no_request_without_usable_hashes() {
        let recorder = StageRecorder::new(Vec::new());

        let found = run_cascade(&[" ".to_string(), String::new()], &recorder).await;

        assert!(recorder.channels().is_empty());
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn cascade_lowercases_and_deduplicates_the_requested_hashes() {
        let hashes = vec![
            "AABBCC".to_string(),
            " aabbcc ".to_string(),
            "DDEEFF".to_string(),
        ];
        let recorder = StageRecorder::new(vec![(
            "release",
            vec![(
                "aabbcc".to_string(),
                bulk_version("a-rel", "release", "2025-01-01"),
            )],
        )]);

        let found = run_cascade(&hashes, &recorder).await;

        assert_eq!(
            recorder.requested()[0],
            vec!["aabbcc".to_string(), "ddeeff".to_string()]
        );
        assert_eq!(found["aabbcc"].id, "a-rel");
        assert!(!found.contains_key("AABBCC"));
    }

    #[tokio::test]
    async fn cascade_splits_a_hash_set_above_the_request_guard() {
        let hashes: Vec<String> = (0..MAX_HASHES_PER_UPDATE_REQUEST + 7)
            .map(|index| format!("{index:040x}"))
            .collect();
        let recorder = StageRecorder::new(Vec::new());

        run_cascade(&hashes, &recorder).await;

        let release_sizes: Vec<usize> = recorder
            .calls
            .borrow()
            .iter()
            .filter(|(version_type, _)| *version_type == "release")
            .map(|(_, requested)| requested.len())
            .collect();

        assert_eq!(release_sizes, vec![MAX_HASHES_PER_UPDATE_REQUEST, 7]);
        assert_eq!(
            release_sizes.iter().sum::<usize>(),
            MAX_HASHES_PER_UPDATE_REQUEST + 7
        );
    }

    #[test]
    fn unknown_dependency_type_does_not_fail_the_whole_payload() {
        let value = serde_json::json!({
            "id": "version",
            "project_id": "project",
            "version_number": "1.0.0",
            "name": "Version",
            "game_versions": ["1.21.1"],
            "loaders": ["fabric"],
            "version_type": "release",
            "date_published": "2026-06-01T10:00:00.000Z",
            "dependencies": [
                { "project_id": "other", "dependency_type": "suggested" },
                { "project_id": "fabric-api", "dependency_type": "required" }
            ]
        });

        let version: ModrinthVersion =
            serde_json::from_value(value).expect("version should deserialize");

        assert_eq!(version.dependencies[0].dependency_type, DependencyType::Unknown);
        assert_eq!(
            version.dependencies[1].dependency_type,
            DependencyType::Required
        );
    }
}
