use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use crate::adoptium::{
    host_adoptium_os, normalize_adoptium_architecture, plan_runtime_download, AdoptiumClient,
};
use crate::java_runtime::{
    discover_java_installations, persist_java_installations, select_java_for_requirement,
    CommandJavaBinaryInspector, JavaBinaryInspector,
};
use crate::launcher_paths::LauncherPaths;
use crate::launch_command::PreparedLaunchCommand;
use crate::loader_metadata::{
    LibraryDownloadArtifact, LoaderLibrary, LoaderMetadata, LoaderMetadataClient,
};
use crate::offline_account::deterministic_offline_uuid;
use crate::path_safety::{contained_join, validate_path_component};
use crate::process_streaming::{spawn_and_stream_process, ProcessEventSink, ProcessLogStream};
use crate::resolver::{ModLoader, ResolutionTarget};

use super::artifacts::extract_artifact_name;
use super::{
    download_file, emit_log, emit_progress, EffectiveLaunchSettings, LaunchPlaceholders,
    LoggingProcessEventSink, PlayerIdentity, StartedLaunch, ACTIVE_MC_PID,
};

pub(super) async fn materialize_loader_libraries(
    http_client: &reqwest::Client,
    library_root: &Path,
    loader_metadata: &LoaderMetadata,
) -> Result<Vec<PathBuf>> {
    materialize_loader_artifacts(http_client, library_root, &loader_metadata.libraries).await
}

pub(super) async fn materialize_loader_maven_files(
    http_client: &reqwest::Client,
    library_root: &Path,
    loader_metadata: &LoaderMetadata,
) -> Result<Vec<PathBuf>> {
    materialize_loader_artifacts(http_client, library_root, &loader_metadata.maven_files).await
}

async fn materialize_loader_artifacts(
    http_client: &reqwest::Client,
    library_root: &Path,
    artifacts: &[LoaderLibrary],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();

    for library in artifacts {
        let relative_path = relative_loader_library_path(library)?;
        let destination_path = contained_loader_library_path(library_root, &relative_path)?;

        if !destination_path.exists() {
            // Determine the download URL: prefer explicit download artifact,
            // fall back to constructing from Maven base URL + coordinates.
            let download_url = if let Some(download) = &library.download {
                if download.url.trim().is_empty() {
                    continue;
                }
                download.url.clone()
            } else if let Some(base_url) = &library.url {
                let base = base_url.trim_end_matches('/');
                format!(
                    "{base}/{}",
                    relative_path.to_string_lossy().replace('\\', "/")
                )
            } else {
                continue; // no way to obtain this library
            };

            download_file(http_client, &download_url, &destination_path).await?;
        }

        paths.push(destination_path);
    }

    Ok(paths)
}

pub(super) fn contained_loader_library_path(
    library_root: &Path,
    relative_path: &Path,
) -> Result<PathBuf> {
    let relative_path = relative_path
        .to_str()
        .context("loader library path is not valid UTF-8")?;
    contained_join(library_root, relative_path)
}

pub(super) async fn prepare_forge_wrapper_launch(
    http_client: &reqwest::Client,
    library_root: &Path,
    minecraft_jar_path: &Path,
    loader_metadata: &mut LoaderMetadata,
) -> Result<Option<ForgeWrapperLaunchPreparation>> {
    let Some(installer_artifact) = forge_wrapper_installer_artifact(loader_metadata)? else {
        return Ok(None);
    };

    let installer_path =
        contained_loader_library_path(library_root, &installer_artifact.relative_path)?;
    if !installer_path.exists() {
        download_file(http_client, &installer_artifact.url, &installer_path).await?;
    }

    loader_metadata.jvm_arguments.extend([
        format!("-Dforgewrapper.installer={}", installer_path.display()),
        format!("-Dforgewrapper.librariesDir={}", library_root.display()),
        format!("-Dforgewrapper.minecraft={}", minecraft_jar_path.display()),
    ]);

    let profile_libraries = read_forge_installer_profile_libraries(&installer_path)?;
    let profile_library_paths =
        materialize_loader_artifacts(http_client, library_root, &profile_libraries).await?;

    Ok(Some(ForgeWrapperLaunchPreparation {
        installer_path,
        profile_library_paths,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForgeWrapperLaunchPreparation {
    pub(super) installer_path: PathBuf,
    pub(super) profile_library_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForgeWrapperInstallerArtifact {
    pub(super) url: String,
    pub(super) relative_path: PathBuf,
}

pub(super) fn forge_wrapper_installer_artifact(
    loader_metadata: &LoaderMetadata,
) -> Result<Option<ForgeWrapperInstallerArtifact>> {
    match loader_metadata.mod_loader {
        ModLoader::Forge => {
            let version = loader_metadata
                .libraries
                .iter()
                .chain(loader_metadata.maven_files.iter())
                .filter_map(|library| parse_maven_coordinates(&library.name))
                .find_map(|coordinates| {
                    (coordinates.group == "net.minecraftforge" && coordinates.artifact == "forge")
                        .then_some(coordinates.version.to_string())
                })
                .with_context(|| {
                    format!(
                        "Forge metadata did not include a net.minecraftforge:forge library for {}",
                        loader_metadata.minecraft_version
                    )
                })?;

            Ok(Some(ForgeWrapperInstallerArtifact {
                url: format!(
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/{version}/forge-{version}-installer.jar"
                ),
                relative_path: PathBuf::from("net")
                    .join("minecraftforge")
                    .join("forge")
                    .join(&version)
                    .join(format!("forge-{version}-installer.jar")),
            }))
        }
        ModLoader::NeoForge => {
            let version = loader_metadata.loader_version.trim();
            anyhow::ensure!(
                !version.is_empty(),
                "NeoForge metadata did not include a loader version"
            );

            Ok(Some(ForgeWrapperInstallerArtifact {
                url: format!(
                    "https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar"
                ),
                relative_path: PathBuf::from("net")
                    .join("neoforged")
                    .join("neoforge")
                    .join(version)
                    .join(format!("neoforge-{version}-installer.jar")),
            }))
        }
        _ => Ok(None),
    }
}

pub(super) fn read_forge_installer_profile_libraries(
    installer_path: &Path,
) -> Result<Vec<LoaderLibrary>> {
    let file = File::open(installer_path).with_context(|| {
        format!(
            "failed to open Forge installer {}",
            installer_path.display()
        )
    })?;
    let mut archive = zip::ZipArchive::new(file).with_context(|| {
        format!(
            "failed to read Forge installer {}",
            installer_path.display()
        )
    })?;
    let mut version_json = String::new();
    archive
        .by_name("version.json")
        .with_context(|| {
            format!(
                "Forge installer {} did not contain version.json",
                installer_path.display()
            )
        })?
        .read_to_string(&mut version_json)
        .with_context(|| {
            format!(
                "failed to read version.json from Forge installer {}",
                installer_path.display()
            )
        })?;

    forge_profile_libraries_from_json(&version_json)
}

pub(super) fn forge_profile_libraries_from_json(contents: &str) -> Result<Vec<LoaderLibrary>> {
    let profile = serde_json::from_str::<ForgeInstallerVersionProfile>(contents)
        .context("failed to parse Forge installer version.json")?;
    Ok(profile
        .libraries
        .into_iter()
        .filter_map(|library| {
            let artifact = library.downloads?.artifact?;
            Some(LoaderLibrary {
                name: library.name,
                url: None,
                download: Some(LibraryDownloadArtifact {
                    url: artifact.url,
                    path: artifact.path,
                    sha1: artifact.sha1,
                    size: artifact.size,
                }),
            })
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct ForgeInstallerVersionProfile {
    #[serde(default)]
    libraries: Vec<ForgeInstallerProfileLibrary>,
}

#[derive(Debug, Deserialize)]
struct ForgeInstallerProfileLibrary {
    name: String,
    #[serde(default)]
    downloads: Option<ForgeInstallerProfileDownloads>,
}

#[derive(Debug, Deserialize)]
struct ForgeInstallerProfileDownloads {
    #[serde(default)]
    artifact: Option<ForgeInstallerProfileArtifact>,
}

#[derive(Debug, Deserialize)]
struct ForgeInstallerProfileArtifact {
    url: String,
    path: Option<String>,
    sha1: Option<String>,
    size: Option<u64>,
}

struct MavenCoordinates<'a> {
    group: &'a str,
    artifact: &'a str,
    version: &'a str,
}

fn parse_maven_coordinates(coordinates: &str) -> Option<MavenCoordinates<'_>> {
    let mut parts = coordinates.split(':');
    Some(MavenCoordinates {
        group: parts.next()?,
        artifact: parts.next()?,
        version: parts.next()?,
    })
}

pub(super) fn relative_loader_library_path(library: &LoaderLibrary) -> Result<PathBuf> {
    if let Some(download) = &library.download {
        if let Some(path) = &download.path {
            return Ok(PathBuf::from(path));
        }
    }

    maven_artifact_relative_path(&library.name)
}

pub(super) fn maven_artifact_relative_path(coordinates: &str) -> Result<PathBuf> {
    let parts = coordinates.split(':').collect::<Vec<_>>();
    if parts.len() < 3 {
        bail!("invalid Maven coordinates '{coordinates}'");
    }

    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = parts.get(3).copied();
    let file_name = match classifier {
        Some(classifier) if !classifier.trim().is_empty() => {
            format!("{artifact}-{version}-{classifier}.jar")
        }
        _ => format!("{artifact}-{version}.jar"),
    };

    Ok(PathBuf::from(group)
        .join(artifact)
        .join(version)
        .join(file_name))
}

pub(super) async fn fetch_launch_loader_metadata(
    target: &ResolutionTarget,
) -> Result<LoaderMetadata> {
    LoaderMetadataClient::new()
        .fetch_loader_metadata(&target.minecraft_version, target.mod_loader)
        .await
        .map_err(Into::into)
}

pub(super) fn spawn_minecraft_process(
    app_handle: tauri::AppHandle,
    launch_log_session: Arc<super::LaunchLogSession>,
    prepared_command: PreparedLaunchCommand,
) -> Result<StartedLaunch> {
    let sink: Arc<dyn ProcessEventSink> = Arc::new(LoggingProcessEventSink::new(
        app_handle.clone(),
        launch_log_session.clone(),
    ));
    let process = spawn_and_stream_process(prepared_command, sink)?;

    let pid = process.pid;
    *ACTIVE_MC_PID.lock().unwrap() = Some(pid);

    emit_progress(
        &app_handle,
        "running",
        100,
        "Launching Minecraft",
        &format!("Minecraft process started with PID {}.", pid),
    )?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!("[Launch] Spawned Minecraft process with PID {}", pid),
    )?;

    let app_for_wait = app_handle.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = process.wait();
        *ACTIVE_MC_PID.lock().unwrap() = None;
        super::set_active_launch_log_session(None);
        let _ = emit_progress(&app_for_wait, "idle", 0, "Ready", "Minecraft has exited.");
    });

    Ok(StartedLaunch {
        pid,
        launch_log_dir: launch_log_session.dir().to_path_buf(),
    })
}

pub(super) fn filter_minecraft_launch_game_arguments(arguments: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut skip_next = false;

    for argument in arguments {
        if skip_next {
            skip_next = false;
            continue;
        }

        if argument.starts_with("--quickPlay") {
            skip_next = true;
            continue;
        }

        if argument == "--demo" {
            continue;
        }

        filtered.push(argument.clone());
    }

    filtered
}

pub(super) fn merge_minecraft_and_loader_game_arguments(
    minecraft_arguments: &[String],
    loader_arguments: Vec<String>,
) -> Vec<String> {
    let loader_options = loader_arguments
        .iter()
        .filter(|argument| argument.starts_with("--"))
        .cloned()
        .collect::<HashSet<_>>();
    let minecraft_arguments = filter_minecraft_launch_game_arguments(minecraft_arguments);
    let mut merged = Vec::with_capacity(minecraft_arguments.len() + loader_arguments.len());
    let mut index = 0;

    while index < minecraft_arguments.len() {
        let argument = &minecraft_arguments[index];
        if loader_options.contains(argument) {
            index += if minecraft_option_consumes_value(argument)
                && index + 1 < minecraft_arguments.len()
            {
                2
            } else {
                1
            };
            continue;
        }

        merged.push(argument.clone());
        index += 1;
    }

    merged.extend(loader_arguments);
    merged
}

fn minecraft_option_consumes_value(argument: &str) -> bool {
    argument.starts_with("--") && argument != "--demo"
}

pub(super) fn merge_minecraft_and_loader_jvm_arguments(
    minecraft_arguments: &[String],
    loader_arguments: Vec<String>,
) -> Vec<String> {
    let mut merged = filter_minecraft_launch_jvm_arguments(minecraft_arguments);
    merged.extend(loader_arguments);

    if !merged
        .iter()
        .any(|argument| argument.starts_with("-Djava.library.path="))
    {
        merged.push("-Djava.library.path=${natives_directory}".to_string());
    }
    if !merged
        .iter()
        .any(|argument| argument.starts_with("-Dorg.lwjgl.librarypath="))
    {
        merged.push("-Dorg.lwjgl.librarypath=${natives_directory}".to_string());
    }

    merged
}

fn filter_minecraft_launch_jvm_arguments(arguments: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut skip_next = false;

    for argument in arguments {
        if skip_next {
            skip_next = false;
            continue;
        }

        if minecraft_jvm_option_is_classpath(argument) {
            skip_next = true;
            continue;
        }

        if argument == "${classpath}" {
            continue;
        }

        filtered.push(argument.clone());
    }

    filtered
}

fn minecraft_jvm_option_is_classpath(argument: &str) -> bool {
    matches!(argument, "-cp" | "-classpath" | "--class-path")
}

pub(super) fn build_modded_classpath_entries(
    minecraft_library_paths: &[PathBuf],
    loader_library_paths: Vec<PathBuf>,
    client_jar_path: PathBuf,
) -> Vec<PathBuf> {
    let mut seen_loader_paths = HashSet::new();
    let loader_library_paths = loader_library_paths
        .into_iter()
        .filter(|path| seen_loader_paths.insert(path.clone()))
        .collect::<Vec<_>>();

    let loader_artifacts: HashSet<String> = loader_library_paths
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(extract_artifact_name)
        })
        .collect();
    let mut entries: Vec<PathBuf> = minecraft_library_paths
        .iter()
        .filter(|path| {
            let dominated = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| loader_artifacts.contains(&extract_artifact_name(name)))
                .unwrap_or(false);
            !dominated
        })
        .cloned()
        .collect();
    entries.extend(loader_library_paths);
    entries.push(client_jar_path);
    entries
}

pub(super) fn build_instance_root(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    target: &ResolutionTarget,
) -> Result<PathBuf> {
    validate_path_component(modlist_name)?;
    Ok(launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join("instances")
        .join(format!(
            "{}-{}",
            target.minecraft_version,
            target.mod_loader.as_modrinth_loader()
        )))
}

pub(super) async fn load_player_identity(launcher_paths: &LauncherPaths) -> Result<PlayerIdentity> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;

    // Read the raw active account; no decryption is needed because we use profile_data.
    // Extract all data from DB BEFORE any async work (Connection is not Send).
    let raw_account = {
        use crate::microsoft_auth::AccountsRepository as RawRepo;
        RawRepo::new(&connection)
            .load_active_account()
            .ok()
            .flatten()
    };
    let db_path = launcher_paths.database_path().to_path_buf();
    drop(connection); // Release connection before async work.

    if let Some(raw) = raw_account {
        let profile = raw
            .profile_data
            .as_deref()
            .and_then(|pd| serde_json::from_str::<serde_json::Value>(pd).ok());

        let username = profile
            .as_ref()
            .and_then(|v| v.get("username").and_then(|u| u.as_str()).map(String::from))
            .or(raw.xbox_gamertag.clone())
            .unwrap_or_else(|| "CubicPlayer".to_string());

        let uuid = format_uuid_with_dashes(
            &raw.minecraft_uuid
                .unwrap_or_else(|| deterministic_offline_uuid(&username).to_string()),
        );

        let microsoft_id = raw.microsoft_id.clone();

        // Try to refresh the Minecraft access token using the stored MS refresh token.
        let ms_refresh = profile.as_ref().and_then(|v| {
            v.get("ms_refresh_token")
                .and_then(|t| t.as_str())
                .map(String::from)
        });

        if let Some(refresh_token) = ms_refresh {
            if !refresh_token.is_empty() {
                eprintln!(
                    "[Auth] Has refresh token (len={}), attempting refresh...",
                    refresh_token.len()
                );

                let env_path = launcher_paths.root_dir().join(".env");
                let Some(client_id) =
                    crate::microsoft_auth::configured_microsoft_client_id(&env_path)
                        .ok()
                        .flatten()
                else {
                    eprintln!(
                        "[Auth] Microsoft token refresh skipped: MICROSOFT_CLIENT_ID is not configured"
                    );
                    return Ok(PlayerIdentity {
                        username,
                        uuid,
                        access_token: "0".to_string(),
                        user_type: "offline".to_string(),
                        version_type: "Cubic".to_string(),
                    });
                };

                let config = crate::microsoft_auth::MicrosoftOAuthConfig {
                    client_id,
                    redirect_uri: crate::microsoft_auth::REGISTERED_LOOPBACK_REDIRECT_URI
                        .to_string(),
                    scopes: vec!["XboxLive.signin".into(), "offline_access".into()],
                };
                let oauth_client = crate::microsoft_auth::MicrosoftOAuthClient::new();

                match oauth_client
                    .refresh_access_token(&config, &refresh_token)
                    .await
                {
                    Ok(ms_tokens) => {
                        eprintln!("[Auth] MS token refresh OK, authenticating with Xbox/MC...");
                        let chain = crate::microsoft_auth::MinecraftAuthChain::new();
                        match chain
                            .authenticate(
                                &ms_tokens.access_token,
                                ms_tokens.refresh_token.as_deref(),
                                ms_tokens.user_id.as_deref(),
                            )
                            .await
                        {
                            Ok(login) => {
                                eprintln!(
                                    "[Auth] Full auth chain OK: username={}",
                                    login.minecraft_username
                                );
                                // Update stored tokens in profile_data.
                                let new_profile = serde_json::json!({
                                    "username": login.minecraft_username,
                                    "uuid": login.minecraft_uuid,
                                    "mc_access_token": login.minecraft_access_token,
                                    "ms_refresh_token": ms_tokens.refresh_token.as_deref()
                                        .unwrap_or(&refresh_token),
                                });
                                if let Ok(conn) = Connection::open(&db_path) {
                                    use crate::microsoft_auth::AccountsRepository as RawRepo;
                                    let _ = RawRepo::new(&conn).update_profile_data(
                                        &microsoft_id,
                                        &new_profile.to_string(),
                                    );
                                }

                                return Ok(PlayerIdentity {
                                    username: login.minecraft_username,
                                    uuid: format_uuid_with_dashes(&login.minecraft_uuid),
                                    access_token: login.minecraft_access_token,
                                    user_type: "msa".to_string(),
                                    version_type: "Cubic".to_string(),
                                });
                            }
                            Err(e) => eprintln!("[Auth] MC auth chain failed: {e:#}"),
                        }
                    }
                    Err(e) => eprintln!("[Auth] MS token refresh failed: {e:#}"),
                }
            }
        }

        eprintln!("[Auth] Falling back to offline mode");
        // No refresh token or refresh failed; use offline mode with the correct name.
        return Ok(PlayerIdentity {
            username,
            uuid,
            access_token: "0".to_string(),
            user_type: "offline".to_string(),
            version_type: "Cubic".to_string(),
        });
    }

    // No active account at all; use fully offline mode.
    Ok(PlayerIdentity {
        username: "CubicPlayer".to_string(),
        uuid: deterministic_offline_uuid("CubicPlayer").to_string(),
        access_token: "0".to_string(),
        user_type: "offline".to_string(),
        version_type: "Cubic".to_string(),
    })
}

pub(super) async fn select_or_download_java(
    app_handle: &tauri::AppHandle,
    launcher_paths: &LauncherPaths,
    settings: &EffectiveLaunchSettings,
    target: &ResolutionTarget,
    required_version: u32,
) -> Result<PathBuf> {
    if let Some(path) = &settings.java_path_override {
        let inspector = CommandJavaBinaryInspector;
        let probe = inspector
            .inspect(path)
            .with_context(|| format!("failed to inspect Java override at {}", path.display()))?
            .with_context(|| format!("Java override '{}' is not runnable", path.display()))?;
        if probe.version < required_version {
            bail!(
                "Java override '{}' reports version {}, but launch requires Java {} or higher for Minecraft {}",
                path.display(),
                probe.version,
                required_version,
                target.minecraft_version
            );
        }

        return Ok(path.clone());
    }

    let installations = discover_java_installations(launcher_paths.java_runtimes_dir())?;
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    persist_java_installations(&connection, &installations)?;

    if let Some(installation) = select_java_for_requirement(&installations, required_version) {
        return Ok(installation.path);
    }

    // No suitable Java found; auto-download via Adoptium.
    emit_log(
        app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Java] No Java {} found, downloading from Adoptium...",
            required_version
        ),
    )?;
    emit_progress(
        app_handle,
        "resolving",
        85,
        "Downloading Java",
        &format!("Fetching Java {} runtime from Adoptium.", required_version),
    )?;

    let adoptium = AdoptiumClient::new();
    let os = host_adoptium_os();
    let arch = normalize_adoptium_architecture(std::env::consts::ARCH);

    let package = adoptium
        .fetch_latest_jre_package(required_version, os, arch)
        .await?
        .with_context(|| {
            format!(
                "Adoptium has no JRE {} for {}/{}",
                required_version, os, arch
            )
        })?;

    let plan = plan_runtime_download(
        launcher_paths.java_runtimes_dir(),
        required_version,
        package,
        os,
        arch,
    )?;

    // Download the archive.
    adoptium
        .download_package(&plan.package, &plan.archive_path)
        .await?;

    // Extract the archive into the install directory.
    emit_log(
        app_handle,
        ProcessLogStream::Stdout,
        format!("[Java] Extracting to {}", plan.install_dir.display()),
    )?;
    let archive_path = plan.archive_path.clone();
    let install_dir = plan.install_dir.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::open(&archive_path)
            .with_context(|| format!("failed to open Java archive {}", archive_path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read Java archive {}", archive_path.display()))?;
        archive.extract(&install_dir).with_context(|| {
            format!(
                "failed to extract Java archive to {}",
                install_dir.display()
            )
        })?;
        // Clean up archive file.
        let _ = std::fs::remove_file(&archive_path);
        Ok(())
    })
    .await
    .context("Java extraction task panicked")??;

    // Re-scan and select.
    let installations = discover_java_installations(launcher_paths.java_runtimes_dir())?;
    persist_java_installations(&connection, &installations)?;

    select_java_for_requirement(&installations, required_version)
        .map(|installation| installation.path)
        .with_context(|| {
            format!(
                "Java {} was downloaded but could not be found after extraction. Check {}",
                required_version,
                launcher_paths.java_runtimes_dir().display()
            )
        })
}

pub(super) fn format_uuid_with_dashes(uuid: &str) -> String {
    let clean = uuid.replace('-', "");
    if clean.len() == 32 {
        format!(
            "{}-{}-{}-{}-{}",
            &clean[0..8],
            &clean[8..12],
            &clean[12..16],
            &clean[16..20],
            &clean[20..32]
        )
    } else {
        uuid.to_string()
    }
}

pub(super) fn substitute_loader_placeholders(
    loader_metadata: &mut LoaderMetadata,
    placeholders: &LaunchPlaceholders,
) {
    loader_metadata.jvm_arguments = loader_metadata
        .jvm_arguments
        .iter()
        .map(|argument| substitute_known_placeholders(argument, placeholders))
        .collect();
    loader_metadata.game_arguments = loader_metadata
        .game_arguments
        .iter()
        .map(|argument| substitute_known_placeholders(argument, placeholders))
        .collect();
}

pub(super) fn substitute_known_placeholders(
    argument: &str,
    placeholders: &LaunchPlaceholders,
) -> String {
    [
        (
            "${auth_player_name}",
            placeholders.auth_player_name.as_str(),
        ),
        ("${version_name}", placeholders.version_name.as_str()),
        ("${game_directory}", placeholders.game_directory.as_str()),
        ("${assets_root}", placeholders.assets_root.as_str()),
        (
            "${assets_index_name}",
            placeholders.assets_index_name.as_str(),
        ),
        ("${auth_uuid}", placeholders.auth_uuid.as_str()),
        (
            "${auth_access_token}",
            placeholders.auth_access_token.as_str(),
        ),
        ("${user_type}", placeholders.user_type.as_str()),
        ("${version_type}", placeholders.version_type.as_str()),
        (
            "${library_directory}",
            placeholders.library_directory.as_str(),
        ),
        (
            "${natives_directory}",
            placeholders.natives_directory.as_str(),
        ),
        ("${launcher_name}", placeholders.launcher_name.as_str()),
        (
            "${launcher_version}",
            placeholders.launcher_version.as_str(),
        ),
        (
            "${classpath_separator}",
            placeholders.classpath_separator.as_str(),
        ),
        (
            "${resolution_width}",
            placeholders.resolution_width.as_str(),
        ),
        (
            "${resolution_height}",
            placeholders.resolution_height.as_str(),
        ),
    ]
    .into_iter()
    .fold(argument.to_string(), |current, (needle, replacement)| {
        current.replace(needle, replacement)
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use zip::write::FileOptions;

    use super::{forge_profile_libraries_from_json, read_forge_installer_profile_libraries};

    fn forge_version_json() -> &'static str {
        r#"{
          "libraries": [
            {
              "name": "org.apache.logging.log4j:log4j-core:2.15.0",
              "downloads": {
                "artifact": {
                  "path": "org/apache/logging/log4j/log4j-core/2.15.0/log4j-core-2.15.0.jar",
                  "url": "https://libraries.minecraft.net/org/apache/logging/log4j/log4j-core/2.15.0/log4j-core-2.15.0.jar",
                  "sha1": "ba55c13d7ac2fd44df9cc8074455719a33f375b9",
                  "size": 1789769
                }
              }
            },
            {
              "name": "net.minecraftforge:forge:1.16.5-36.2.34",
              "downloads": {
                "artifact": {
                  "path": "net/minecraftforge/forge/1.16.5-36.2.34/forge-1.16.5-36.2.34.jar",
                  "url": "",
                  "sha1": "b05f056824252928b59c746f8a28fe2cf36db6c0",
                  "size": 212608
                }
              }
            }
          ]
        }"#
    }

    #[test]
    fn forge_profile_libraries_parse_download_artifacts() {
        let libraries = forge_profile_libraries_from_json(forge_version_json())
            .expect("Forge version profile should parse");

        assert_eq!(libraries.len(), 2);
        assert_eq!(
            libraries[0].name,
            "org.apache.logging.log4j:log4j-core:2.15.0"
        );
        let download = libraries[0]
            .download
            .as_ref()
            .expect("log4j should have a download artifact");
        assert_eq!(
            download.path.as_deref(),
            Some("org/apache/logging/log4j/log4j-core/2.15.0/log4j-core-2.15.0.jar")
        );
        assert_eq!(
            download.sha1.as_deref(),
            Some("ba55c13d7ac2fd44df9cc8074455719a33f375b9")
        );
        assert_eq!(download.size, Some(1789769));
    }

    #[test]
    fn forge_profile_libraries_read_from_installer_jar() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cubic-forge-installer-profile-{unique}-{}.jar",
            std::process::id()
        ));

        {
            let file = std::fs::File::create(&path).expect("test jar should be created");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file("version.json", FileOptions::default())
                .expect("version.json entry should start");
            archive
                .write_all(forge_version_json().as_bytes())
                .expect("version.json should be written");
            archive.finish().expect("test jar should finish");
        }

        let libraries =
            read_forge_installer_profile_libraries(&path).expect("installer profile should parse");
        std::fs::remove_file(&path).ok();

        assert!(libraries
            .iter()
            .any(|library| library.name == "org.apache.logging.log4j:log4j-core:2.15.0"));
    }
}
