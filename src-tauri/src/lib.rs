use tauri::Manager;

pub mod account_manager;
pub mod adoptium;
pub mod app_shell;
mod atomic_write;
pub mod content_packs;
mod database;
pub mod debug_trace;
pub mod editor_data;
pub mod instance_configs;
pub mod instance_content;
pub mod instance_mods;
pub mod java_runtime;
pub mod launch_command;
pub mod launch_preview;
mod launcher_paths;
pub mod loader_metadata;
pub mod local_content_packs;
pub mod microsoft_auth;
pub mod minecraft_downloader;
pub mod minecraft_versions;
pub mod mod_cache;
pub mod mod_icons;
pub mod mod_version_pin;
pub mod modlist_assets;
pub mod modlist_manager;
pub mod modrinth;
pub mod offline_account;
pub mod options_file;
pub mod options_keys;
pub mod options_share;
pub mod path_safety;
pub mod process_streaming;
pub mod resolver;
pub mod rules;
pub mod screenshots;
pub mod shared_files;
pub mod shared_worlds;
pub mod skins;
mod startup;
#[cfg(test)]
mod test_client_jars;
pub mod token_storage;
mod updater;
pub mod worlds;

/// L'evento con cui il backend chiede al frontend di confermare la chiusura
/// col gioco aperto (D152).
const CLOSE_WHILE_RUNNING_EVENT: &str = "close-requested-while-running";

/// Chiudere il launcher col gioco aperto salta la copia indietro dei file
/// condivisi, che vive dopo `process.wait()` (`concorrenza-1`). La copia dentro
/// del lancio dopo li recupera (D152), ma la chiusura chiede comunque conferma:
/// qui si ferma, e il frontend decide se richiamare
/// [`close_launcher_anyway_command`]. Vale per ogni strada — il bottone della
/// testata, Alt+F4, il menu della barra delle applicazioni — perché passano
/// tutte da `CloseRequested`.
///
/// Se il frontend non risponde — JavaScript bloccato, la testata mai montata —
/// la finestra non si chiuderebbe più con nessuna strada (review-frontend-shell-30).
/// Per questo il frontend conferma subito di aver ricevuto l'evento
/// ([`close_request_received_command`]): una chiusura che arriva quando la
/// precedente è senza risposta da [`CLOSE_ANSWER_WAIT`] passa. Prima di quel
/// tempo (un doppio clic) si ferma e basta. Un ricaricamento della finestra
/// azzera il segno quando la testata si rimonta, così il suo clic perso non
/// conta.
fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    use tauri::Emitter;

    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        if window.label() == "main" && launch_preview::current_minecraft_pid().is_some() {
            let mut unanswered = unanswered_close_request();
            match *unanswered {
                Some(since) if since.elapsed() >= CLOSE_ANSWER_WAIT => {
                    eprintln!(
                        "[close] the window did not answer the last close request while Minecraft runs: closing"
                    );
                    // Not prevented: the window closes.
                    *unanswered = None;
                }
                Some(_) => api.prevent_close(),
                None => {
                    *unanswered = Some(std::time::Instant::now());
                    api.prevent_close();
                    let _ = window.emit(CLOSE_WHILE_RUNNING_EVENT, ());
                }
            }
        }
    }
}

/// Quanto la finestra ha per dire di aver sentito una richiesta di chiusura.
const CLOSE_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Da quando una richiesta di chiusura col gioco aperto aspetta che il
/// frontend dica di averla ricevuta.
fn unanswered_close_request() -> std::sync::MutexGuard<'static, Option<std::time::Instant>> {
    static UNANSWERED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    UNANSWERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Il frontend ha ricevuto la richiesta di chiusura (o si è appena montato, e
/// quindi la ascolta): la prossima chiusura chiede di nuovo conferma.
#[tauri::command]
fn close_request_received_command() {
    *unanswered_close_request() = None;
}

/// La chiusura confermata: `destroy` non ripassa da `CloseRequested`.
#[tauri::command]
fn close_launcher_anyway_command(window: tauri::Window) -> Result<(), String> {
    window.destroy().map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();

    // The data folder, its lock and the database are checked before Tauri
    // starts, so that a failure is a dialog that says where and why instead
    // of a panic with no window (r2-primo-avvio-ambiente-degradato-4), and a
    // second launcher on the same folder stops here (concorrenza-5).
    let launcher_data = startup::launcher_root(&context.config().identifier)
        .and_then(startup::prepare_launcher_data)
        .unwrap_or_else(|failure| startup::report_and_exit(&failure));
    let startup::LauncherData {
        paths: launcher_paths,
        instance_lock,
    } = launcher_data;

    tauri::Builder::default()
        .setup(move |app| {
            app.manage(instance_lock);
            app.manage(launcher_paths.clone());
            app.manage(skins::SkinsState::default());
            app.manage(app_shell::MicrosoftLoginState::default());
            // review-contenuti-14: a deletion or an import a previous run did
            // not finish, removed in the background.
            modlist_manager::sweep_leftover_work_dirs(&launcher_paths);

            if launch_preview::automation_mode_enabled() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            launch_preview::maybe_start_automation_verifier(app.handle().clone(), launcher_paths)
                .map_err(|error| std::io::Error::other(error.to_string()))?;

            Ok(())
        })
        .on_window_event(on_window_event)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            app_shell::load_shell_snapshot_command,
            app_shell::switch_active_account_command,
            app_shell::microsoft_login_command,
            app_shell::cancel_microsoft_login_command,
            app_shell::delete_account_command,
            app_shell::list_accounts_command,
            app_shell::set_account_plays_offline_command,
            app_shell::save_global_settings_command,
            app_shell::save_modlist_overrides_command,
            app_shell::save_modlist_target_command,
            debug_trace::append_debug_trace_command,
            editor_data::load_modlist_editor_command,
            editor_data::add_mod_rule_command,
            editor_data::delete_rules_command,
            editor_data::reorder_rules_command,
            editor_data::save_alternative_order_command,
            editor_data::add_alternative_command,
            editor_data::add_nested_alternative_command,
            editor_data::remove_alternative_command,
            editor_data::save_incompatibilities_command,
            editor_data::toggle_rule_enabled_command,
            editor_data::toggle_rules_enabled_command,
            editor_data::save_advanced_batch_command,
            editor_data::import_custom_config_files_command,
            modlist_manager::create_modlist_command,
            modlist_manager::delete_modlist_command,
            modlist_manager::modlist_delete_preview_command,
            modlist_manager::copy_local_jar_command,
            modlist_manager::import_modlist_command,
            mod_version_pin::pin_mod_version_command,
            mod_version_pin::remove_pin_command,
            modlist_assets::load_modlist_presentation_command,
            modlist_assets::save_modlist_presentation_command,
            modlist_assets::load_modlist_groups_command,
            modlist_assets::save_modlist_groups_command,
            modlist_assets::export_modlist_command,
            modlist_assets::list_instance_files_command,
            modlist_assets::read_image_as_data_url_command,
            options_share::load_shared_options_command,
            options_share::save_shared_options_command,
            options_share::apply_options_command,
            resolver::resolve_modlist_command,
            resolver::backfill_availability_command,
            minecraft_versions::fetch_minecraft_versions_command,
            content_packs::load_content_list_command,
            content_packs::add_content_command,
            content_packs::remove_content_command,
            content_packs::reorder_content_command,
            content_packs::save_content_groups_command,
            content_packs::save_content_version_rules_command,
            local_content_packs::import_local_content_pack_command,
            launch_preview::start_launch_command,
            launch_preview::update_precheck_command,
            launch_preview::stop_minecraft_command,
            launch_preview::launch_status_command,
            launch_preview::last_profile_recording_command,
            screenshots::list_screenshots_command,
            screenshots::delete_screenshot_command,
            screenshots::open_screenshot_folder_command,
            screenshots::screenshot_thumbnail_command,
            shared_files::shared_files_view_command,
            shared_files::set_shared_file_enabled_command,
            shared_files::link_shared_files_command,
            shared_files::unlink_shared_files_command,
            skins::load_skins_command,
            skins::add_skin_command,
            skins::set_saved_skin_variant_command,
            skins::rename_saved_skin_command,
            skins::remove_saved_skin_command,
            skins::equip_skin_command,
            skins::reset_skin_command,
            skins::set_cape_command,
            skins::save_worn_skin_command,
            worlds::list_worlds_command,
            worlds::set_world_hidden_command,
            worlds::open_world_folder_command,
            shared_worlds::share_world_with_instance_command,
            shared_worlds::unshare_world_from_instance_command,
            updater::check_for_updates,
            updater::install_update,
            close_launcher_anyway_command,
            close_request_received_command
        ])
        .run(context)
        .expect("error while running tauri application");
}
