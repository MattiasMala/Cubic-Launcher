use std::sync::Arc;

use anyhow::{Context, Result};

use crate::instance_mods::{prepare_instance_mods_directory, CachedModJar};
use crate::java_runtime::required_java_for_minecraft_version;
use crate::launch_command::{build_launch_command, JavaLaunchRequest};
use crate::launcher_paths::LauncherPaths;
use crate::loader_metadata::LoaderMetadataClient;
use crate::minecraft_downloader::{ensure_minecraft_version, extract_natives};
use crate::mod_cache::ModAcquisitionPlan;
use crate::modrinth::ModrinthClient;
use crate::process_streaming::ProcessLogStream;
use crate::resolver::{ModLoader, ResolutionTarget};
use crate::worlds::quick_play_arguments;

use super::{
    build_instance_root, emit_log, emit_progress, filter_minecraft_launch_game_arguments,
    install_launch_content_packs, launch_profiler, load_player_identity,
    merge_minecraft_and_loader_jvm_arguments, minecraft_cache_policy, seed_instance_options,
    select_or_download_java, spawn_minecraft_process, substitute_loader_placeholders,
    EffectiveLaunchSettings, LaunchDirectories, LaunchLogSession, LaunchPlaceholders,
    ResolvedContentVersions, SelectedMod, StartedLaunch,
};

// Dieci input distinti di un passo del lancio. Una struct di parametri
// cambierebbe i chiamanti del percorso di lancio, fuori da una modifica di
// solo stile (gate fmt/clippy, D120): il lint è sospeso solo qui.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_vanilla_launch_pipeline(
    app_handle: tauri::AppHandle,
    launcher_paths: LauncherPaths,
    modlist_name: String,
    target: ResolutionTarget,
    launch_log_session: Arc<LaunchLogSession>,
    effective_settings: EffectiveLaunchSettings,
    http_client: reqwest::Client,
    quick_play_singleplayer: Option<String>,
    // The pre-check's content-pack versions (D58), when it named any.
    resolved_content: Option<ResolvedContentVersions>,
    // The active account's row, read once by the caller (D115, concorrenza-8):
    // its play-offline choice and its identity come from this one reading.
    launch_account: Option<crate::microsoft_auth::AccountRecord>,
) -> Result<StartedLaunch> {
    let plays_offline = launch_account
        .as_ref()
        .is_some_and(|account| account.plays_offline);
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        "[Launch] Vanilla loader selected; skipping mod resolution and mod downloads.".to_string(),
    )?;

    let selected_mods: Vec<SelectedMod> = Vec::new();
    let acquisition_plan = ModAcquisitionPlan {
        cached: Vec::new(),
        to_download: Vec::new(),
    };
    let cached_mod_jars = Vec::<CachedModJar>::new();

    launch_log_session.write_selected_mods(&selected_mods)?;
    launch_log_session.write_resolved_versions(&[], &[], &[])?;
    launch_log_session.write_cache_plan(&acquisition_plan)?;
    launch_log_session.write_final_mod_set(&cached_mod_jars)?;
    launch_log_session.append_summary_line("vanilla_direct_launch=true")?;
    launch_log_session.append_summary_line("excluded_top_level_mods=0")?;
    // No mods means no version resolution and no branch to pick: `summary.log`
    // says so instead of reporting a setting that no longer decides anything.
    launch_log_session.append_summary_line("launch_branch=vanilla_no_mods")?;
    launch_log_session.append_summary_line("selected_mods=0")?;
    launch_log_session.append_summary_line("resolved_remote_versions=0")?;
    launch_log_session.append_summary_line("resolved_cached_records=0")?;

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

    // D142: offline la cache deve avere tutto, e nessuna richiesta parte. Qui
    // non ci sono mod, e Java viene dopo: il controllo è già il primo passo.
    let mc_data = ensure_minecraft_version(
        &http_client,
        &target.minecraft_version,
        &launcher_paths,
        minecraft_cache_policy(plays_offline),
        |label, detail| {
            let _ = emit_log(
                &app_handle,
                ProcessLogStream::Stdout,
                format!("[MC] {label}: {detail}"),
            );
        },
    )
    .await?;

    // download-3: la major di Java la dice il version.json appena letto.
    let effective_required_java =
        required_java_for_minecraft_version(mc_data.java_major_version, &target.minecraft_version);
    launch_log_session.append_summary_line(&format!(
        "required_java_before_loader={}",
        effective_required_java
    ))?;

    emit_progress(
        &app_handle,
        "resolving",
        74,
        "Prepare Instance",
        "Preparing the vanilla instance: game settings and content packs, no mods.",
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

    // condivisi-5: le impostazioni di gioco della mod list valgono anche per
    // un'istanza Vanilla, con la stessa riga `[options]` del percorso moddato.
    seed_instance_options(&app_handle, &launcher_paths, &modlist_name, &instance_root);
    // contentpack-3: resource pack e data pack anche con Vanilla, con lo stesso
    // cancello dell'account offline del percorso moddato; gli shader no.
    install_launch_content_packs(
        &app_handle,
        &launcher_paths,
        &http_client,
        &ModrinthClient::new(),
        &modlist_name,
        &target,
        &instance_root,
        resolved_content.as_ref(),
        plays_offline,
    )
    .await?;

    let mut loader_metadata = LoaderMetadataClient::new()
        .fetch_loader_metadata(&target.minecraft_version, ModLoader::Vanilla)
        .await?;
    loader_metadata.main_class = mc_data.main_class.clone();
    loader_metadata.jvm_arguments =
        merge_minecraft_and_loader_jvm_arguments(&mc_data.jvm_arguments, Vec::new());
    loader_metadata.game_arguments =
        filter_minecraft_launch_game_arguments(&mc_data.game_arguments);

    emit_progress(
        &app_handle,
        "resolving",
        90,
        "Authenticating",
        "Refreshing Minecraft session...",
    )?;
    let player_identity =
        load_player_identity(&app_handle, &launcher_paths, launch_account.as_ref()).await?;
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
        &LaunchDirectories {
            instance_root: &instance_root,
            assets_root: &mc_data.assets_dir,
            asset_index_id: &mc_data.asset_index_id,
            library_directory: &instance_library_dir,
            natives_directory: &instance_natives_dir,
            is_virtual_assets: mc_data.is_virtual_assets,
        },
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

    let selected_java = select_or_download_java(
        &app_handle,
        &launcher_paths,
        &effective_settings,
        &target,
        effective_required_java,
        plays_offline,
    )
    .await?;
    emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!(
            "[Java] Using {} (Java {})",
            selected_java.path.display(),
            selected_java.major
        ),
    )?;

    let mut classpath_entries = mc_data.library_paths.clone();
    classpath_entries.push(mc_data.client_jar_path.clone());
    let profiler = launch_profiler(
        &app_handle,
        &effective_settings,
        // todo.md:1800: la Java che parte davvero, non il requisito.
        selected_java.major,
        launch_log_session.dir(),
    )?;
    let prepared_command = build_launch_command(&JavaLaunchRequest {
        java_binary_path: selected_java.path.clone(),
        working_directory: instance_root.clone(),
        classpath_entries,
        loader_metadata,
        launch_settings: effective_settings.java_launch_settings(profiler),
        additional_game_arguments: quick_play_arguments(
            &instance_root,
            quick_play_singleplayer.as_deref(),
            mc_data.supports_quick_play_singleplayer,
        ),
    })?;

    // Stessa regola del percorso moddato: la copia entra prima dello spawn.
    let shared_files = super::runtime::SharedFilesContext {
        launcher_paths: launcher_paths.clone(),
        modlist_name: modlist_name.clone(),
        instance_root: instance_root.clone(),
    };
    {
        let connection = rusqlite::Connection::open(launcher_paths.database_path())?;
        let instance_data_version =
            crate::options_keys::load_or_derive(&launcher_paths, &target.minecraft_version)
                .ok()
                .map(|keys| keys.data_version);
        let statuses = crate::shared_files::copy_into_instance(
            &launcher_paths,
            &connection,
            &modlist_name,
            &instance_root,
            instance_data_version,
        );
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[shared] {}",
                crate::shared_files::describe(&statuses, "copied into the instance")
            ),
        )?;
    }

    spawn_minecraft_process(
        app_handle,
        launch_log_session,
        prepared_command,
        shared_files,
    )
}
