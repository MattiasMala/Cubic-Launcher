use anyhow::{bail, Context, Result};
use tauri::State;

// Launch orchestration entry point.
//
// The sibling `launch_preview_*` files are focused slices of the launch
// pipeline: cache handling, loader metadata, logging,
// runtime setup, and verification. Keeping this file as the command-facing
// orchestrator makes launch behavior easier to audit without returning to one
// very large mixed-responsibility file.
use crate::app_shell::load_shell_snapshot_from_root;
use crate::instance_configs::{
    materialize_custom_configs, prepare_instance_config_directory, CachedConfigPlacement,
};
use crate::instance_mods::prepare_instance_mods_directory;
use crate::java_runtime::required_java_for_minecraft_version;
use crate::launch_command::{build_launch_command, JavaLaunchRequest};
use crate::launcher_paths::LauncherPaths;
use crate::minecraft_downloader::{
    ensure_minecraft_version, extract_natives, MinecraftCachePolicy,
};
use crate::mod_cache::cached_artifact_path_for_pending_download;
use crate::modrinth::ModrinthClient;
use crate::process_streaming::{
    stop_process, ProcessExitSignal, ProcessLogStream, StopOutcome, GRACEFUL_STOP_TIMEOUT,
};
use crate::resolver::{ModLoader, ResolutionTarget};
use crate::rules::ModSource;
use crate::worlds::quick_play_arguments;

use std::sync::Mutex;

#[path = "launch_preview_fabric.rs"]
pub(crate) mod fabric;
use fabric::*;

#[path = "launch_preview_fabric_versions.rs"]
mod fabric_versions;
use fabric_versions::*;

#[path = "launch_preview_artifacts.rs"]
mod artifacts;
use artifacts::*;

#[path = "launch_preview_cache.rs"]
mod cache;
use cache::*;

#[path = "launch_preview_precheck.rs"]
mod precheck;
use precheck::*;
pub use precheck::{DroppedModGroup, ModUpdateRow, UpdatePrecheckRequest, UpdatePrecheckResult};

#[path = "launch_preview_precheck_content.rs"]
mod precheck_content;
use precheck_content::*;
pub use precheck_content::{ContentEntryWithoutVersions, ContentLookupFailure, ContentUpdateRow};

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

/// The currently running Minecraft process, so that it can be stopped. The
/// monitor clears it when the process exits, and only then (windows-1): a STOP
/// that fails leaves it there, and the next STOP can try again.
static ACTIVE_MC: Mutex<Option<RunningMinecraft>> = Mutex::new(None);

pub(super) struct RunningMinecraft {
    pub(super) pid: u32,
    pub(super) exit_signal: ProcessExitSignal,
}

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

/// The pre-check the frontend calls before a launch: what would be updated,
/// and the versions the launch must then use. Writes nothing and launches
/// nothing.
#[tauri::command]
pub async fn update_precheck_command(
    app_handle: tauri::AppHandle,
    launcher_paths: State<'_, LauncherPaths>,
    request: UpdatePrecheckRequest,
) -> Result<UpdatePrecheckResult, String> {
    let launcher_paths = launcher_paths.inner().clone();

    run_update_precheck(&app_handle, &launcher_paths, request)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Stops the currently running Minecraft process, if any: first by asking it
/// to close, so that the world is saved, and only after
/// `GRACEFUL_STOP_TIMEOUT` by force (windows-1).
///
/// `async` because the wait can last seconds, and a synchronous command runs on
/// the main thread, which would freeze the window meanwhile.
#[tauri::command]
pub async fn stop_minecraft_command(app_handle: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || stop_active_minecraft(&app_handle))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))
}

fn stop_active_minecraft(app_handle: &tauri::AppHandle) -> Result<()> {
    let Some(pid) = current_minecraft_pid() else {
        bail!("No Minecraft process is running.");
    };
    let _ = emit_log(
        app_handle,
        ProcessLogStream::Stdout,
        format!("[Launch] STOP: asking Minecraft (PID {pid}) to close and save"),
    );
    let outcome = terminate_minecraft_pid(pid)?;
    let line = match outcome {
        StopOutcome::AlreadyExited | StopOutcome::Graceful => {
            "[Launch] Minecraft closed after STOP".to_string()
        }
        StopOutcome::Forced => format!(
            "[Launch] Minecraft did not close within {} s of STOP and was forced to stop; the last minutes of an open world may not be saved",
            GRACEFUL_STOP_TIMEOUT.as_secs()
        ),
    };
    let _ = emit_log(app_handle, ProcessLogStream::Stdout, line);
    Ok(())
}

/// Il file JFR dell'ultimo lancio, se esiste sul disco (D109). Il frontend lo chiede quando
/// arriva `minecraft-exit`: il valore è messo da parte prima dello spawn, quindi non dipende
/// dalla sessione attiva che l'uscita del gioco azzera.
#[tauri::command]
pub fn last_profile_recording_command() -> Option<String> {
    last_profile_recording_on_disk().map(|output_file| output_file.to_string_lossy().into_owned())
}

pub(super) fn current_minecraft_pid() -> Option<u32> {
    ACTIVE_MC
        .lock()
        .unwrap()
        .as_ref()
        .map(|running| running.pid)
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

/// Ferma `pid` se è ancora il Minecraft in corso: prima con garbo, poi con la
/// forza (windows-1). Un PID che non è più tracciato è già uscito.
pub(super) fn terminate_minecraft_pid(pid: u32) -> Result<StopOutcome> {
    let exit_signal = ACTIVE_MC
        .lock()
        .unwrap()
        .as_ref()
        .filter(|running| running.pid == pid)
        .map(|running| running.exit_signal.clone());
    let Some(exit_signal) = exit_signal else {
        return Ok(StopOutcome::AlreadyExited);
    };
    stop_process(pid, &exit_signal, GRACEFUL_STOP_TIMEOUT)
        .with_context(|| format!("failed to stop Minecraft PID {pid}"))
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

    let shell_snapshot = load_shell_snapshot_from_root(
        launcher_paths.root_dir(),
        Some(&modlist_name),
        crate::token_storage::KeyringSecretStore::new(),
    )?;
    let effective_settings = EffectiveLaunchSettings::from_shell_settings(
        &shell_snapshot.global_settings,
        &shell_snapshot.selected_modlist_overrides,
    );
    // shell-5: la RAM sbagliata si dice qui, prima di risolvere e scaricare
    // qualunque cosa, e non alla fine della preparazione.
    crate::app_shell::validate_effective_ram(
        shell_snapshot.global_settings.min_ram_mb,
        shell_snapshot.global_settings.max_ram_mb,
        shell_snapshot.selected_modlist_overrides.min_ram_mb,
        shell_snapshot.selected_modlist_overrides.max_ram_mb,
    )?;
    // D115: read once, before anything is chosen, so that the identity and
    // the resolution path of this launch come from the same reading of the
    // active account's row. The vanilla pipeline needs it for the identity.
    let plays_offline = active_account_plays_offline(&launcher_paths)?;
    if plays_offline {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            "[Launcher] The active account is set to play offline: no update check, no version lookups, offline identity".to_string(),
        )?;
    }

    remove_stale_partial_downloads(&app_handle, &launcher_paths);

    let http_client = crate::modrinth::build_download_client();
    if target.mod_loader == ModLoader::Vanilla {
        return run_vanilla_launch_pipeline(
            app_handle,
            launcher_paths,
            modlist_name,
            target,
            launch_log_session,
            effective_settings,
            http_client,
            request.quick_play_singleplayer.clone(),
            plays_offline,
        )
        .await;
    }

    let minecraft_progress = |label: &str, detail: &str| {
        let _ = emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!("[MC] {label}: {detail}"),
        );
    };
    // D142: con l'account offline Minecraft deve essere già tutto in cache, e
    // lo si guarda prima dei mod e di Java: se manca qualcosa il lancio si
    // ferma qui con un errore che lo dice, senza una richiesta (D132). Prima
    // partivano manifest, client, librerie e migliaia di asset.
    let cached_minecraft = if plays_offline {
        Some(
            ensure_minecraft_version(
                &http_client,
                &target.minecraft_version,
                &launcher_paths,
                MinecraftCachePolicy::CacheOnly,
                &minecraft_progress,
            )
            .await?,
        )
    } else {
        None
    };

    // lancio-11: i metadati del loader prima di risolvere e scaricare
    // qualunque cosa. Una coppia versione/loader che non esiste (NeoForge
    // 1.19.2) si ferma qui, invece che dopo centinaia di MB di mod e di
    // Minecraft.
    let mut loader_metadata = fetch_launch_loader_metadata(&target).await?;

    let modlist = load_modlist(&launcher_paths, &modlist_name)?;
    let modrinth_client = ModrinthClient::new();

    // Verify Modrinth availability for resolved mods. If a resolved mod has no
    // compatible version on Modrinth, temporarily disable it and re-resolve so
    // alternatives get a chance. On the preresolved path a valid cached
    // artifact also counts as available.
    //
    // Both branches live in `launch_preview_artifacts.rs` because the update
    // pre-check runs the online one too: it has to name exactly the mods this
    // launch will install.
    let resolution_path =
        LaunchResolutionPath::for_active_account(plays_offline, request.resolved_versions.as_ref());
    // `None` on the `Resolve` path, always — the path is chosen by the map's
    // presence. The online branch below keeps reading it so that "no map" is
    // literally today's path, unchanged: whether that branch should still
    // carry its map handling is part of the merge-the-two-branches decision
    // this task does not take.
    let preresolved_versions = resolution_path.preresolved();
    let selection = match resolution_path {
        LaunchResolutionPath::Preresolved(preresolved) => {
            resolve_offline_selection(&launcher_paths, &modlist, &target, Some(preresolved))?
        }
        // No map, and none wanted: the cache alone answers (D115).
        LaunchResolutionPath::Offline => {
            resolve_offline_selection(&launcher_paths, &modlist, &target, None)?
        }
        LaunchResolutionPath::Resolve => {
            resolve_online_selection(
                &app_handle,
                &launcher_paths,
                &http_client,
                &modlist,
                &modrinth_client,
                &target,
                // No map on this path, by construction.
                None,
            )
            .await?
        }
    };
    log_resolution(&app_handle, &selection.resolution)?;
    // The availability pass runs before the artifact stage, so a mod it
    // disabled is gone from `selected_mods` and `report_unusable_probe` can no
    // longer see it: this is the only place that names it (A5). The pre-check
    // calls the same pass and does not report — it decides what the popup
    // shows, and a notice there would say it twice for one launch.
    report_dropped_mods(&app_handle, &selection.dropped, &target);

    let selected_mods = selection.selected_mods;
    launch_log_session.write_selected_mods(&selected_mods)?;
    if selected_mods.len() > 300 {
        let detail = match resolution_path {
            LaunchResolutionPath::Preresolved(_) => {
                "You have more than 300 mods. The versions were already chosen before this launch, so only the jars you do not have yet are downloaded."
            }
            LaunchResolutionPath::Resolve => {
                "You have more than 300 mods and this launch has no pre-checked versions to start from, so every version is resolved against Modrinth: it may take 1-2 minutes due to API rate limits."
            }
            LaunchResolutionPath::Offline => {
                "You have more than 300 mods. The active account plays offline, so every mod comes from the jars you already have."
            }
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

    let (all_remote_versions, cached_remote_records, missing_jar_records, project_aliases) = if matches!(
        resolution_path,
        LaunchResolutionPath::Preresolved(_) | LaunchResolutionPath::Offline
    ) {
        let cache_message = match resolution_path.preresolved() {
            Some(preresolved) => format!(
                "[Cache] Launching from the {} versions chosen before launch; the cache answers first and only what is missing is fetched.",
                preresolved.len()
            ),
            // With no map nothing is covered and no version is fetched: every
            // mod takes the cached artifact of its project.
            None => "[Cache] Launching from the cached versions; no version is looked up on Modrinth.".to_string(),
        };
        emit_log(&app_handle, ProcessLogStream::Stdout, cache_message)?;

        let parent_artifacts = resolve_selected_remote_artifacts(
            &app_handle,
            &launcher_paths,
            &selected_mods,
            &modrinth_client,
            &target,
            resolution_path.preresolved(),
        )
        .await?;
        // The rows for the versions fetched here are written at the end of the
        // launch under their canonical project id; without the alias a rule
        // that names a slug would stop finding them on the next launch. Same
        // list the online branch builds, for the same reason.
        let project_aliases = parent_artifacts
            .iter()
            .filter_map(|(mod_id, artifact)| match artifact {
                RemoteArtifact::Live(version) => Some((mod_id.clone(), version.project_id.clone())),
                RemoteArtifact::Cached(_) | RemoteArtifact::MissingJar(_) => None,
            })
            .collect::<Vec<_>>();
        let parent_artifact_values = parent_artifacts.into_values().collect::<Vec<_>>();
        let split = split_remote_artifacts(&parent_artifact_values);
        let parent_versions = split.live_versions;

        // DESIGN: the launcher does not manage dependencies. It downloads ONLY
        // what the user selected; a dependency the user did not add is the
        // user's call, and the Minecraft client says so at startup if it is
        // really needed.

        (
            parent_versions,
            split.cached_records,
            split.missing_jar_records,
            project_aliases,
        )
    } else {
        // DESIGN: deliberately a second pass, not a reuse of the one inside
        // `resolve_online_selection`. That one ran on the mods the first
        // resolution selected; this one runs on `selected_mods`, which exists
        // only after the re-resolution and can contain alternatives the first
        // pass never saw. Merging the two means handling that delta, and
        // getting it wrong means a mod the re-resolution enabled that nobody
        // looks up. Two passes still cost ~10 requests against the 52 of one
        // request per mod per pass.
        //
        // With a version map from the pre-check this pass stops choosing and
        // starts looking up: the launch installs exactly what the popup showed
        // (D16).
        let compatible_versions = match preresolved_versions {
            Some(preresolved) => {
                resolve_preresolved_versions(
                    &app_handle,
                    &launcher_paths,
                    &http_client,
                    &selected_mods,
                    &modrinth_client,
                    &target,
                    preresolved,
                )
                .await?
            }
            None => {
                resolve_compatible_versions_hybrid(
                    &app_handle,
                    &launcher_paths,
                    &http_client,
                    &selected_mods,
                    &modrinth_client,
                    &target,
                )
                .await?
            }
        };
        // The mods no version came back for keep the jar the cache already
        // holds (D29): the availability pass kept them, so this is where they
        // are actually acquired.
        let fallback = resolve_cache_fallback_artifacts(
            &app_handle,
            &launcher_paths,
            &selected_mods,
            &compatible_versions,
            &target,
        )?;
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
        // version, independent of what the others declare. What a mod declares
        // it needs is not inspected: dependencies are the user's to manage.

        (
            parent_versions,
            fallback.cached_records,
            fallback.missing_jar_records,
            project_aliases,
        )
    };
    launch_log_session.write_resolved_versions(
        &all_remote_versions,
        &cached_remote_records,
        &missing_jar_records,
    )?;
    launch_log_session.append_summary_line(&format!(
        "launch_branch={}",
        resolution_path.summary_label()
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
    emit_progress(
        &app_handle,
        "resolving",
        42,
        "Check Cache",
        "Inspecting cached mods and downloading missing dependencies.",
    )?;

    // One builder for both paths: the three lists say what each launch found,
    // and on the resolving path the two record lists are empty unless the
    // cache had to answer for a mod Modrinth did not.
    let acquisition_plan = build_remote_acquisition_plan_from_artifacts(
        &launcher_paths,
        &all_remote_versions,
        &cached_remote_records,
        &missing_jar_records,
        &target,
    )?;
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
    persist_remote_versions_and_aliases(
        &launcher_paths,
        &all_remote_versions,
        &target,
        &project_aliases,
    )?;

    let mc_data = match cached_minecraft {
        Some(mc_data) => mc_data,
        None => {
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
            ensure_minecraft_version(
                &http_client,
                &target.minecraft_version,
                &launcher_paths,
                MinecraftCachePolicy::Download,
                &minecraft_progress,
            )
            .await?
        }
    };
    // download-3: la major di Java la dice il version.json, che conosce anche
    // snapshot, pre-release e release candidate.
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
        "Refreshing mods, loader libraries, configs and launch metadata.",
    )?;

    let instance_root = build_instance_root(&launcher_paths, &modlist_name, &target)?;
    let instance_mods_dir = instance_root.join("mods");
    let instance_config_dir = instance_root.join("config");
    let instance_natives_dir = instance_root.join("natives");
    let instance_library_dir = instance_root.join("libraries");

    std::fs::create_dir_all(&instance_natives_dir)
        .with_context(|| format!("failed to create {}", instance_natives_dir.display()))?;

    // Primo avvio di questa istanza: si semina da `options.txt` della modlist,
    // e mai più (D66). Il "mai più" non ha bisogno di uno stato nostro — dal
    // secondo avvio il file c'è, perché l'abbiamo scritto noi o perché il
    // gioco l'ha riscritto — e la semina lascia comunque una riga nel log,
    // anche quando non fa niente.
    let options_seed = crate::options_share::seed_instance_from_modlist(
        &launcher_paths,
        &modlist_name,
        &instance_root,
    );
    let _ = emit_log(
        &app_handle,
        ProcessLogStream::Stdout,
        format!("[options] {}", options_seed.describe()),
    );

    let cached_mod_jars = build_cached_mod_jars(
        &app_handle,
        &selected_mods,
        &all_remote_versions,
        &cached_remote_records,
        &missing_jar_records,
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
    // D105: the custom configs of the mod list that apply to this target,
    // copied into the instance at every launch (D118). A file that does not
    // resolve does not stop the launch, but it is never silent.
    let custom_configs = materialize_custom_configs(
        &launcher_paths.modlists_dir().join(&modlist_name),
        &instance_root,
        &modlist.rules,
        &target.minecraft_version,
        target.mod_loader.as_modrinth_loader(),
    )?;
    for missing in &custom_configs.missing_files {
        emit_log(
            &app_handle,
            ProcessLogStream::Stderr,
            format!(
                "[Config] Custom config '{}' of '{}' lists '{}', which is not in the mod list: not placed",
                missing.config_id, missing.mod_id, missing.file_name
            ),
        )?;
    }
    if !custom_configs.materialized_files.is_empty() {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[Config] Placed {} custom config file(s)",
                custom_configs.materialized_files.len()
            ),
        )?;
    }

    // Download and install content packs (resource packs, data packs, shaders),
    // at the versions the pre-check named when it named any (D58).
    //
    // D-c (onda 8a): con l'account offline il passo non gira. Nessun lookup su
    // Modrinth, nessuna installazione, nessuno svuotamento: restano nell'istanza
    // i file del lancio precedente. Il lookup fallito lasciava già lo stesso
    // risultato, ma dopo ~70 s di tentativi (report 098, S3), e con la rete
    // funzionante un account offline si vedeva aggiornare i pack.
    if plays_offline {
        emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            "[Content] The active account plays offline: content packs are not looked up or installed; the instance keeps the ones from the previous launch".to_string(),
        )?;
    } else {
        resolve_and_install_content_packs(
            &app_handle,
            &launcher_paths,
            &http_client,
            &modrinth_client,
            &modlist_name,
            &target,
            &instance_root,
            request.resolved_content.as_ref(),
        )
        .await?;
    }

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
        loader_metadata.jvm_arguments =
            merge_minecraft_and_loader_jvm_arguments(&mc_data.jvm_arguments, Vec::new());
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
    let player_identity = load_player_identity(&app_handle, &launcher_paths, plays_offline).await?;
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
        mc_data.is_virtual_assets,
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
        classpath_entries: build_modded_classpath_entries(
            &mc_data.library_paths,
            loader_library_paths,
            mc_data.client_jar_path.clone(),
        ),
        loader_metadata,
        launch_settings: effective_settings.java_launch_settings(profiler),
        additional_game_arguments: quick_play_arguments(
            &instance_root,
            request.quick_play_singleplayer.as_deref(),
            mc_data.supports_quick_play_singleplayer,
        ),
        config_attribution: None,
    })?;

    // I tre file condivisi entrano **prima dello spawn**: per `servers.dat` il
    // margine è stretto, perché basta che il gioco apra la schermata
    // Multigiocatore e risponda a un ping per riscriverlo (D76).
    let shared_files = runtime::SharedFilesContext {
        launcher_paths: launcher_paths.clone(),
        modlist_name: modlist_name.clone(),
        instance_root: instance_root.clone(),
    };
    {
        let connection = rusqlite::Connection::open(launcher_paths.database_path())?;
        // La `DataVersion` della versione che sta per partire serve al
        // guardiano delle hotbar (D79) per l'istanza che un `hotbar.nbt` non
        // ce l'ha ancora. Se non si riesce a ricavarla il guardiano non ha un
        // termine di confronto e la copia delle hotbar si ferma lì: è la
        // direzione sicura.
        let instance_data_version =
            crate::options_keys::load_or_derive(&launcher_paths, &target.minecraft_version)
                .ok()
                .map(|keys| keys.data_version);
        let statuses = crate::shared_files::copy_into_instance(
            &shared_files.launcher_paths,
            &connection,
            &shared_files.modlist_name,
            &shared_files.instance_root,
            instance_data_version,
        );
        let _ = emit_log(
            &app_handle,
            ProcessLogStream::Stdout,
            format!(
                "[shared] {}",
                crate::shared_files::describe(&statuses, "copied into the instance")
            ),
        );
    }

    spawn_minecraft_process(
        app_handle,
        launch_log_session,
        prepared_command,
        shared_files,
    )
}

#[cfg(test)]
#[path = "launch_preview_tests.rs"]
mod tests;
