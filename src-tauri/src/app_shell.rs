use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::account_manager::{AccountManager, ManagedAccountProfile, ManagedAccountTokens};
use crate::launcher_paths::LauncherPaths;
use crate::microsoft_auth::{
    configured_microsoft_client_id, run_microsoft_login, AccountsRepository,
};
use crate::rules::{ModList, RULES_FILENAME};
use crate::token_storage::KeyringSecretStore;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellSnapshot {
    pub modlists: Vec<ShellModListSummary>,
    pub active_account: Option<ShellActiveAccount>,
    pub global_settings: ShellGlobalSettings,
    pub selected_modlist_overrides: ShellModListOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellModListSummary {
    pub name: String,
    pub description: String,
    pub author: Option<String>,
    pub rule_count: usize,
    pub minecraft_version: Option<String>,
    pub mod_loader: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellActiveAccount {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub avatar_url: Option<String>,
    pub status: String,
    pub last_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellGlobalSettings {
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub custom_jvm_args: String,
    pub profiler_enabled: bool,
    pub cache_only_mode: bool,
    pub wrapper_command: String,
    pub java_path_override: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellModListOverrides {
    pub modlist_name: Option<String>,
    pub min_ram_mb: Option<u32>,
    pub max_ram_mb: Option<u32>,
    pub custom_jvm_args: Option<String>,
    pub profiler_enabled: Option<bool>,
    pub wrapper_command: Option<String>,
    pub minecraft_version: Option<String>,
    pub mod_loader: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellGlobalSettingsInput {
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub custom_jvm_args: String,
    pub profiler_enabled: bool,
    pub cache_only_mode: bool,
    pub wrapper_command: String,
    pub java_path_override: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellModListOverridesInput {
    pub modlist_name: String,
    pub min_ram_mb: Option<u32>,
    pub max_ram_mb: Option<u32>,
    pub custom_jvm_args: Option<String>,
    pub profiler_enabled: Option<bool>,
    pub wrapper_command: Option<String>,
    #[serde(default)]
    pub minecraft_version: Option<String>,
    #[serde(default)]
    pub mod_loader: Option<String>,
}

#[tauri::command]
pub fn load_shell_snapshot_command(
    launcher_paths: State<'_, LauncherPaths>,
    selected_modlist_name: Option<String>,
) -> Result<ShellSnapshot, String> {
    load_shell_snapshot_from_root(launcher_paths.root_dir(), selected_modlist_name.as_deref())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn switch_active_account_command(
    launcher_paths: State<'_, LauncherPaths>,
    microsoft_id: String,
) -> Result<(), String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;

    AccountsRepository::new(&connection)
        .set_active_account(&microsoft_id)
        .map_err(|error| error.to_string())
}

/// Starts the Microsoft OAuth login flow, opens the browser, waits for
/// callback, exchanges tokens through Xbox Live → XSTS → Minecraft, and
/// saves the account to the database.
#[tauri::command]
pub async fn microsoft_login_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<String, String> {
    let env_path = launcher_paths.root_dir().join(".env");
    let client_id = configured_microsoft_client_id(&env_path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            [
                "Microsoft login is not configured.",
                "Set MICROSOFT_CLIENT_ID in the launcher .env file, process environment,",
                "or build environment to a public Microsoft Entra app registration",
                "that allows personal Microsoft accounts and redirects to",
                "http://localhost/callback.",
            ]
            .join(" ")
        })?;

    let db_path = launcher_paths.database_path().to_path_buf();

    let login_result = run_microsoft_login(&client_id)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Save to database
    let connection = Connection::open(&db_path).map_err(|e| e.to_string())?;
    let manager = AccountManager::new(&connection, KeyringSecretStore::new());
    manager
        .save_account_login(
            ManagedAccountProfile {
                microsoft_id: login_result.microsoft_id.clone(),
                xbox_gamertag: Some(login_result.minecraft_username.clone()),
                minecraft_uuid: Some(login_result.minecraft_uuid.clone()),
                profile_data: Some(
                    serde_json::json!({
                        "username": login_result.minecraft_username,
                        "uuid": login_result.minecraft_uuid,
                        "mc_access_token": login_result.minecraft_access_token,
                        "ms_refresh_token": login_result.microsoft_refresh_token,
                    })
                    .to_string(),
                ),
            },
            ManagedAccountTokens {
                access_token: login_result.minecraft_access_token.clone(),
                refresh_token: login_result.microsoft_refresh_token,
            },
            true, // make active
        )
        .map_err(|e| e.to_string())?;

    Ok(login_result
        .xbox_gamertag
        .unwrap_or(login_result.microsoft_id))
}

#[tauri::command]
pub fn delete_account_command(
    launcher_paths: State<'_, LauncherPaths>,
    microsoft_id: String,
) -> Result<(), String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;
    AccountsRepository::new(&connection)
        .delete_account(&microsoft_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn save_global_settings_command(
    launcher_paths: State<'_, LauncherPaths>,
    settings: ShellGlobalSettingsInput,
) -> Result<(), String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;

    save_global_settings(&connection, &settings).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn save_modlist_overrides_command(
    launcher_paths: State<'_, LauncherPaths>,
    overrides: ShellModListOverridesInput,
) -> Result<(), String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;

    save_modlist_overrides(&connection, &overrides).map_err(|error| error.to_string())
}

pub fn load_shell_snapshot_from_root(
    root_dir: &Path,
    selected_modlist_name: Option<&str>,
) -> Result<ShellSnapshot> {
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;

    let raw_modlists = load_modlist_summaries(launcher_paths.modlists_dir())?;
    let selected_modlist_name = selected_modlist_name
        .map(ToString::to_string)
        .or_else(|| raw_modlists.first().map(|modlist| modlist.name.clone()));
    let active_account = load_active_account_summary(&connection)?;
    let global_settings = load_global_settings(&connection)?;
    let selected_modlist_overrides =
        load_modlist_overrides(&connection, selected_modlist_name.as_deref())?;

    let version_loaders = load_all_modlist_version_loaders(&connection)?;
    let modlists: Vec<ShellModListSummary> = raw_modlists
        .into_iter()
        .map(|mut s| {
            if let Some((ver, loader)) = version_loaders.get(&s.name) {
                s.minecraft_version = ver.clone();
                s.mod_loader = loader.clone();
            }
            s
        })
        .collect();

    Ok(ShellSnapshot {
        modlists,
        active_account,
        global_settings,
        selected_modlist_overrides,
    })
}

fn load_all_modlist_version_loaders(
    connection: &Connection,
) -> Result<HashMap<String, (Option<String>, Option<String>)>> {
    let mut stmt = connection.prepare(
        "SELECT modlist_name, key, value FROM modlist_settings \
         WHERE key IN ('minecraft_version', 'mod_loader')",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut map: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for row in rows {
        let (name, key, value) = row?;
        let entry = map.entry(name).or_default();
        if key == "minecraft_version" {
            entry.0 = Some(value);
        } else if key == "mod_loader" {
            entry.1 = Some(value);
        }
    }
    Ok(map)
}

fn load_modlist_summaries(modlists_dir: &Path) -> Result<Vec<ShellModListSummary>> {
    if !modlists_dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();

    for entry in fs::read_dir(modlists_dir)
        .with_context(|| format!("failed to scan {}", modlists_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let rules_path = path.join(RULES_FILENAME);
        if rules_path.exists() {
            let modlist = ModList::read_from_file(&rules_path)?;
            summaries.push(ShellModListSummary {
                name: modlist.modlist_name,
                description: modlist.description,
                author: Some(modlist.author),
                rule_count: modlist.rules.len(),
                minecraft_version: None,
                mod_loader: None,
            });
        } else if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            summaries.push(ShellModListSummary {
                name: name.to_string(),
                description: String::new(),
                author: None,
                rule_count: 0,
                minecraft_version: None,
                mod_loader: None,
            });
        }
    }

    summaries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(summaries)
}

fn load_active_account_summary(connection: &Connection) -> Result<Option<ShellActiveAccount>> {
    let account = AccountsRepository::new(connection).load_active_account()?;

    Ok(account.map(|account| {
        // Try to get the Minecraft username from profile_data.
        let mc_username = account.profile_data.as_deref().and_then(|pd| {
            serde_json::from_str::<serde_json::Value>(pd)
                .ok()
                .and_then(|v| v.get("username").and_then(|u| u.as_str()).map(String::from))
        });

        let avatar_url = account.minecraft_uuid.as_ref().map(|uuid| {
            let clean_uuid = uuid.replace('-', "");
            format!("https://mc-heads.net/avatar/{clean_uuid}/32")
        });

        ShellActiveAccount {
            microsoft_id: account.microsoft_id,
            xbox_gamertag: mc_username.or(account.xbox_gamertag),
            minecraft_uuid: account.minecraft_uuid,
            avatar_url,
            status: if account.access_token_enc.is_some() {
                "online".to_string()
            } else {
                "offline".to_string()
            },
            last_mode: if account.access_token_enc.is_some() {
                "microsoft".to_string()
            } else {
                "offline".to_string()
            },
        }
    }))
}

fn load_global_settings(connection: &Connection) -> Result<ShellGlobalSettings> {
    let values = load_key_value_settings(connection, None)?;

    Ok(ShellGlobalSettings {
        min_ram_mb: parse_u32_setting(&values, "min_ram_mb").unwrap_or(2048),
        max_ram_mb: parse_u32_setting(&values, "max_ram_mb").unwrap_or(4096),
        custom_jvm_args: values
            .get("custom_jvm_args")
            .cloned()
            .unwrap_or_else(|| "-XX:+UseG1GC -XX:+ParallelRefProcEnabled".to_string()),
        profiler_enabled: parse_bool_setting(&values, "profiler_enabled").unwrap_or(false),
        cache_only_mode: parse_bool_setting(&values, "cache_only_mode").unwrap_or(false),
        wrapper_command: values.get("wrapper_command").cloned().unwrap_or_default(),
        java_path_override: values
            .get("java_path_override")
            .cloned()
            .unwrap_or_default(),
    })
}

fn load_modlist_overrides(
    connection: &Connection,
    modlist_name: Option<&str>,
) -> Result<ShellModListOverrides> {
    let values = load_key_value_settings(connection, modlist_name)?;

    Ok(ShellModListOverrides {
        modlist_name: modlist_name.map(ToString::to_string),
        min_ram_mb: parse_u32_setting(&values, "min_ram_mb"),
        max_ram_mb: parse_u32_setting(&values, "max_ram_mb"),
        custom_jvm_args: values.get("custom_jvm_args").cloned(),
        profiler_enabled: parse_bool_setting(&values, "profiler_enabled"),
        wrapper_command: values.get("wrapper_command").cloned(),
        minecraft_version: values.get("minecraft_version").cloned(),
        mod_loader: values.get("mod_loader").cloned(),
    })
}

pub fn save_global_settings(
    connection: &Connection,
    settings: &ShellGlobalSettingsInput,
) -> Result<()> {
    if settings.min_ram_mb == 0 {
        bail!("min_ram_mb must be greater than zero");
    }
    if settings.max_ram_mb == 0 {
        bail!("max_ram_mb must be greater than zero");
    }
    if settings.min_ram_mb > settings.max_ram_mb {
        bail!("min_ram_mb cannot exceed max_ram_mb");
    }

    let values = [
        ("min_ram_mb", settings.min_ram_mb.to_string()),
        ("max_ram_mb", settings.max_ram_mb.to_string()),
        ("custom_jvm_args", settings.custom_jvm_args.clone()),
        (
            "profiler_enabled",
            if settings.profiler_enabled {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ),
        (
            "cache_only_mode",
            if settings.cache_only_mode {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ),
        ("wrapper_command", settings.wrapper_command.clone()),
        ("java_path_override", settings.java_path_override.clone()),
    ];

    replace_global_settings(connection, &values)
}

pub fn save_modlist_overrides(
    connection: &Connection,
    overrides: &ShellModListOverridesInput,
) -> Result<()> {
    let mut values = Vec::new();

    if let Some(value) = overrides.min_ram_mb {
        values.push(("min_ram_mb", value.to_string()));
    }
    if let Some(value) = overrides.max_ram_mb {
        values.push(("max_ram_mb", value.to_string()));
    }
    if let Some(value) = &overrides.custom_jvm_args {
        values.push(("custom_jvm_args", value.clone()));
    }
    if let Some(value) = overrides.profiler_enabled {
        values.push((
            "profiler_enabled",
            if value { "true" } else { "false" }.to_string(),
        ));
    }
    if let Some(value) = &overrides.wrapper_command {
        values.push(("wrapper_command", value.clone()));
    }
    if let Some(value) = &overrides.minecraft_version {
        values.push(("minecraft_version", value.clone()));
    }
    if let Some(value) = &overrides.mod_loader {
        values.push(("mod_loader", value.clone()));
    }

    replace_modlist_settings(connection, &overrides.modlist_name, &values)
}

fn load_key_value_settings(
    connection: &Connection,
    modlist_name: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut settings = HashMap::new();

    match modlist_name {
        Some(modlist_name) => {
            let mut statement = connection
                .prepare("SELECT key, value FROM modlist_settings WHERE modlist_name = ?1")?;

            let rows = statement.query_map([modlist_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            for row in rows {
                let (key, value) = row?;
                settings.insert(key, value);
            }
        }
        None => {
            let mut statement = connection.prepare("SELECT key, value FROM global_settings")?;

            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            for row in rows {
                let (key, value) = row?;
                settings.insert(key, value);
            }
        }
    }

    Ok(settings)
}

fn parse_u32_setting(settings: &HashMap<String, String>, key: &str) -> Option<u32> {
    settings.get(key)?.trim().parse::<u32>().ok()
}

fn parse_bool_setting(settings: &HashMap<String, String>, key: &str) -> Option<bool> {
    let value = settings.get(key)?.trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn replace_global_settings(connection: &Connection, values: &[(&str, String)]) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute("DELETE FROM global_settings", [])?;

    for (key, value) in values {
        transaction.execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
            [*key, value.as_str()],
        )?;
    }

    transaction.commit()?;
    Ok(())
}

fn replace_modlist_settings(
    connection: &Connection,
    modlist_name: &str,
    values: &[(&str, String)],
) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "DELETE FROM modlist_settings WHERE modlist_name = ?1",
        [modlist_name],
    )?;

    for (key, value) in values {
        transaction.execute(
            "INSERT INTO modlist_settings (modlist_name, key, value) VALUES (?1, ?2, ?3)",
            [modlist_name, *key, value.as_str()],
        )?;
    }

    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{params, Connection};

    use crate::database::initialize_database;
    use crate::microsoft_auth::{AccountRecord, AccountsRepository};
    use crate::rules::{ModList, ModSource, Rule};

    use super::{
        load_shell_snapshot_from_root, save_global_settings, save_modlist_overrides,
        ShellGlobalSettingsInput, ShellModListOverridesInput,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-shell-snapshot-test-{timestamp}"))
    }

    #[test]
    fn load_shell_snapshot_returns_defaults_for_empty_workspace() {
        let root_dir = unique_test_root();
        fs::create_dir_all(&root_dir).expect("root directory should exist");
        initialize_database(&root_dir.join("launcher_data.db"))
            .expect("database should initialize");
        fs::create_dir_all(root_dir.join("mod-lists")).expect("mod-lists directory should exist");

        let snapshot =
            load_shell_snapshot_from_root(&root_dir, None).expect("snapshot should load");

        assert!(snapshot.modlists.is_empty());
        assert!(snapshot.active_account.is_none());
        assert_eq!(snapshot.global_settings.min_ram_mb, 2048);
        assert_eq!(snapshot.global_settings.max_ram_mb, 4096);
        assert_eq!(snapshot.selected_modlist_overrides.modlist_name, None);

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn load_shell_snapshot_reads_modlists_account_and_settings() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let modlist_root = root_dir.join("mod-lists").join("Cubic Vanilla+");
        let rules_path = modlist_root.join("rules.json");

        fs::create_dir_all(&modlist_root).expect("modlist directory should exist");
        initialize_database(&database_path).expect("database should initialize");

        ModList {
            modlist_name: "Cubic Vanilla+".into(),
            author: "PlayerLine".into(),
            description: "Primary integrated test pack".into(),
            rules: vec![Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            }],
        }
        .write_to_file(&rules_path)
        .expect("rules should write");

        let connection = Connection::open(&database_path).expect("database should open");
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
                params!["min_ram_mb", "3072"],
            )
            .expect("global setting should insert");
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
                params!["max_ram_mb", "5120"],
            )
            .expect("global setting should insert");
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
                params!["wrapper_command", "gamemoderun"],
            )
            .expect("global setting should insert");
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
                params!["cache_only_mode", "true"],
            )
            .expect("global setting should insert");
        connection
            .execute(
                "INSERT INTO modlist_settings (modlist_name, key, value) VALUES (?1, ?2, ?3)",
                params!["Cubic Vanilla+", "custom_jvm_args", "-Dpack.profile=test"],
            )
            .expect("override should insert");
        AccountsRepository::new(&connection)
            .upsert_account(&AccountRecord {
                microsoft_id: "playerline@outlook.example".into(),
                xbox_gamertag: Some("PlayerLine".into()),
                minecraft_uuid: Some("uuid-a".into()),
                access_token_enc: Some(vec![1, 2, 3]),
                refresh_token_enc: Some(vec![4, 5, 6]),
                profile_data: Some("{}".into()),
                is_active: true,
            })
            .expect("account should insert");
        drop(connection);

        let snapshot =
            load_shell_snapshot_from_root(&root_dir, None).expect("snapshot should load");

        assert_eq!(snapshot.modlists.len(), 1);
        assert_eq!(snapshot.modlists[0].name, "Cubic Vanilla+");
        assert_eq!(
            snapshot.modlists[0].description,
            "Primary integrated test pack"
        );
        assert_eq!(snapshot.modlists[0].author.as_deref(), Some("PlayerLine"));
        assert_eq!(snapshot.modlists[0].rule_count, 1);
        assert_eq!(
            snapshot
                .active_account
                .as_ref()
                .map(|account| account.status.as_str()),
            Some("online")
        );
        assert_eq!(snapshot.global_settings.min_ram_mb, 3072);
        assert_eq!(snapshot.global_settings.max_ram_mb, 5120);
        assert!(snapshot.global_settings.cache_only_mode);
        assert_eq!(snapshot.global_settings.wrapper_command, "gamemoderun");
        assert_eq!(
            snapshot
                .selected_modlist_overrides
                .custom_jvm_args
                .as_deref(),
            Some("-Dpack.profile=test")
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn save_settings_and_explicit_modlist_selection_are_reflected_in_snapshot() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let alpha_modlist_root = root_dir.join("mod-lists").join("Alpha Pack");
        let beta_modlist_root = root_dir.join("mod-lists").join("Beta Pack");

        fs::create_dir_all(&alpha_modlist_root).expect("alpha modlist directory should exist");
        fs::create_dir_all(&beta_modlist_root).expect("beta modlist directory should exist");
        initialize_database(&database_path).expect("database should initialize");

        ModList {
            modlist_name: "Alpha Pack".into(),
            author: "AlphaAuthor".into(),
            description: "Alpha description".into(),
            rules: Vec::new(),
        }
        .write_to_file(&alpha_modlist_root.join("rules.json"))
        .expect("alpha rules should write");
        ModList {
            modlist_name: "Beta Pack".into(),
            author: "BetaAuthor".into(),
            description: "Beta description".into(),
            rules: Vec::new(),
        }
        .write_to_file(&beta_modlist_root.join("rules.json"))
        .expect("beta rules should write");

        let connection = Connection::open(&database_path).expect("database should open");
        save_global_settings(
            &connection,
            &ShellGlobalSettingsInput {
                min_ram_mb: 2304,
                max_ram_mb: 6144,
                custom_jvm_args: "-Dglobal=true".into(),
                profiler_enabled: true,
                cache_only_mode: true,
                wrapper_command: "gamemoderun".into(),
                java_path_override: "/custom/java".into(),
            },
        )
        .expect("global settings should save");
        save_modlist_overrides(
            &connection,
            &ShellModListOverridesInput {
                modlist_name: "Beta Pack".into(),
                min_ram_mb: Some(8192),
                max_ram_mb: None,
                custom_jvm_args: Some("-Dbeta=true".into()),
                profiler_enabled: Some(false),
                wrapper_command: Some("mangohud".into()),
                minecraft_version: None,
                mod_loader: None,
            },
        )
        .expect("modlist overrides should save");
        drop(connection);

        let snapshot = load_shell_snapshot_from_root(&root_dir, Some("Beta Pack"))
            .expect("snapshot should load");

        assert_eq!(snapshot.modlists.len(), 2);
        assert_eq!(snapshot.global_settings.min_ram_mb, 2304);
        assert_eq!(snapshot.global_settings.max_ram_mb, 6144);
        assert_eq!(snapshot.global_settings.custom_jvm_args, "-Dglobal=true");
        assert!(snapshot.global_settings.profiler_enabled);
        assert!(snapshot.global_settings.cache_only_mode);
        assert_eq!(snapshot.global_settings.wrapper_command, "gamemoderun");
        assert_eq!(snapshot.global_settings.java_path_override, "/custom/java");
        assert_eq!(
            snapshot.selected_modlist_overrides.modlist_name.as_deref(),
            Some("Beta Pack")
        );
        assert_eq!(snapshot.selected_modlist_overrides.min_ram_mb, Some(8192));
        assert_eq!(
            snapshot
                .selected_modlist_overrides
                .custom_jvm_args
                .as_deref(),
            Some("-Dbeta=true")
        );
        assert_eq!(
            snapshot.selected_modlist_overrides.profiler_enabled,
            Some(false)
        );
        assert_eq!(
            snapshot
                .selected_modlist_overrides
                .wrapper_command
                .as_deref(),
            Some("mangohud")
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
    #[test]
    fn save_global_settings_rejects_invalid_ram() {
        let connection = Connection::open_in_memory().expect("in-memory database should open");
        connection
            .execute_batch(
                "CREATE TABLE global_settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .expect("global settings schema should initialize");

        let settings = |min_ram_mb, max_ram_mb| ShellGlobalSettingsInput {
            min_ram_mb,
            max_ram_mb,
            custom_jvm_args: String::new(),
            profiler_enabled: false,
            cache_only_mode: false,
            wrapper_command: String::new(),
            java_path_override: String::new(),
        };

        let error = save_global_settings(&connection, &settings(0, 4096))
            .expect_err("zero minimum RAM should be rejected");
        assert_eq!(error.to_string(), "min_ram_mb must be greater than zero");

        let error = save_global_settings(&connection, &settings(1024, 0))
            .expect_err("zero maximum RAM should be rejected");
        assert_eq!(error.to_string(), "max_ram_mb must be greater than zero");

        let error = save_global_settings(&connection, &settings(8192, 4096))
            .expect_err("minimum RAM above maximum RAM should be rejected");
        assert_eq!(error.to_string(), "min_ram_mb cannot exceed max_ram_mb");

        save_global_settings(&connection, &settings(1024, 4096))
            .expect("valid RAM settings should save");
    }
}
