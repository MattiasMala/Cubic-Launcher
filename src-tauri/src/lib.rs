use tauri::Manager;

pub mod account_manager;
pub mod adoptium;
pub mod app_shell;
pub mod config_attribution;
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
pub mod mod_cache;
pub mod mod_icons;
pub mod mod_version_pin;
pub mod modlist_assets;
pub mod modlist_manager;
pub mod modrinth;
pub mod path_safety;
pub mod offline_account;
pub mod options_file;
pub mod options_keys;
pub mod options_share;
pub mod process_streaming;
pub mod resolver;
pub mod rules;
pub mod screenshots;
pub mod shared_files;
pub mod shared_worlds;
pub mod skins;
pub mod token_storage;
mod updater;
pub mod worlds;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let launcher_root = app
                .path()
                .app_local_data_dir()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let launcher_paths = launcher_paths::LauncherPaths::new(launcher_root);

            launcher_paths
                .create_required_directories()
                .map_err(|error| std::io::Error::other(error.to_string()))?;

            database::initialize_database(launcher_paths.database_path())
                .map_err(|error| std::io::Error::other(error.to_string()))?;

            {
                let connection =
                    rusqlite::Connection::open(launcher_paths.database_path())
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                database::migrate_account_tokens_from_profile_data(
                    &connection,
                    token_storage::KeyringSecretStore::new(),
                )
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            }

            app.manage(launcher_paths.clone());
            app.manage(skins::SkinsState::default());

            if launch_preview::automation_mode_enabled() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            launch_preview::maybe_start_automation_verifier(app.handle().clone(), launcher_paths)
                .map_err(|error| std::io::Error::other(error.to_string()))?;

            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            app_shell::load_shell_snapshot_command,
            app_shell::switch_active_account_command,
            app_shell::microsoft_login_command,
            app_shell::delete_account_command,
            app_shell::save_global_settings_command,
            app_shell::save_modlist_overrides_command,
            debug_trace::append_debug_trace_command,
            debug_trace::clear_debug_trace_command,
            editor_data::load_modlist_editor_command,
            editor_data::add_mod_rule_command,
            editor_data::delete_rules_command,
            editor_data::rename_rule_command,
            editor_data::reorder_rules_command,
            editor_data::save_alternative_order_command,
            editor_data::add_alternative_command,
            editor_data::add_nested_alternative_command,
            editor_data::remove_alternative_command,
            editor_data::save_incompatibilities_command,
            editor_data::toggle_rule_enabled_command,
            editor_data::save_rule_advanced_command,
            editor_data::save_advanced_batch_command,
            modlist_manager::create_modlist_command,
            modlist_manager::delete_modlist_command,
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
            options_share::derive_option_keys_command,
            resolver::resolve_modlist_command,
            resolver::backfill_availability_command,
            minecraft_downloader::fetch_minecraft_versions_command,
            minecraft_downloader::start_minecraft_predownload_command,
            content_packs::load_content_list_command,
            content_packs::add_content_command,
            content_packs::remove_content_command,
            content_packs::reorder_content_command,
            content_packs::save_content_groups_command,
            content_packs::save_content_version_rules_command,
            local_content_packs::import_local_content_pack_command,
            launch_preview::start_launch_command,
            launch_preview::update_precheck_command,
            launch_preview::verify_launch_command,
            launch_preview::stop_minecraft_command,
            screenshots::list_screenshots_command,
            screenshots::delete_screenshot_command,
            screenshots::open_screenshot_folder_command,
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
            updater::install_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
