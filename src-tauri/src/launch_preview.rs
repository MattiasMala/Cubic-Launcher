use anyhow::{bail, Context, Result};
use tauri::State;

// Launch orchestration entry point.
//
// The sibling `launch_preview_*` files are focused slices of the launch
// pipeline: cache handling, dependency resolution, loader metadata, logging,
// runtime setup, and verification. Keeping this file as the command-facing
// orchestrator makes launch behavior easier to audit without returning to one
// very large mixed-responsibility file.
use crate::app_shell::load_shell_snapshot_from_root;
use crate::dependencies::DependencyResolution;
use crate::instance_configs::{prepare_instance_config_directory, CachedConfigPlacement};
use crate::instance_mods::prepare_instance_mods_directory;
use crate::java_runtime::required_java_version_for_minecraft;
use crate::launch_command::{build_launch_command, JavaLaunchRequest, JavaLaunchSettings};
use crate::launcher_paths::LauncherPaths;
use crate::minecraft_downloader::{ensure_minecraft_version, extract_natives};
use crate::mod_cache::cached_artifact_path_for_pending_download;
use crate::modrinth::ModrinthClient;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{resolve_modlist, ModLoader, ResolutionTarget};
use crate::rules::ModSource;

use std::sync::Mutex;

#[path = "launch_preview_fabric.rs"]
mod fabric;
use fabric::*;

#[path = "launch_preview_fabric_versions.rs"]
mod fabric_versions;
use fabric_versions::*;

#[path = "launch_preview_dependencies.rs"]
mod dependencies;
use dependencies::*;

#[path = "launch_preview_artifacts.rs"]
mod artifacts;
use artifacts::*;

#[path = "launch_preview_cache.rs"]
mod cache;
use cache::*;


#[path = "launch_preview_models.rs"]
mod models;
use models::*;
pub use models::{LaunchRequest, LaunchVerificationRequest, LaunchVerificationResult};

#[path = "launch_preview_logging.rs"]
mod logging;
use logging::*;

#[path = "launch_preview_verification.rs"]
mod verification;
use verification::*;
pub use verification::{automation_mode_enabled, maybe_start_automation_verifier};

#[path = "launch_preview_content.rs"]
mod content;
use content::*;

#[path = "launch_preview_vanilla.rs"]
mod vanilla;
use vanilla::*;

#[path = "launch_preview_runtime.rs"]
mod runtime;
use runtime::*;

pub const LAUNCH_PROGRESS_EVENT: &str = "launch-progress";

/// Tracks the PID of the currently running Minecraft process so it can be killed.
static ACTIVE_MC_PID: Mutex<Option<u32>> = Mutex::new(None);

#[tauri::command]
pub fn start_launch_command(
    app_handle: tauri::AppHandle,
    launcher_paths: State<'_, LauncherPaths>,
    request: LaunchRequest,
) -> Result<(), String> {
    let launcher_paths = launcher_paths.inner().clone();

    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_launch_pipeline(app_handle.clone(), launcher_paths, request).await {
            let detail = format!("{error:#}");
            let _ = emit_launch_failure(&app_handle, &detail);
            set_active_launch_log_session(None);
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn verify_launch_command(
    app_handle: tauri::AppHandle,
    launcher_paths: State<'_, LauncherPaths>,
    request: LaunchVerificationRequest,
) -> Result<LaunchVerificationResult, String> {
    run_launch_verification(app_handle, launcher_paths.inner().clone(), request)
        .await
        .map_err(|error| error.to_string())
}

/// Kills the currently running Minecraft process, if any.
#[tauri::command]
pub fn stop_minecraft_command() -> Result<(), String> {
    let pid = ACTIVE_MC_PID.lock().unwrap().take();
    if let Some(pid) = pid {
        terminate_minecraft_pid(pid).map_err(|error| error.to_string())
    } else {
        Err("No Minecraft process is running.".into())
    }
}

pub(super) fn current_minecraft_pid() -> Option<u32> {
    *ACTIVE_MC_PID.lock().unwrap()
}

pub(super) fn emit_launch_failure(app_handle: &tauri::AppHandle, detail: &str) -> Result<()> {
    emit_log(
        app_handle,
        ProcessLogStream::Stderr,
        format!("[Launch] {detail}"),
    )?;
    emit_progress(
        app_handle,
        "idle",
        0,
        "Launch Aborted",
        "Launch preparation stopped before Minecraft could start.",
    )?;
    emit_launcher_issue(
        app_handle,
        "Launch failed",
        "The launcher could not finish preparing the selected mod list.",
        detail,
        "error",
        "launch",
    )
}

pub(super) fn terminate_minecraft_pid(pid: u32) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let output = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .with_context(|| format!("failed to terminate Minecraft PID {pid}"))?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "failed to terminate Minecraft PID {pid}: {}",
                if stderr.is_empty() {
                    "taskkill returned a non-zero exit code"
                } else {
                    &stderr
                }
            )
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            bail!("failed to terminate Minecraft PID {pid}")
        }
    }
}

pub(in crate::launch_preview) async fn run_launch_pipeline(
    app_handle: tauri::AppHandle,
    launcher_paths: LauncherPaths,
    request: LaunchRequest,
) -> Result<StartedLaunch> {
    let target = ResolutionTarget {
        minecraft_version: request.minecraft_version.trim().to_string(),
        mod_loader: parse_mod_loader(&request.mod_loader)?,
    };
    let modlist_name = request.modlist_name.trim().to_string();
    anyhow::ensure!(!modlist_name.is_empty(), "modlist_name cannot be empty");
    let launch_log_session = LaunchLogSession::create(&launcher_paths, &modlist_name, &target)?;
    set_active_launch_log_session(Some(launch_log_session.clone()));

    emit_progress(
        &app_handle,
        "resolving",
        10,
        "Resolve Rules",
        &format!(
            "Evaluating '{}' for {} / {}.",
            modlist_name, target.minecraft_version, request.mod_loader
        ),
    )?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Launcher] Starting launch for '{}' on {} / {}",
            modlist_name, target.minecraft_version, request.mod_loader
        ),
    )?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Launcher] Writing launch logs to {}",
            launch_log_session.dir().display()
        ),
    )?;

    let shell_snapshot =
        load_shell_snapshot_from_root(launcher_paths.root_dir(), Some(&modlist_name))?;
    let effective_settings = EffectiveLaunchSettings::from_shell_settings(
        &shell_snapshot.global_settings,
        &shell_snapshot.selected_modlist_overrides,
    );

    let http_client = reqwest::Client::new();
    if target.mod_loader == ModLoader::Vanilla {
        return run_vanilla_launch_pipeline(
            app_handle,
            launcher_paths,
            modlist_name,
            target,
            launch_log_session,
            effective_settings,
            http_client,
        )
        .await;
    }

    let modlist = load_modlist(&launcher_paths, &modlist_name)?;
    let modrinth_client = ModrinthClient::new();
    let resolution = resolve_modlist(&modlist, &target)?;

    // Verify Modrinth availability for resolved mods. If a resolved mod has no
    // compatible version on Modrinth, temporarily disable it and re-resolve so
    // alternatives get a chance. In cache-only mode, a valid cached artifact
    // also counts as available.
    let resolution = if effective_settings.cache_only_mode {
        let selected = collect_selected_mods(&modlist, &resolution, &target);
        let available_artifacts =
            resolve_selected_remote_artifacts(&launcher_paths, &selected, &target).await?;
        let unavailable = selected
            .iter()
            .filter(|selected| matches!(selected.source, ModSource::Modrinth))
            .filter(|selected| !available_artifacts.contains_key(&selected.mod_id))
            .map(|selected| selected.mod_id.clone())
            .collect::<Vec<_>>();

        if unavailable.is_empty() {
            resolution
        } else {
            let mut patched = modlist.clone();
            for uid in &unavailable {
                if let Some(rule) = patched.find_rule_mut(uid) {
                    rule.enabled = false;
                }
            }
            resolve_modlist(&patched, &target)?
        }
    } else {
        let selected = collect_selected_mods(&modlist, &resolution, &target);
        let versions = prefetch_compatible_versions_for_selected(
            &app_handle,
            &launcher_paths,
            &http_client,
            &selected,
            &modrinth_client,
            &target,
        )
        .await?;
        let unavailable = selected
            .iter()
            .filter(|selected| matches!(selected.source, ModSource::Modrinth))
            .filter(|selected| !versions.contains_key(&selected.mod_id))
            .map(|selected| selected.mod_id.clone())
            .collect::<Vec<_>>();

        if unavailable.is_empty() {
            resolution
        } else {
            let mut patched = modlist.clone();
            for uid in &unavailable {
                if let Some(rule) = patched.find_rule_mut(uid) {
                    rule.enabled = false;
                }
            }
            resolve_modlist(&patched, &target)?
        }
    };
    log_resolution(&app_handle, &resolution)?;

    let selected_mods = collect_selected_mods(&modlist, &resolution, &target);
    launch_log_session.write_selected_mods(&selected_mods)?;
    if selected_mods.len() > 300 {
        let detail = if effective_settings.cache_only_mode {
            "You have more than 300 mods. The first launch may still take 1-2 minutes, but cache-only mode is already enabled for future launches."
        } else {
            "You have more than 300 mods. The first launch may take 1-2 minutes due to API rate limits. You can enable \"Cache-Only Mode\" in Settings to skip API checks on future launches."
        };
        emit_launcher_issue(
            &app_handle,
            "Large modpack detected",
            "Launch preparation may take longer than usual.",
            detail,
            "warning",
            "launch",
        )?;
    }

    let (
        all_remote_versions,
        cached_remote_records,
        dependency_resolution,
        effective_required_java,
        project_aliases,
    ) = if effective_settings.cache_only_mode {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            "[Cache] Cache-only mode enabled. Reusing cached artifacts only; uncached mods are skipped."
                .to_string(),
        )?;

        let parent_artifacts =
            resolve_selected_remote_artifacts(&launcher_paths, &selected_mods, &target).await?;
        let parent_artifact_values = parent_artifacts.into_values().collect::<Vec<_>>();
        let (parent_versions, cached_parent_records) =
            split_remote_artifacts(&parent_artifact_values);

        // DESIGN: the launcher does not manage dependencies. Detect & report the
        // requirements the selected mods DECLARE (best-effort over the live
        // parent versions; cached-only records carry no dep metadata offline),
        // then download ONLY what the user selected.
        let notices = detect_dependency_notices(
            &launcher_paths,
            &http_client,
            &modrinth_client,
            &target,
            &parent_versions,
        )
        .await;
        emit_dependency_notices(&app_handle, &notices)?;

        (
            parent_versions,
            cached_parent_records,
            DependencyResolution::default(),
            required_java_version_for_minecraft(&target.minecraft_version)?,
            Vec::new(),
        )
    } else {
        let compatible_versions = prefetch_compatible_versions_for_selected(
            &app_handle,
            &launcher_paths,
            &http_client,
            &selected_mods,
            &modrinth_client,
            &target,
        )
        .await?;
        let parent_versions = selected_mods
            .iter()
            .filter(|selected| matches!(selected.source, ModSource::Modrinth))
            .filter_map(|selected| compatible_versions.get(&selected.mod_id))
            .cloned()
            .collect::<Vec<_>>();
        let project_aliases = selected_mods
            .iter()
            .filter(|selected| matches!(selected.source, ModSource::Modrinth))
            .filter_map(|selected| {
                compatible_versions
                    .get(&selected.mod_id)
                    .map(|version| (selected.mod_id.clone(), version.project_id.clone()))
            })
            .collect::<Vec<_>>();

        // DESIGN: no auto-resolution, no pinning, no parent exclusion, no
        // constraint-solving. Each selected mod is the latest exact-tagged
        // version, independent of what the others declare. We DETECT declared
        // requirements (Modrinth metadata incl. version_id pins + embedded
        // fabric.mod.json predicates) and REPORT them; the user decides.
        let notices = detect_dependency_notices(
            &launcher_paths,
            &http_client,
            &modrinth_client,
            &target,
            &parent_versions,
        )
        .await;
        emit_dependency_notices(&app_handle, &notices)?;

        (
            parent_versions,
            Vec::new(),
            DependencyResolution::default(),
            required_java_version_for_minecraft(&target.minecraft_version)?,
            project_aliases,
        )
    };
    launch_log_session.write_dependency_summary(&dependency_resolution)?;
    launch_log_session.write_resolved_versions(&all_remote_versions, &cached_remote_records)?;
    launch_log_session.append_summary_line(&format!(
        "cache_only_mode={}",
        effective_settings.cache_only_mode
    ))?;
    launch_log_session.append_summary_line(&format!("selected_mods={}", selected_mods.len()))?;
    launch_log_session.append_summary_line(&format!(
        "resolved_remote_versions={}",
        all_remote_versions.len()
    ))?;
    launch_log_session.append_summary_line(&format!(
        "resolved_cached_records={}",
        cached_remote_records.len()
    ))?;
    launch_log_session.append_summary_line(&format!(
        "required_java_before_loader={}",
        effective_required_java
    ))?;
    emit_progress(
        &app_handle,
        "resolving",
        42,
        "Check Cache",
        "Inspecting cached mods and downloading missing dependencies.",
    )?;

    let acquisition_plan = if effective_settings.cache_only_mode {
        build_remote_acquisition_plan_from_artifacts(
            &launcher_paths,
            &all_remote_versions,
            &cached_remote_records,
            &target,
        )?
    } else {
        build_remote_acquisition_plan(&launcher_paths, &all_remote_versions, &target)?
    };
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Cache] {} cached, {} pending download",
            acquisition_plan.cached.len(),
            acquisition_plan.to_download.len()
        ),
    )?;
    launch_log_session.write_cache_plan(&acquisition_plan)?;

    download_pending_artifacts(
        &app_handle,
        &http_client,
        launcher_paths.mods_cache_dir(),
        &acquisition_plan
            .to_download
            .iter()
            .map(|download| DownloadArtifact {
                filename: download.jar_filename.clone(),
                url: download.download_url.clone(),
                destination_path: cached_artifact_path_for_pending_download(
                    launcher_paths.mods_cache_dir(),
                    download,
                ),
                file_hash: download.file_hash.clone(),
                file_size: download.file_size,
            })
            .collect::<Vec<_>>(),
    )
    .await?;
    persist_remote_versions_and_dependencies(
        &launcher_paths,
        &all_remote_versions,
        &target,
        &dependency_resolution.links,
        &project_aliases,
    )?;

    emit_progress(
        &app_handle,
        "resolving",
        58,
        "Download Minecraft",
        &format!(
            "Ensuring Minecraft {} client, libraries and assets are cached.",
            target.minecraft_version
        ),
    )?;

    let mc_data = ensure_minecraft_version(
        &http_client,
        &target.minecraft_version,
        &launcher_paths,
        |label, detail| {
            let _ = emit_log(
                &app_handle,
                ProcessLogStream::Stdout,
                format!("[MC] {label}: {detail}"),
            );
        },
    )
    .await?;

    emit_progress(
        &app_handle,
        "resolving",
        74,
        "Prepare Instance",
        "Refreshing mods, loader libraries, configs and launch metadata.",
    )?;

    let instance_root = build_instance_root(&launcher_paths, &modlist_name, &target)?;
    let instance_mods_dir = instance_root.join("mods");
    let instance_config_dir = instance_root.join("config");
    let instance_natives_dir = instance_root.join("natives");
    let instance_library_dir = instance_root.join("libraries");

    std::fs::create_dir_all(&instance_natives_dir)
        .with_context(|| format!("failed to create {}", instance_natives_dir.display()))?;

    let cached_mod_jars = build_cached_mod_jars(
        &app_handle,
        &selected_mods,
        &all_remote_versions,
        &cached_remote_records,
        &target,
        &launcher_paths,
        &modlist_name,
    )?;
    launch_log_session.write_final_mod_set(&cached_mod_jars)?;
    prepare_instance_mods_directory(
        launcher_paths.mods_cache_dir(),
        &instance_mods_dir,
        &cached_mod_jars,
    )?;
    prepare_instance_config_directory(
        launcher_paths.configs_cache_dir(),
        &instance_config_dir,
        &Vec::<CachedConfigPlacement>::new(),
    )?;

    // Download and install content packs (resource packs, data packs, shaders)
    resolve_and_install_content_packs(
        &app_handle,
        &launcher_paths,
        &http_client,
        &modrinth_client,
        &modlist_name,
        &target,
        &instance_root,
    )
    .await?;

    let mut loader_metadata = fetch_launch_loader_metadata(&target).await?;
    let mut loader_library_paths =
        materialize_loader_libraries(&http_client, &instance_library_dir, &loader_metadata).await?;
    let loader_maven_file_paths =
        materialize_loader_maven_files(&http_client, &instance_library_dir, &loader_metadata)
            .await?;
    if !loader_maven_file_paths.is_empty() {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Loader] Prepared {} loader Maven file(s)",
                loader_maven_file_paths.len()
            ),
        )?;
    }
    if let Some(preparation) = prepare_forge_wrapper_launch(
        &http_client,
        &instance_library_dir,
        &mc_data.client_jar_path,
        &mut loader_metadata,
    )
    .await?
    {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Loader] Prepared ForgeWrapper installer {}",
                preparation.installer_path.display()
            ),
        )?;
        if !preparation.profile_library_paths.is_empty() {
            emit_log(
                &app_handle,
                ProcessLogStream::Stdout,
                format!(
                    "[Loader] Prepared {} Forge profile runtime librar{}",
                    preparation.profile_library_paths.len(),
                    if preparation.profile_library_paths.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                ),
            )?;
            loader_library_paths.extend(preparation.profile_library_paths);
        }
    }

    extract_natives(&mc_data.native_paths, &instance_natives_dir)?;

    if target.mod_loader == ModLoader::Vanilla {
        loader_metadata.main_class = mc_data.main_class.clone();
        loader_metadata.jvm_arguments = mc_data.jvm_arguments.clone();
        loader_metadata.game_arguments = mc_data.game_arguments.clone();
    } else {
        // For modded loaders (Fabric, Forge, etc.): prepend essential MC game
        // arguments (auth, version, directories) but skip quickPlay entries
        // which require specific values and cause conflicts.
        loader_metadata.game_arguments = merge_minecraft_and_loader_game_arguments(
            &mc_data.game_arguments,
            loader_metadata.game_arguments.drain(..).collect(),
        );
        loader_metadata.jvm_arguments = merge_minecraft_and_loader_jvm_arguments(
            &mc_data.jvm_arguments,
            loader_metadata.jvm_arguments.drain(..).collect(),
        );
    }

    emit_progress(
        &app_handle,
        "resolving",
        90,
        "Authenticating",
        "Refreshing Minecraft session...",
    )?;
    let player_identity = load_player_identity(&launcher_paths).await?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Auth] username={}, uuid={}, user_type={}, token_len={}",
            player_identity.username,
            player_identity.uuid,
            player_identity.user_type,
            player_identity.access_token.len()
        ),
    )?;
    let placeholders = LaunchPlaceholders::new(
        &player_identity,
        &modlist_name,
        &target,
        &instance_root,
        &mc_data.assets_dir,
        &mc_data.asset_index_id,
        &instance_library_dir,
        &instance_natives_dir,
    );
    substitute_loader_placeholders(&mut loader_metadata, &placeholders);

    let effective_required_java =
        effective_required_java.max(required_java_for_cached_mod_jars(&cached_mod_jars)?);
    let effective_required_java = effective_required_java.max(
        loader_metadata
            .min_java_version
            .unwrap_or(effective_required_java),
    );
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Java] Effective runtime requirement is Java {}",
            effective_required_java
        ),
    )?;
    launch_log_session.append_summary_line(&format!(
        "required_java_after_loader={}",
        effective_required_java
    ))?;
    let java_binary_path = select_or_download_java(
        &app_handle,
        &launcher_paths,
        &effective_settings,
        &target,
        effective_required_java,
    )
    .await?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!("[Java] Using {}", java_binary_path.display()),
    )?;

    let prepared_command = build_launch_command(&JavaLaunchRequest {
        java_binary_path: java_binary_path.clone(),
        working_directory: instance_root.clone(),
        classpath_entries: build_modded_classpath_entries(
            &mc_data.library_paths,
            loader_library_paths,
            mc_data.client_jar_path.clone(),
        ),
        loader_metadata,
        launch_settings: JavaLaunchSettings {
            min_ram_mb: effective_settings.min_ram_mb,
            max_ram_mb: effective_settings.max_ram_mb,
            custom_jvm_args: effective_settings.custom_jvm_args.clone(),
            profiler: None,
            wrapper_command: effective_settings.wrapper_command.clone(),
        },
        additional_game_arguments: Vec::new(),
        config_attribution: None,
    })?;

    spawn_minecraft_process(app_handle, launch_log_session, prepared_command)
}

#[cfg(test)]
#[path = "launch_preview_tests.rs"]
mod tests;
