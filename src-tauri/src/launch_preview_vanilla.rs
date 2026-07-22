use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::dependencies::DependencyResolution;
use crate::instance_mods::{prepare_instance_mods_directory, CachedModJar};
use crate::java_runtime::required_java_version_for_minecraft;
use crate::launch_command::{build_launch_command, JavaLaunchRequest, JavaLaunchSettings};
use crate::launcher_paths::LauncherPaths;
use crate::loader_metadata::LoaderMetadataClient;
use crate::minecraft_downloader::{ensure_minecraft_version, extract_natives};
use crate::mod_cache::ModAcquisitionPlan;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{ModLoader, ResolutionTarget};

use super::{
    build_instance_root, emit_log, emit_progress, filter_minecraft_launch_game_arguments,
    load_player_identity, select_or_download_java, spawn_minecraft_process,
    substitute_loader_placeholders, EffectiveLaunchSettings, LaunchLogSession, LaunchPlaceholders,
    SelectedMod, StartedLaunch,
};

pub(super) async fn run_vanilla_launch_pipeline(
    app_handle: tauri::AppHandle,
    launcher_paths: LauncherPaths,
    modlist_name: String,
    target: ResolutionTarget,
    launch_log_session: Arc<LaunchLogSession>,
    effective_settings: EffectiveLaunchSettings,
    http_client: reqwest::Client,
) -> Result<StartedLaunch> {
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        "[Launch] Vanilla loader selected; skipping mod resolution, mod downloads and content packs."
            .to_string(),
    )?;

    let selected_mods: Vec<SelectedMod> = Vec::new();
    let dependency_resolution = DependencyResolution {
        resolved_dependencies: Vec::new(),
        links: Vec::new(),
        excluded_parents: HashSet::new(),
    };
    let acquisition_plan = ModAcquisitionPlan {
        cached: Vec::new(),
        to_download: Vec::new(),
    };
    let cached_mod_jars = Vec::<CachedModJar>::new();

    launch_log_session.write_selected_mods(&selected_mods)?;
    launch_log_session.write_dependency_summary(&dependency_resolution)?;
    launch_log_session.write_resolved_versions(&[], &[])?;
    launch_log_session.write_cache_plan(&acquisition_plan)?;
    launch_log_session.write_final_mod_set(&cached_mod_jars)?;
    launch_log_session.append_summary_line("vanilla_direct_launch=true")?;
    launch_log_session.append_summary_line("excluded_top_level_mods=0")?;
    launch_log_session.append_summary_line(&format!(
        "cache_only_mode={}",
        effective_settings.cache_only_mode
    ))?;
    launch_log_session.append_summary_line("cache_only_ignored_for_vanilla=true")?;
    launch_log_session.append_summary_line("selected_mods=0")?;
    launch_log_session.append_summary_line("resolved_remote_versions=0")?;
    launch_log_session.append_summary_line("resolved_cached_records=0")?;

    let effective_required_java = required_java_version_for_minecraft(&target.minecraft_version)?;
    launch_log_session.append_summary_line(&format!(
        "required_java_before_loader={}",
        effective_required_java
    ))?;

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
        "Preparing a clean vanilla instance without mod or content-pack materialization.",
    )?;

    let instance_root = build_instance_root(&launcher_paths, &modlist_name, &target)?;
    let instance_mods_dir = instance_root.join("mods");
    let instance_natives_dir = instance_root.join("natives");
    let instance_library_dir = instance_root.join("libraries");

    std::fs::create_dir_all(&instance_natives_dir)
        .with_context(|| format!("failed to create {}", instance_natives_dir.display()))?;
    prepare_instance_mods_directory(
        launcher_paths.mods_cache_dir(),
        &instance_mods_dir,
        &cached_mod_jars,
    )?;
    extract_natives(&mc_data.native_paths, &instance_natives_dir)?;

    let mut loader_metadata = LoaderMetadataClient::new()
        .fetch_loader_metadata(&target.minecraft_version, ModLoader::Vanilla)
        .await?;
    loader_metadata.main_class = mc_data.main_class.clone();
    loader_metadata.jvm_arguments = mc_data.jvm_arguments.clone();
    loader_metadata.game_arguments =
        filter_minecraft_launch_game_arguments(&mc_data.game_arguments);

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

    let mut classpath_entries = mc_data.library_paths.clone();
    classpath_entries.push(mc_data.client_jar_path.clone());
    let prepared_command = build_launch_command(&JavaLaunchRequest {
        java_binary_path: java_binary_path.clone(),
        working_directory: instance_root,
        classpath_entries,
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
