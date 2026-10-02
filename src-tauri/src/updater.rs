use serde::Serialize;
use tauri_plugin_updater::UpdaterExt;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum Status {
    UpToDate,
    Available,
    Error,
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
    }
}

#[tauri::command]
pub async fn check_for_updates(app: tauri::AppHandle) -> UpdateCheckPayload {
    let current_version = app.package_info().version.to_string();
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

#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
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
    use super::{payload_from_outcome, CheckOutcome, Status};

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
