use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app_shell::{ShellGlobalSettings, ShellModListOverrides};
use crate::resolver::ResolutionTarget;
use crate::rules::ModSource;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    pub modlist_name: String,
    pub minecraft_version: String,
    pub mod_loader: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchVerificationRequest {
    pub modlist_name: String,
    pub minecraft_version: String,
    pub mod_loader: String,
    #[serde(default = "default_verification_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_success_after_seconds")]
    pub success_after_seconds: u64,
    #[serde(default = "default_terminate_on_success")]
    pub terminate_on_success: bool,
    #[serde(default = "default_terminate_on_timeout")]
    pub terminate_on_timeout: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchVerificationResult {
    pub started: bool,
    pub success: bool,
    pub state: String,
    pub pid: Option<u32>,
    pub launch_log_dir: Option<String>,
    pub duration_ms: u64,
    pub failure_kind: Option<String>,
    pub failure_summary: Option<String>,
    pub minecraft_log_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchProgressEvent {
    pub state: String,
    pub progress: u8,
    pub stage: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LauncherErrorEvent {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) message: String,
    pub(super) detail: String,
    pub(super) severity: String,
    pub(super) scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EffectiveLaunchSettings {
    pub(super) min_ram_mb: u32,
    pub(super) max_ram_mb: u32,
    pub(super) custom_jvm_args: String,
    pub(super) cache_only_mode: bool,
    pub(super) wrapper_command: Option<String>,
    pub(super) java_path_override: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlayerIdentity {
    pub(super) username: String,
    pub(super) uuid: String,
    pub(super) access_token: String,
    pub(super) user_type: String,
    pub(super) version_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LaunchPlaceholders {
    pub(super) auth_player_name: String,
    pub(super) version_name: String,
    pub(super) game_directory: String,
    pub(super) assets_root: String,
    pub(super) assets_index_name: String,
    pub(super) auth_uuid: String,
    pub(super) auth_access_token: String,
    pub(super) user_type: String,
    pub(super) version_type: String,
    pub(super) library_directory: String,
    pub(super) natives_directory: String,
    pub(super) launcher_name: String,
    pub(super) launcher_version: String,
    pub(super) classpath_separator: String,
    pub(super) resolution_width: String,
    pub(super) resolution_height: String,
}

/// A selected mod from resolution: carries mod_id + source for downstream processing.
#[derive(Debug, Clone)]
pub(super) struct SelectedMod {
    pub(super) mod_id: String,
    pub(super) source: ModSource,
}

#[derive(Debug, Clone)]
pub(super) struct StartedLaunch {
    pub(super) pid: u32,
    pub(super) launch_log_dir: PathBuf,
}

impl LaunchVerificationRequest {
    pub(super) fn into_launch_request(self) -> LaunchRequest {
        LaunchRequest {
            modlist_name: self.modlist_name,
            minecraft_version: self.minecraft_version,
            mod_loader: self.mod_loader,
        }
    }
}

impl SelectedMod {
    pub(super) fn source_label(&self) -> &'static str {
        match self.source {
            ModSource::Modrinth => "modrinth",
            ModSource::Local => "local",
        }
    }
}

impl EffectiveLaunchSettings {
    pub(super) fn from_shell_settings(
        global: &ShellGlobalSettings,
        overrides: &ShellModListOverrides,
    ) -> Self {
        let wrapper_command = overrides
            .wrapper_command
            .clone()
            .unwrap_or_else(|| global.wrapper_command.clone())
            .trim()
            .to_string();
        let java_path_override = global.java_path_override.trim();

        let cache_only_mode = automation_cache_only_override().unwrap_or(global.cache_only_mode);

        Self {
            min_ram_mb: overrides.min_ram_mb.unwrap_or(global.min_ram_mb),
            max_ram_mb: overrides.max_ram_mb.unwrap_or(global.max_ram_mb),
            custom_jvm_args: overrides
                .custom_jvm_args
                .clone()
                .unwrap_or_else(|| global.custom_jvm_args.clone()),
            cache_only_mode,
            wrapper_command: if wrapper_command.is_empty() {
                None
            } else {
                Some(wrapper_command)
            },
            java_path_override: if java_path_override.is_empty() {
                None
            } else {
                Some(PathBuf::from(java_path_override))
            },
        }
    }
}

impl LaunchPlaceholders {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        player_identity: &PlayerIdentity,
        modlist_name: &str,
        target: &ResolutionTarget,
        game_directory: &Path,
        assets_root: &Path,
        asset_index_id: &str,
        library_directory: &Path,
        natives_directory: &Path,
    ) -> Self {
        Self {
            auth_player_name: player_identity.username.clone(),
            version_name: format!(
                "{}-{}-{}",
                modlist_name,
                target.minecraft_version,
                target.mod_loader.as_modrinth_loader()
            ),
            game_directory: game_directory.display().to_string(),
            assets_root: assets_root.display().to_string(),
            assets_index_name: asset_index_id.to_string(),
            auth_uuid: player_identity.uuid.clone(),
            auth_access_token: player_identity.access_token.clone(),
            user_type: player_identity.user_type.clone(),
            version_type: player_identity.version_type.clone(),
            library_directory: library_directory.display().to_string(),
            natives_directory: natives_directory.display().to_string(),
            launcher_name: "Cubic Launcher".to_string(),
            launcher_version: env!("CARGO_PKG_VERSION").to_string(),
            classpath_separator: if cfg!(target_os = "windows") {
                ";".to_string()
            } else {
                ":".to_string()
            },
            resolution_width: "854".to_string(),
            resolution_height: "480".to_string(),
        }
    }
}

fn default_verification_timeout_seconds() -> u64 {
    45
}

fn default_success_after_seconds() -> u64 {
    15
}

fn default_terminate_on_success() -> bool {
    true
}

fn default_terminate_on_timeout() -> bool {
    true
}

fn automation_cache_only_override() -> Option<bool> {
    // S2: only honor CUBIC_AUTOMATION_CACHE_ONLY_MODE when the automation
    // entry-point is active. This prevents an inherited env value from silently
    // overriding the user's cache_only_mode in a normal production launch.
    if std::env::var("CUBIC_AUTOMATION_VERIFY_REQUEST")
        .ok()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        return None;
    }
    let value = std::env::var("CUBIC_AUTOMATION_CACHE_ONLY_MODE").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    }
}
