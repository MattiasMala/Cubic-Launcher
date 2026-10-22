use serde::Serialize;
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

use crate::launcher_paths::LauncherPaths;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum Status {
    UpToDate,
    Available,
    Error,
    /// Not checked: the active account plays offline (D140).
    Offline,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckPayload {
    status: Status,
    current_version: String,
    version: Option<String>,
    notes: Option<String>,
}

enum CheckOutcome {
    UpToDate,
    Available {
        version: String,
        notes: Option<String>,
    },
    Failed,
    SkippedOffline,
}

fn payload_from_outcome(current_version: &str, outcome: CheckOutcome) -> UpdateCheckPayload {
    let current_version = current_version.to_owned();

    match outcome {
        CheckOutcome::UpToDate => UpdateCheckPayload {
            status: Status::UpToDate,
            current_version,
            version: None,
            notes: None,
        },
        CheckOutcome::Available { version, notes } => UpdateCheckPayload {
            status: Status::Available,
            current_version,
            version: Some(version),
            notes,
        },
        CheckOutcome::Failed => UpdateCheckPayload {
            status: Status::Error,
            current_version,
            version: None,
            notes: None,
        },
        CheckOutcome::SkippedOffline => UpdateCheckPayload {
            status: Status::Offline,
            current_version,
            version: None,
            notes: None,
        },
    }
}

/// D140: with the active account playing offline, GitHub is not asked. A
/// database that can't be read does not stop the check.
fn active_account_plays_offline(app: &tauri::AppHandle) -> bool {
    app.try_state::<LauncherPaths>().is_some_and(|paths| {
        crate::app_shell::active_account_plays_offline(&paths).unwrap_or(false)
    })
}

#[tauri::command]
pub async fn check_for_updates(app: tauri::AppHandle) -> UpdateCheckPayload {
    let current_version = app.package_info().version.to_string();
    if active_account_plays_offline(&app) {
        return payload_from_outcome(&current_version, CheckOutcome::SkippedOffline);
    }
    let outcome = match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(update)) => CheckOutcome::Available {
                version: update.version,
                notes: update.body,
            },
            Ok(None) => CheckOutcome::UpToDate,
            Err(_) => CheckOutcome::Failed,
        },
        Err(_) => CheckOutcome::Failed,
    };

    payload_from_outcome(&current_version, outcome)
}

/// Il prefisso dell'errore con cui l'installazione si ferma col gioco aperto:
/// il frontend lo riconosce, chiede conferma e riprova con
/// `confirmed_while_running` (D152).
const MINECRAFT_RUNNING: &str = "minecraft-running:";

/// L'aggiornamento riavvia il launcher, e col gioco aperto il riavvio salta la
/// copia indietro dei file condivisi (`concorrenza-1`): si installa solo con la
/// conferma di chi ha visto l'avviso.
fn install_refusal(game_running: bool, confirmed_while_running: bool) -> Option<String> {
    (game_running && !confirmed_while_running).then(|| {
        format!(
            "{MINECRAFT_RUNNING} Minecraft is still running; installing the update restarts \
             Cubic Launcher"
        )
    })
}

#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    confirmed_while_running: Option<bool>,
) -> Result<(), String> {
    if active_account_plays_offline(&app) {
        return Err(
            "The active account plays offline, so the launcher doesn't download updates. Turn off Play offline in Manage Accounts to install it."
                .to_string(),
        );
    }
    if let Some(refusal) = install_refusal(
        crate::launch_preview::current_minecraft_pid().is_some(),
        confirmed_while_running.unwrap_or(false),
    ) {
        return Err(refusal);
    }

    let updater = app
        .updater()
        .map_err(|error| format!("Failed to initialize updater: {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("Failed to check for updates: {error}"))?
        .ok_or_else(|| "No update available".to_string())?;

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| format!("Failed to download and install update: {error}"))?;

    app.restart();
}

#[cfg(test)]
mod tests {
    use super::{install_refusal, payload_from_outcome, CheckOutcome, Status, MINECRAFT_RUNNING};

    #[test]
    fn an_update_waits_for_a_confirmation_while_the_game_runs() {
        let refusal = install_refusal(true, false).expect("a running game must stop the install");
        assert!(refusal.starts_with(MINECRAFT_RUNNING));
        assert_eq!(install_refusal(true, true), None);
        assert_eq!(install_refusal(false, false), None);
    }

    #[test]
    fn maps_up_to_date_outcome() {
        let payload = payload_from_outcome("0.1.0", CheckOutcome::UpToDate);

        assert!(matches!(payload.status, Status::UpToDate));
        assert_eq!(payload.current_version, "0.1.0");
        assert_eq!(payload.version, None);
        assert_eq!(payload.notes, None);
    }

    #[test]
    fn maps_available_outcome() {
        let payload = payload_from_outcome(
            "0.1.0",
            CheckOutcome::Available {
                version: "0.2.0".to_string(),
                notes: Some("Release notes".to_string()),
            },
        );

        assert!(matches!(payload.status, Status::Available));
        assert_eq!(payload.current_version, "0.1.0");
        assert_eq!(payload.version, Some("0.2.0".to_string()));
        assert_eq!(payload.notes, Some("Release notes".to_string()));
    }

    #[test]
    fn maps_failed_outcome_without_details() {
        let payload = payload_from_outcome("0.1.0", CheckOutcome::Failed);

        assert!(matches!(payload.status, Status::Error));
        assert_eq!(payload.current_version, "0.1.0");
        assert_eq!(payload.version, None);
        assert_eq!(payload.notes, None);
    }

    #[test]
    fn maps_skipped_offline_outcome() {
        let payload = payload_from_outcome("0.1.0", CheckOutcome::SkippedOffline);
        let serialized = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(
            serialized,
            serde_json::json!({
                "status": "offline",
                "currentVersion": "0.1.0",
                "version": null,
                "notes": null,
            })
        );
    }

    #[test]
    fn serializes_exact_payload_contract() {
        let payload = payload_from_outcome("0.1.0", CheckOutcome::UpToDate);
        let serialized = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(
            serialized,
            serde_json::json!({
                "status": "upToDate",
                "currentVersion": "0.1.0",
                "version": null,
                "notes": null,
            })
        );
    }
}
