use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::oneshot;

use crate::account_manager::{
    AccountManager, AccountSummary, ManagedAccountProfile, ManagedAccountTokens,
};
use crate::launcher_paths::LauncherPaths;
use crate::microsoft_auth::{
    configured_microsoft_client_id, run_microsoft_login, AccountsRepository,
};
use crate::rules::{ModList, RULES_FILENAME};
use crate::token_storage::{CredentialState, KeyringSecretStore, SecretStore};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellSnapshot {
    pub modlists: Vec<ShellModListSummary>,
    /// Mod list folders whose `rules.json` could not be read. They are left
    /// out of `modlists` instead of failing the whole snapshot (todo.md:1414):
    /// one broken folder must not hide the others, the settings and the
    /// accounts, nor stop the launch of another mod list.
    pub unreadable_modlists: Vec<ShellUnreadableModList>,
    pub active_account: Option<ShellAccount>,
    pub global_settings: ShellGlobalSettings,
    pub selected_modlist_overrides: ShellModListOverrides,
}

/// A mod list folder left out of the snapshot, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellUnreadableModList {
    /// The folder name under `mod-lists/`.
    pub folder: String,
    pub error: String,
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

/// One saved account as the interface shows it: the snapshot's active
/// account and every row of the account list. Both are built by
/// `shell_account`, so the same account cannot read differently in the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellAccount {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub avatar_url: Option<String>,
    pub status: String,
    pub last_mode: String,
    /// `usable`, `signed_out`, `unreadable` or `keyring_unavailable` (F1):
    /// why `status` says what it says, so the interface can tell the user
    /// what to do instead of launching offline in silence.
    pub credentials: String,
    /// The keyring's own error, only for `keyring_unavailable`.
    pub credentials_detail: Option<String>,
    /// The user's choice to play this account offline (D115): every launch
    /// with it is offline, whatever `credentials` says. A second sign apart
    /// from `status` on purpose: choosing offline is not a broken sign-in.
    pub plays_offline: bool,
    /// The row the launch uses. The account list carries it so that the
    /// interface learns which account is active from the list alone, without
    /// depending on the snapshot (todo.md:1414, todo.md:1390).
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellGlobalSettings {
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub custom_jvm_args: String,
    pub profiler_enabled: bool,
    /// Whether a launch may stop to show the update popup. It does not decide
    /// whether the update pre-check runs: the launch needs the pre-check's
    /// version map either way, or a mod that was never downloaded would never
    /// be installed.
    pub update_notifications_enabled: bool,
    /// The content categories the popup may report on. Mods are always
    /// included and have no flag. Persisted only: the content-pack side of the
    /// pre-check is C4, so nothing reads these three yet.
    pub update_notifications_resource_packs: bool,
    pub update_notifications_data_packs: bool,
    pub update_notifications_shaders: bool,
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
    pub update_notifications_enabled: bool,
    pub update_notifications_resource_packs: bool,
    pub update_notifications_data_packs: bool,
    pub update_notifications_shaders: bool,
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

/// Async, with the database and the keyring on a blocking thread: a
/// synchronous Tauri command runs on the main thread, and on Linux a locked
/// keyring can hold the call for as long as its unlock prompt stays open,
/// freezing the whole window (account-8).
#[tauri::command]
pub async fn load_shell_snapshot_command(
    launcher_paths: State<'_, LauncherPaths>,
    selected_modlist_name: Option<String>,
) -> Result<ShellSnapshot, String> {
    let root_dir = launcher_paths.root_dir().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        load_shell_snapshot_from_root(
            &root_dir,
            selected_modlist_name.as_deref(),
            KeyringSecretStore::new(),
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
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

/// Every saved account, the active one first (D113): the list the Accounts
/// window shows and switches between. Off the main thread for the same
/// reason as the snapshot (account-8): it asks the keyring.
#[tauri::command]
pub async fn list_accounts_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<Vec<ShellAccount>, String> {
    let database_path = launcher_paths.database_path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let connection = Connection::open(&database_path).map_err(|error| error.to_string())?;
        load_account_list_with_secret_store(&connection, KeyringSecretStore::new())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Whether the active account plays offline (D115), from its row. The app's
/// own requests ask this before going to the network (D140): with it the
/// launcher works from what it has on disk, and asks nobody. No active
/// account plays online, as before.
pub fn active_account_plays_offline(launcher_paths: &LauncherPaths) -> Result<bool> {
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;
    Ok(AccountsRepository::new(&connection)
        .load_active_account()?
        .is_some_and(|account| account.plays_offline))
}

/// Saves the choice to play one account offline (D115). The launch and the
/// update pre-check read it back from the database, never from the frontend.
#[tauri::command]
pub fn set_account_plays_offline_command(
    launcher_paths: State<'_, LauncherPaths>,
    microsoft_id: String,
    plays_offline: bool,
) -> Result<(), String> {
    let connection =
        Connection::open(launcher_paths.database_path()).map_err(|error| error.to_string())?;
    AccountsRepository::new(&connection)
        .set_plays_offline(&microsoft_id, plays_offline)
        .map_err(|error| error.to_string())
}

/// The event that carries the Microsoft page the browser was sent to, so
/// that the Accounts window can show it as a link (account-3).
pub const MICROSOFT_LOGIN_URL_EVENT: &str = "microsoft-login-url";

/// What `microsoft_login_command` answers when the user pressed Cancel. The
/// frontend compares against it to tell a cancel from a failure.
pub const MICROSOFT_LOGIN_CANCELLED: &str = "Microsoft sign-in cancelled";

/// The sign-in in progress, if any: the sender that cancels it (account-3).
#[derive(Default)]
pub struct MicrosoftLoginState {
    cancel: Mutex<Option<oneshot::Sender<()>>>,
}

/// Holds the cancel slot for the length of one sign-in and empties it however
/// the sign-in ends.
struct LoginInProgress<'a>(&'a MicrosoftLoginState);

impl<'a> LoginInProgress<'a> {
    fn start(state: &'a MicrosoftLoginState) -> Result<(Self, oneshot::Receiver<()>), String> {
        let mut slot = state.cancel.lock();
        if slot.is_some() {
            return Err("A Microsoft sign-in is already in progress".to_string());
        }
        let (sender, receiver) = oneshot::channel();
        *slot = Some(sender);
        Ok((Self(state), receiver))
    }
}

impl Drop for LoginInProgress<'_> {
    fn drop(&mut self) {
        self.0.cancel.lock().take();
    }
}

/// Stops the sign-in in progress: the loopback listener closes and
/// `microsoft_login_command` answers `MICROSOFT_LOGIN_CANCELLED`. Nothing is
/// saved. Without a sign-in in progress it does nothing.
#[tauri::command]
pub fn cancel_microsoft_login_command(login_state: State<'_, MicrosoftLoginState>) {
    if let Some(sender) = login_state.cancel.lock().take() {
        let _ = sender.send(());
    }
}

/// Whether a "Sign in again" on one row came back with another Microsoft
/// account (todo.md:1417). The Microsoft id is the row's key; the Minecraft
/// UUID also counts as the same player, for a row saved before Microsoft
/// returned its own user id.
fn signed_in_as_another_account(
    expected_microsoft_id: &str,
    expected_minecraft_uuid: Option<&str>,
    login_microsoft_id: &str,
    login_minecraft_uuid: &str,
) -> bool {
    let same_uuid = expected_minecraft_uuid.is_some_and(|uuid| {
        !uuid.is_empty() && uuid.replace('-', "") == login_minecraft_uuid.replace('-', "")
    });
    expected_microsoft_id != login_microsoft_id && !same_uuid
}

/// Starts the Microsoft OAuth login flow, opens the browser, waits for
/// callback, exchanges tokens through Xbox Live → XSTS → Minecraft, and
/// saves the account to the database.
///
/// `replace_unreadable` is the "Sign in again" button (F1): it allows the save
/// to discard saved sign-ins that the keyring says nobody can decrypt.
///
/// `expected_microsoft_id` is the row a "Sign in again" belongs to: when the
/// browser signs in another Microsoft account, nothing is saved and the
/// answer says so, instead of adding or activating that other account while
/// the row stays as it was (todo.md:1417).
///
/// The page the browser is sent to goes out as `MICROSOFT_LOGIN_URL_EVENT`,
/// and `cancel_microsoft_login_command` ends the wait (account-3).
#[tauri::command]
pub async fn microsoft_login_command(
    app: AppHandle,
    launcher_paths: State<'_, LauncherPaths>,
    login_state: State<'_, MicrosoftLoginState>,
    replace_unreadable: Option<bool>,
    expected_microsoft_id: Option<String>,
) -> Result<String, String> {
    let replace_unreadable = replace_unreadable.unwrap_or(false);
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

    // Before the browser: a sign-in that could not be saved must not cost the
    // user the whole OAuth round trip first.
    let expected_account = {
        let connection = Connection::open(&db_path).map_err(|e| e.to_string())?;
        AccountManager::new(&connection, KeyringSecretStore::new())
            .check_login_can_be_saved(replace_unreadable)
            .map_err(|e| format!("{e:#}"))?;
        match expected_microsoft_id.as_deref() {
            Some(expected) => Some(
                AccountsRepository::new(&connection)
                    .list_accounts()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|account| account.microsoft_id == expected)
                    .ok_or_else(|| format!("account '{expected}' does not exist"))?,
            ),
            None => None,
        }
    };

    let (_in_progress, cancelled) = LoginInProgress::start(&login_state)?;
    let url_app = app.clone();
    let login = run_microsoft_login(&client_id, move |url| {
        let _ = url_app.emit(MICROSOFT_LOGIN_URL_EVENT, url.to_string());
    });
    let login_result = tokio::select! {
        result = login => result.map_err(|e| format!("{e:#}"))?,
        _ = cancelled => return Err(MICROSOFT_LOGIN_CANCELLED.to_string()),
    };

    if let Some(expected) = &expected_account {
        if signed_in_as_another_account(
            &expected.microsoft_id,
            expected.minecraft_uuid.as_deref(),
            &login_result.microsoft_id,
            &login_result.minecraft_uuid,
        ) {
            let row_name = resolve_row_name(expected);
            return Err(format!(
                "You signed in as {signed_in}, a different Microsoft account from {row_name}. \
                 Nothing was saved, and {row_name} still needs signing in: press \"Sign in again\" \
                 on its row and choose its account in the browser (\"Use another account\" if \
                 Microsoft signs you straight in). To add {signed_in} as well, use \
                 \"Login with Microsoft\".",
                signed_in = login_result.minecraft_username,
            ));
        }
    }

    // Save to database
    let connection = Connection::open(&db_path).map_err(|e| e.to_string())?;
    let manager = AccountManager::new(&connection, KeyringSecretStore::new());
    manager
        .save_account_login(
            ManagedAccountProfile {
                microsoft_id: login_result.microsoft_id.clone(),
                xbox_gamertag: Some(login_result.minecraft_username.clone()),
                minecraft_uuid: Some(login_result.minecraft_uuid.clone()),
                profile_data: Some(account_profile_data_json(
                    &login_result.minecraft_username,
                    &login_result.minecraft_uuid,
                )),
            },
            ManagedAccountTokens {
                access_token: login_result.minecraft_access_token.clone(),
                refresh_token: login_result.microsoft_refresh_token,
            },
            true, // make active
            replace_unreadable,
        )
        .map_err(|e| format!("{e:#}"))?;

    Ok(login_result
        .xbox_gamertag
        .unwrap_or(login_result.microsoft_id))
}

/// The name a row shows: the Minecraft name saved with it, else its gamertag,
/// else its Microsoft id.
fn resolve_row_name(account: &crate::microsoft_auth::AccountRecord) -> String {
    account
        .profile_data
        .as_deref()
        .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .and_then(|data| {
            data.get("username")
                .and_then(|u| u.as_str())
                .map(String::from)
        })
        .or_else(|| account.xbox_gamertag.clone())
        .unwrap_or_else(|| account.microsoft_id.clone())
}

/// Build the profile_data JSON stored for an account.
///
/// SECURITY (C1): profile_data is persisted UNENCRYPTED, so it MUST contain
/// only non-sensitive display metadata (username, uuid). Access/refresh tokens
/// live exclusively in the AES-GCM encrypted `access_token_enc`/`refresh_token_enc`
/// columns and MUST NOT be written here.
pub fn account_profile_data_json(username: &str, uuid: &str) -> String {
    serde_json::json!({
        "username": username,
        "uuid": uuid,
    })
    .to_string()
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

/// `secret_store` is where the active account's token key lives. The commands
/// pass the system keyring; tests pass a store in memory, so that a test run
/// never reaches anyone's real credentials (F1b).
pub fn load_shell_snapshot_from_root<S: SecretStore>(
    root_dir: &Path,
    selected_modlist_name: Option<&str>,
    secret_store: S,
) -> Result<ShellSnapshot> {
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let connection = Connection::open(launcher_paths.database_path()).with_context(|| {
        format!(
            "failed to open launcher database at {}",
            launcher_paths.database_path().display()
        )
    })?;

    let (raw_modlists, unreadable_modlists) =
        load_modlist_summaries(launcher_paths.modlists_dir())?;
    let selected_modlist_name = selected_modlist_name
        .map(ToString::to_string)
        .or_else(|| raw_modlists.first().map(|modlist| modlist.name.clone()));
    let active_account = load_active_account_summary_with_secret_store(&connection, secret_store)?;
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
        unreadable_modlists,
        active_account,
        global_settings,
        selected_modlist_overrides,
    })
}

/// `modlist_name` → (`minecraft_version`, `mod_loader`) as stored in `modlist_settings`.
type VersionLoaderByModlist = HashMap<String, (Option<String>, Option<String>)>;

fn load_all_modlist_version_loaders(connection: &Connection) -> Result<VersionLoaderByModlist> {
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
    let mut map: VersionLoaderByModlist = HashMap::new();
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

/// The mod lists under `mod-lists/`, and the folders left out because their
/// `rules.json` can't be read. Only a `mod-lists/` that can't be scanned at
/// all is an error.
fn load_modlist_summaries(
    modlists_dir: &Path,
) -> Result<(Vec<ShellModListSummary>, Vec<ShellUnreadableModList>)> {
    if !modlists_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut summaries = Vec::new();
    let mut unreadable = Vec::new();

    for entry in fs::read_dir(modlists_dir)
        .with_context(|| format!("failed to scan {}", modlists_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // An import in progress, or one cut short, is not a mod list; nor is
        // what a permanent deletion could not remove yet.
        if crate::modlist_manager::is_launcher_work_dir(&entry.file_name().to_string_lossy()) {
            continue;
        }
        // `dati-6`: the folder name is the mod list. Every command — editor,
        // launch, export, delete — finds it by that name, so the sidebar
        // shows that name too; `modlist_name` inside rules.json is only a
        // label. A folder copied by hand («Pack copy») then stays itself, and
        // deleting it can never reach the original «Pack».
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            eprintln!(
                "[shell] skipping {}: its name is not valid UTF-8",
                path.display()
            );
            continue;
        };

        let rules_path = path.join(RULES_FILENAME);
        if rules_path.exists() {
            let modlist = match ModList::read_from_file(&rules_path) {
                Ok(modlist) => modlist,
                Err(error) => {
                    let folder = name.to_string();
                    eprintln!("[Shell] Skipping mod list '{folder}': {error:#}");
                    unreadable.push(ShellUnreadableModList {
                        folder,
                        error: format!("{error:#}"),
                    });
                    continue;
                }
            };
            if modlist.modlist_name != name {
                eprintln!(
                    "[shell] the mod list in folder '{name}' calls itself '{}' in its {RULES_FILENAME}: the folder name is used",
                    modlist.modlist_name
                );
            }
            summaries.push(ShellModListSummary {
                name: name.to_string(),
                description: modlist.description,
                author: Some(modlist.author),
                rule_count: modlist.rules.len(),
                minecraft_version: None,
                mod_loader: None,
            });
        } else {
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
    unreadable.sort_by(|left, right| left.folder.cmp(&right.folder));
    Ok((summaries, unreadable))
}

fn load_active_account_summary_with_secret_store<S: SecretStore>(
    connection: &Connection,
    secret_store: S,
) -> Result<Option<ShellAccount>> {
    Ok(AccountManager::new(connection, secret_store)
        .active_account_summary()?
        .map(shell_account))
}

/// Every row of the `accounts` table, including those whose sign-in can no
/// longer be read: they show with their way out instead of disappearing
/// (D117). The active account comes first, the rest by last sign-in.
fn load_account_list_with_secret_store<S: SecretStore>(
    connection: &Connection,
    secret_store: S,
) -> Result<Vec<ShellAccount>> {
    let mut summaries = AccountManager::new(connection, secret_store).list_account_summaries()?;
    // Stable: the rows keep the repository's `last_login DESC` order.
    summaries.sort_by_key(|summary| !summary.is_active);
    Ok(summaries.into_iter().map(shell_account).collect())
}

/// The one place where a saved account becomes what the interface shows:
/// `status` and `last_mode` follow from `credentials` here and nowhere else,
/// and `plays_offline` passes through untouched — the choice never writes
/// `status`.
fn shell_account(summary: AccountSummary) -> ShellAccount {
    // Try to get the Minecraft username from profile_data.
    let mc_username = summary.profile_data.as_deref().and_then(|pd| {
        serde_json::from_str::<serde_json::Value>(pd)
            .ok()
            .and_then(|v| v.get("username").and_then(|u| u.as_str()).map(String::from))
    });

    let avatar_url = summary.minecraft_uuid.as_ref().map(|uuid| {
        let clean_uuid = uuid.replace('-', "");
        format!("https://mc-heads.net/avatar/{clean_uuid}/32")
    });
    let connected = summary.credentials == CredentialState::Usable;
    let credentials_detail = match &summary.credentials {
        CredentialState::KeyringUnavailable(detail) => Some(detail.clone()),
        _ => None,
    };

    ShellAccount {
        microsoft_id: summary.microsoft_id,
        xbox_gamertag: mc_username.or(summary.xbox_gamertag),
        minecraft_uuid: summary.minecraft_uuid,
        avatar_url,
        status: if connected {
            "online".to_string()
        } else {
            "offline".to_string()
        },
        last_mode: if connected {
            "microsoft".to_string()
        } else {
            "offline".to_string()
        },
        credentials: summary.credentials.as_str().to_string(),
        credentials_detail,
        plays_offline: summary.plays_offline,
        is_active: summary.is_active,
    }
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
        // D12: no migration. The keys are new, the rows are not there, and the
        // default is what a fresh install gets: notifications on, every
        // category included. The orphaned `cache_only_mode` row is ignored,
        // and now stays: since E10 a save writes only its own keys
        // (`upsert_global_settings`) instead of rewriting the whole table.
        update_notifications_enabled: parse_bool_setting(&values, "update_notifications_enabled")
            .unwrap_or(true),
        update_notifications_resource_packs: parse_bool_setting(
            &values,
            "update_notifications_resource_packs",
        )
        .unwrap_or(true),
        update_notifications_data_packs: parse_bool_setting(
            &values,
            "update_notifications_data_packs",
        )
        .unwrap_or(true),
        update_notifications_shaders: parse_bool_setting(&values, "update_notifications_shaders")
            .unwrap_or(true),
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
        ("profiler_enabled", bool_setting(settings.profiler_enabled)),
        (
            "update_notifications_enabled",
            bool_setting(settings.update_notifications_enabled),
        ),
        (
            "update_notifications_resource_packs",
            bool_setting(settings.update_notifications_resource_packs),
        ),
        (
            "update_notifications_data_packs",
            bool_setting(settings.update_notifications_data_packs),
        ),
        (
            "update_notifications_shaders",
            bool_setting(settings.update_notifications_shaders),
        ),
        ("wrapper_command", settings.wrapper_command.clone()),
        ("java_path_override", settings.java_path_override.clone()),
    ];

    upsert_global_settings(connection, &values)
}

/// shell-5: la RAM che un lancio di questa mod list userebbe davvero, cioè
/// l'override dove c'è e il valore globale altrove, va bene insieme. Min e
/// max si combinano ognuno per conto suo, quindi un «Max RAM» di 1536 sopra
/// un minimo globale di 2048 passava il salvataggio e faceva fallire il
/// lancio solo alla fine della preparazione, download compresi.
pub fn validate_effective_ram(
    global_min_ram_mb: u32,
    global_max_ram_mb: u32,
    override_min_ram_mb: Option<u32>,
    override_max_ram_mb: Option<u32>,
) -> Result<()> {
    let describe = |value: u32, from_override: bool| {
        if from_override {
            format!("{value} MB, set for this mod list")
        } else {
            format!("{value} MB, from the global settings")
        }
    };
    let min = override_min_ram_mb.unwrap_or(global_min_ram_mb);
    let max = override_max_ram_mb.unwrap_or(global_max_ram_mb);
    let min_label = describe(min, override_min_ram_mb.is_some());
    let max_label = describe(max, override_max_ram_mb.is_some());

    if min == 0 {
        bail!("Minimum RAM ({min_label}) must be greater than zero.");
    }
    if max == 0 {
        bail!("Maximum RAM ({max_label}) must be greater than zero.");
    }
    if min > max {
        bail!(
            "Minimum RAM ({min_label}) cannot exceed maximum RAM ({max_label}). Change one of them in Settings."
        );
    }
    Ok(())
}

pub fn save_modlist_overrides(
    connection: &Connection,
    overrides: &ShellModListOverridesInput,
) -> Result<()> {
    let global = load_global_settings(connection)?;
    validate_effective_ram(
        global.min_ram_mb,
        global.max_ram_mb,
        overrides.min_ram_mb,
        overrides.max_ram_mb,
    )?;

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

/// The written form of a boolean setting, in the spelling `parse_bool_setting`
/// reads back.
fn bool_setting(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// Write the settings form's own keys, leaving every other row alone.
///
/// It used to `DELETE FROM global_settings` first, which made the form the
/// owner of the whole table: any key written by something else — the hidden
/// worlds list of E10 (`worlds::HIDDEN_WORLDS_KEY`) — disappeared the next
/// time the user pressed Save. The upsert keeps the table what its schema
/// says it is, a key/value store with more than one writer.
fn upsert_global_settings(connection: &Connection, values: &[(&str, String)]) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;

    for (key, value) in values {
        transaction.execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
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
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{params, Connection};

    use crate::database::initialize_database;
    use crate::microsoft_auth::{AccountRecord, AccountsRepository};
    use crate::rules::{ModList, ModSource, Rule};
    use crate::token_storage::{AccountTokenCipher, SecretStore};

    use super::{
        account_profile_data_json, active_account_plays_offline,
        load_account_list_with_secret_store, load_active_account_summary_with_secret_store,
        load_shell_snapshot_from_root, save_global_settings, save_modlist_overrides,
        signed_in_as_another_account, ShellGlobalSettingsInput, ShellModListOverridesInput,
    };
    use crate::launcher_paths::LauncherPaths;

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-shell-snapshot-test-{timestamp}"))
    }

    #[derive(Clone, Default)]
    struct MemorySecretStore {
        values: Arc<Mutex<HashMap<String, String>>>,
    }

    impl SecretStore for MemorySecretStore {
        fn get_secret(&self, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self
                .values
                .lock()
                .expect("secret store mutex poisoned")
                .get(key)
                .cloned())
        }

        fn set_secret(&self, key: &str, secret: &str) -> anyhow::Result<()> {
            self.values
                .lock()
                .expect("secret store mutex poisoned")
                .insert(key.to_string(), secret.to_string());
            Ok(())
        }
    }

    #[test]
    fn account_profile_data_json_excludes_tokens() {
        let json = account_profile_data_json("PlayerOne", "uuid-1");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("profile data should be valid json");

        assert_eq!(
            parsed.get("username").and_then(|v| v.as_str()),
            Some("PlayerOne")
        );
        assert_eq!(parsed.get("uuid").and_then(|v| v.as_str()), Some("uuid-1"));
        assert!(parsed.get("mc_access_token").is_none());
        assert!(parsed.get("ms_refresh_token").is_none());
    }

    #[test]
    fn active_account_status_requires_decryptable_refresh_token() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let secret_store = MemorySecretStore::default();
        let refresh_token_enc = AccountTokenCipher::new(secret_store.clone())
            .encrypt_token("refresh-token")
            .expect("refresh token should encrypt");
        AccountsRepository::new(&connection)
            .upsert_account(&AccountRecord {
                microsoft_id: "account-a".into(),
                xbox_gamertag: Some("PlayerA".into()),
                minecraft_uuid: Some("uuid-a".into()),
                access_token_enc: None,
                refresh_token_enc: Some(refresh_token_enc),
                profile_data: None,
                is_active: true,
                plays_offline: false,
            })
            .expect("account should insert");

        let connected =
            load_active_account_summary_with_secret_store(&connection, secret_store.clone())
                .expect("summary should load")
                .expect("active account should exist");
        assert_eq!(connected.status, "online");
        assert_eq!(connected.last_mode, "microsoft");

        connection
            .execute(
                "UPDATE accounts SET refresh_token_enc = ?1 WHERE microsoft_id = ?2",
                params![vec![1_u8, 2, 3], "account-a"],
            )
            .expect("refresh token should corrupt");
        let disconnected = load_active_account_summary_with_secret_store(&connection, secret_store)
            .expect("summary should load")
            .expect("active account should exist");
        assert_eq!(disconnected.status, "offline");
        assert_eq!(disconnected.last_mode, "offline");

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn account_list_puts_the_active_account_first_and_matches_the_snapshot() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let secret_store = MemorySecretStore::default();
        let cipher = AccountTokenCipher::new(secret_store.clone());
        let repository = AccountsRepository::new(&connection);
        // Inserted active first: with equal `last_login` the repository
        // orders by `id DESC`, so the active row comes last before sorting.
        for (microsoft_id, signed_in, is_active) in [
            ("account-active", true, true),
            ("account-other", true, false),
            ("account-signed-out", false, false),
        ] {
            repository
                .upsert_account(&AccountRecord {
                    microsoft_id: microsoft_id.into(),
                    xbox_gamertag: Some(format!("{microsoft_id}-tag")),
                    minecraft_uuid: Some(format!("{microsoft_id}-uuid")),
                    access_token_enc: None,
                    refresh_token_enc: signed_in.then(|| {
                        cipher
                            .encrypt_token("refresh-token")
                            .expect("refresh token should encrypt")
                    }),
                    profile_data: None,
                    is_active,
                    plays_offline: false,
                })
                .expect("account should insert");
        }

        let list = load_account_list_with_secret_store(&connection, secret_store.clone())
            .expect("account list should load");
        let active = load_active_account_summary_with_secret_store(&connection, secret_store)
            .expect("active account should load")
            .expect("active account should exist");

        let rows: Vec<(&str, &str, &str)> = list
            .iter()
            .map(|account| {
                (
                    account.microsoft_id.as_str(),
                    account.status.as_str(),
                    account.credentials.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("account-active", "online", "usable"),
                ("account-signed-out", "offline", "signed_out"),
                ("account-other", "online", "usable"),
            ]
        );
        assert_eq!(list[0], active);

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn load_shell_snapshot_returns_defaults_for_empty_workspace() {
        let root_dir = unique_test_root();
        fs::create_dir_all(&root_dir).expect("root directory should exist");
        initialize_database(&root_dir.join("launcher_data.db"))
            .expect("database should initialize");
        fs::create_dir_all(root_dir.join("mod-lists")).expect("mod-lists directory should exist");

        let snapshot = load_shell_snapshot_from_root(&root_dir, None, MemorySecretStore::default())
            .expect("snapshot should load");

        assert!(snapshot.modlists.is_empty());
        assert!(snapshot.active_account.is_none());
        assert_eq!(snapshot.global_settings.min_ram_mb, 2048);
        assert_eq!(snapshot.global_settings.max_ram_mb, 4096);
        // D12: no migration, so a database that has never seen these keys —
        // which after the rename is every database — must come up with
        // notifications on and all three categories included.
        assert!(snapshot.global_settings.update_notifications_enabled);
        assert!(snapshot.global_settings.update_notifications_resource_packs);
        assert!(snapshot.global_settings.update_notifications_data_packs);
        assert!(snapshot.global_settings.update_notifications_shaders);
        assert_eq!(snapshot.selected_modlist_overrides.modlist_name, None);

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn a_folder_copied_by_hand_is_listed_under_its_own_name() {
        // `dati-6`: «Pack copy» has the rules.json of «Pack», which still says
        // «Pack». The sidebar must show two different names, the folders'.
        let root_dir = unique_test_root();
        initialize_database(&root_dir.join("launcher_data.db"))
            .expect("database should initialize");
        for folder in [
            "Pack",
            "Pack copy",
            ".cubic-delete-1-2",
            ".cubic-import-1-2",
        ] {
            let dir = root_dir.join("mod-lists").join(folder);
            fs::create_dir_all(&dir).expect("modlist directory should exist");
            ModList {
                modlist_name: "Pack".into(),
                author: "Author".into(),
                description: String::new(),
                rules: vec![],
            }
            .write_to_file(&dir.join("rules.json"))
            .expect("rules should write");
        }

        let snapshot = load_shell_snapshot_from_root(&root_dir, None, MemorySecretStore::default())
            .expect("snapshot should load");

        let names: Vec<&str> = snapshot
            .modlists
            .iter()
            .map(|modlist| modlist.name.as_str())
            .collect();
        assert_eq!(names, vec!["Pack", "Pack copy"]);

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
                params!["update_notifications_enabled", "false"],
            )
            .expect("global setting should insert");
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
                params!["update_notifications_shaders", "false"],
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
                plays_offline: false,
            })
            .expect("account should insert");
        drop(connection);

        let snapshot = load_shell_snapshot_from_root(&root_dir, None, MemorySecretStore::default())
            .expect("snapshot should load");

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
            Some("offline")
        );
        assert_eq!(snapshot.global_settings.min_ram_mb, 3072);
        assert_eq!(snapshot.global_settings.max_ram_mb, 5120);
        // A stored `false` has to beat the `true` default, and the two
        // category rows nobody wrote have to fall back to it.
        assert!(!snapshot.global_settings.update_notifications_enabled);
        assert!(!snapshot.global_settings.update_notifications_shaders);
        assert!(snapshot.global_settings.update_notifications_resource_packs);
        assert!(snapshot.global_settings.update_notifications_data_packs);
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
                update_notifications_enabled: false,
                update_notifications_resource_packs: true,
                update_notifications_data_packs: false,
                update_notifications_shaders: true,
                wrapper_command: "gamemoderun".into(),
                java_path_override: "/custom/java".into(),
            },
        )
        .expect("global settings should save");
        save_modlist_overrides(
            &connection,
            &ShellModListOverridesInput {
                modlist_name: "Beta Pack".into(),
                min_ram_mb: Some(4096),
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

        let snapshot = load_shell_snapshot_from_root(
            &root_dir,
            Some("Beta Pack"),
            MemorySecretStore::default(),
        )
        .expect("snapshot should load");

        assert_eq!(snapshot.modlists.len(), 2);
        assert_eq!(snapshot.global_settings.min_ram_mb, 2304);
        assert_eq!(snapshot.global_settings.max_ram_mb, 6144);
        assert_eq!(snapshot.global_settings.custom_jvm_args, "-Dglobal=true");
        assert!(snapshot.global_settings.profiler_enabled);
        // Mixed on purpose: a key swapped between save and load flips one of
        // these four.
        assert!(!snapshot.global_settings.update_notifications_enabled);
        assert!(snapshot.global_settings.update_notifications_resource_packs);
        assert!(!snapshot.global_settings.update_notifications_data_packs);
        assert!(snapshot.global_settings.update_notifications_shaders);
        assert_eq!(snapshot.global_settings.wrapper_command, "gamemoderun");
        assert_eq!(snapshot.global_settings.java_path_override, "/custom/java");
        assert_eq!(
            snapshot.selected_modlist_overrides.modlist_name.as_deref(),
            Some("Beta Pack")
        );
        assert_eq!(snapshot.selected_modlist_overrides.min_ram_mb, Some(4096));
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
    fn an_unreadable_rules_json_skips_that_mod_list_and_keeps_the_rest() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        let good_root = root_dir.join("mod-lists").join("Good Pack");
        let broken_root = root_dir.join("mod-lists").join("Broken Pack");
        fs::create_dir_all(&good_root).expect("good modlist directory should exist");
        fs::create_dir_all(&broken_root).expect("broken modlist directory should exist");
        initialize_database(&database_path).expect("database should initialize");
        ModList {
            modlist_name: "Good Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: Vec::new(),
        }
        .write_to_file(&good_root.join("rules.json"))
        .expect("good rules should write");
        fs::write(broken_root.join("rules.json"), "{ not json").expect("broken rules should write");
        let connection = Connection::open(&database_path).expect("database should open");
        AccountsRepository::new(&connection)
            .upsert_account(&AccountRecord {
                microsoft_id: "account-a".into(),
                xbox_gamertag: Some("PlayerA".into()),
                minecraft_uuid: Some("uuid-a".into()),
                access_token_enc: None,
                refresh_token_enc: None,
                profile_data: None,
                is_active: true,
                plays_offline: false,
            })
            .expect("account should insert");
        drop(connection);

        // Selecting the good one, as a launch of it does.
        let snapshot = load_shell_snapshot_from_root(
            &root_dir,
            Some("Good Pack"),
            MemorySecretStore::default(),
        )
        .expect("one broken mod list must not fail the snapshot");

        let names: Vec<&str> = snapshot.modlists.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["Good Pack"]);
        assert_eq!(snapshot.unreadable_modlists.len(), 1);
        assert_eq!(snapshot.unreadable_modlists[0].folder, "Broken Pack");
        assert!(!snapshot.unreadable_modlists[0].error.is_empty());
        assert_eq!(
            snapshot
                .active_account
                .as_ref()
                .map(|account| account.microsoft_id.as_str()),
            Some("account-a")
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn the_account_list_says_which_row_is_active() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);
        for (microsoft_id, is_active) in [("account-a", false), ("account-b", true)] {
            repository
                .upsert_account(&AccountRecord {
                    microsoft_id: microsoft_id.into(),
                    xbox_gamertag: None,
                    minecraft_uuid: None,
                    access_token_enc: None,
                    refresh_token_enc: None,
                    profile_data: None,
                    is_active,
                    plays_offline: false,
                })
                .expect("account should insert");
        }

        let list = load_account_list_with_secret_store(&connection, MemorySecretStore::default())
            .expect("account list should load");
        let flags: Vec<(&str, bool)> = list
            .iter()
            .map(|account| (account.microsoft_id.as_str(), account.is_active))
            .collect();
        assert_eq!(flags, vec![("account-b", true), ("account-a", false)]);

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn plays_offline_is_read_from_the_active_row_only() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let paths = LauncherPaths::new(root_dir.clone());
        assert!(!active_account_plays_offline(&paths).expect("no account reads as online"));

        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);
        for (microsoft_id, is_active) in [("offline-row", false), ("online-row", true)] {
            repository
                .upsert_account(&AccountRecord {
                    microsoft_id: microsoft_id.into(),
                    xbox_gamertag: None,
                    minecraft_uuid: None,
                    access_token_enc: None,
                    refresh_token_enc: None,
                    profile_data: None,
                    is_active,
                    plays_offline: false,
                })
                .expect("account should insert");
        }
        repository
            .set_plays_offline("offline-row", true)
            .expect("choice should save");
        assert!(!active_account_plays_offline(&paths).expect("active row plays online"));

        repository
            .set_active_account("offline-row")
            .expect("switch should work");
        assert!(active_account_plays_offline(&paths).expect("active row plays offline"));

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn sign_in_again_with_another_microsoft_account_is_told_apart() {
        // Same Microsoft id: the row's own account.
        assert!(!signed_in_as_another_account(
            "ms-a",
            Some("uuid-a"),
            "ms-a",
            "uuid-a"
        ));
        // A row saved with the Minecraft UUID as its id, signed in again with
        // the same player: still the same account.
        assert!(!signed_in_as_another_account(
            "0f1e2d3c-0000-0000-0000-000000000001",
            Some("0f1e2d3c-0000-0000-0000-000000000001"),
            "ms-a",
            "0f1e2d3c000000000000000000000001"
        ));
        // Another player altogether.
        assert!(signed_in_as_another_account(
            "ms-a",
            Some("uuid-a"),
            "ms-b",
            "uuid-b"
        ));
        assert!(signed_in_as_another_account("ms-a", None, "ms-b", "uuid-b"));
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
            update_notifications_enabled: true,
            update_notifications_resource_packs: true,
            update_notifications_data_packs: true,
            update_notifications_shaders: true,
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
