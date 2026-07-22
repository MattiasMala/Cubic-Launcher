use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::launcher_paths::LauncherPaths;

use super::{
    current_launch_log_session, current_minecraft_pid, emit_launch_failure, run_launch_pipeline,
    set_active_launch_log_session, terminate_minecraft_pid, LaunchLogSession,
    LaunchVerificationRequest, LaunchVerificationResult,
};

pub fn automation_mode_enabled() -> bool {
    std::env::var("CUBIC_AUTOMATION_VERIFY_REQUEST")
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

pub fn maybe_start_automation_verifier(
    app_handle: tauri::AppHandle,
    launcher_paths: LauncherPaths,
) -> Result<()> {
    let request_json = match std::env::var("CUBIC_AUTOMATION_VERIFY_REQUEST") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(()),
    };

    let request: LaunchVerificationRequest =
        serde_json::from_str(&request_json).context("failed to parse automation request JSON")?;
    let output_path = std::env::var("CUBIC_AUTOMATION_VERIFY_OUTPUT")
        .ok()
        .map(PathBuf::from);
    let should_exit = std::env::var("CUBIC_AUTOMATION_VERIFY_EXIT")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false);
    append_automation_trace(
        &launcher_paths,
        &format!(
            "automation bootstrap requested: output={} exit_on_failure={should_exit}",
            output_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ),
    );

    tauri::async_runtime::spawn(async move {
        append_automation_trace(&launcher_paths, "automation task started");
        // Route through the SAME verification path as verify_launch_command, so
        // the env entry-point observes the launch (timeout/successAfter/terminate*)
        // and emits a full LaunchVerificationResult with the 5 states.
        let verification =
            run_launch_verification(app_handle.clone(), launcher_paths.clone(), request).await;

        let output_result = match &verification {
            Ok(result) => {
                append_automation_trace(
                    &launcher_paths,
                    &format!(
                        "verification finished: state={} success={} pid={:?} log_dir={:?}",
                        result.state, result.success, result.pid, result.launch_log_dir
                    ),
                );
                serde_json::to_string_pretty(result)
                    .context("failed to serialize automation verification result")
            }
            Err(error) => {
                let detail = format!("{error:#}");
                let _ = emit_launch_failure(&app_handle, &detail);
                append_automation_trace(
                    &launcher_paths,
                    &format!("verification failed to run: {detail}"),
                );
                serde_json::to_string_pretty(&serde_json::json!({
                    "started": false,
                    "success": false,
                    "state": "automation_error",
                    "failureKind": "automation_error",
                    "failureSummary": detail,
                    "launchLogDir": current_launch_log_session().map(|session| session.dir().display().to_string()),
                }))
                .context("failed to serialize automation verification error")
            }
        };

        if let Some(path) = output_path {
            if let Err(error) = write_automation_output_json(&path, output_result) {
                append_automation_trace(
                    &launcher_paths,
                    &format!(
                        "failed to write automation output {}: {error:#}",
                        path.display()
                    ),
                );
                eprintln!(
                    "failed to write automation verification output {}: {error:#}",
                    path.display()
                );
            } else {
                append_automation_trace(
                    &launcher_paths,
                    &format!("wrote automation output {}", path.display()),
                );
            }
        }

        // run_launch_verification already terminates Minecraft per terminate_on_*;
        // clear the tracked session so nothing leaks before we exit.
        set_active_launch_log_session(None);

        if let Some(code) = automation_exit_code(should_exit, verification.as_ref().ok()) {
            append_automation_trace(
                &launcher_paths,
                &format!("automation exit requested with code {code}"),
            );
            app_handle.exit(code);
        }
    });

    Ok(())
}

/// Decide the process exit code for the automation entry-point.
///
/// Returns `None` when the process should keep running (exit not requested).
/// When exit IS requested, the process always exits once verification concludes
/// — code 0 when the launch verified as running/success, code 1 otherwise
/// (any non-success state, or a verification error). Minecraft itself is already
/// terminated by `run_launch_verification` per `terminate_on_*`.
pub(super) fn automation_exit_code(
    should_exit: bool,
    verification: Option<&LaunchVerificationResult>,
) -> Option<i32> {
    if !should_exit {
        return None;
    }
    match verification {
        Some(result) if result.success => Some(0),
        _ => Some(1),
    }
}

pub(super) async fn run_launch_verification(
    app_handle: tauri::AppHandle,
    launcher_paths: LauncherPaths,
    request: LaunchVerificationRequest,
) -> Result<LaunchVerificationResult> {
    anyhow::ensure!(
        current_minecraft_pid().is_none(),
        "cannot verify launch while another Minecraft process is already tracked"
    );

    let started_at = Instant::now();
    let timeout = Duration::from_secs(request.timeout_seconds.max(5));
    let success_after = Duration::from_secs(request.success_after_seconds.max(1));
    let launch_request = request.clone().into_launch_request();

    let started_launch =
        match run_launch_pipeline(app_handle.clone(), launcher_paths, launch_request).await {
            Ok(started_launch) => started_launch,
            Err(error) => {
                let detail = format!("{error:#}");
                let _ = emit_launch_failure(&app_handle, &detail);
                let result = build_failed_verification_result(
                    started_at.elapsed(),
                    current_launch_log_session(),
                    None,
                    "launch_failed",
                    "launch_failed",
                    &detail,
                )?;
                set_active_launch_log_session(None);
                return Ok(result);
            }
        };

    loop {
        let elapsed = started_at.elapsed();
        let log_tail = read_log_tail(&started_launch.launch_log_dir.join("minecraft.log"), 80)?;
        let summary_exit = summary_reports_minecraft_exit(&started_launch.launch_log_dir)?;

        if let Some((failure_kind, failure_summary)) = summarize_launch_failure(&log_tail) {
            if current_minecraft_pid() == Some(started_launch.pid) {
                let _ = terminate_minecraft_pid(started_launch.pid);
            }
            let result = finalize_verification_result(LaunchVerificationResult {
                started: true,
                success: false,
                state: "crashed".to_string(),
                pid: Some(started_launch.pid),
                launch_log_dir: Some(started_launch.launch_log_dir.display().to_string()),
                duration_ms: elapsed.as_millis() as u64,
                failure_kind: Some(failure_kind),
                failure_summary: Some(failure_summary),
                minecraft_log_tail: log_tail,
            })?;
            return Ok(result);
        }

        if current_minecraft_pid() != Some(started_launch.pid) || summary_exit.is_some() {
            let failure_summary = if let Some((_, exit_code)) = summary_exit {
                format!(
                    "Minecraft exited before the verification success window was reached{}.",
                    exit_code
                        .map(|code| format!(" (exit code {code})"))
                        .unwrap_or_default()
                )
            } else {
                "Minecraft exited before the verification success window was reached.".to_string()
            };
            let result = finalize_verification_result(LaunchVerificationResult {
                started: true,
                success: false,
                state: "exited".to_string(),
                pid: Some(started_launch.pid),
                launch_log_dir: Some(started_launch.launch_log_dir.display().to_string()),
                duration_ms: elapsed.as_millis() as u64,
                failure_kind: Some("process_exited".to_string()),
                failure_summary: Some(failure_summary),
                minecraft_log_tail: log_tail,
            })?;
            return Ok(result);
        }

        if elapsed >= success_after && launch_appears_healthy(&log_tail) {
            if request.terminate_on_success {
                let _ = terminate_minecraft_pid(started_launch.pid);
            }
            let result = finalize_verification_result(LaunchVerificationResult {
                started: true,
                success: true,
                state: "running".to_string(),
                pid: Some(started_launch.pid),
                launch_log_dir: Some(started_launch.launch_log_dir.display().to_string()),
                duration_ms: elapsed.as_millis() as u64,
                failure_kind: None,
                failure_summary: None,
                minecraft_log_tail: log_tail,
            })?;
            return Ok(result);
        }

        if elapsed >= timeout {
            if request.terminate_on_timeout && current_minecraft_pid() == Some(started_launch.pid) {
                let _ = terminate_minecraft_pid(started_launch.pid);
            }
            let result = finalize_verification_result(LaunchVerificationResult {
                started: true,
                success: false,
                state: "timed_out".to_string(),
                pid: Some(started_launch.pid),
                launch_log_dir: Some(started_launch.launch_log_dir.display().to_string()),
                duration_ms: elapsed.as_millis() as u64,
                failure_kind: Some("timed_out".to_string()),
                failure_summary: Some(format!(
                    "Minecraft did not reach the verification success window within {} seconds.",
                    timeout.as_secs()
                )),
                minecraft_log_tail: log_tail,
            })?;
            return Ok(result);
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn append_automation_trace(launcher_paths: &LauncherPaths, message: &str) {
    let path = launcher_paths.logs_dir().join("automation.log");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!("[{timestamp}] {message}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

pub(super) fn read_log_tail(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = content.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.len() > max_lines {
        let split_point = lines.len() - max_lines;
        lines.drain(0..split_point);
    }
    Ok(lines)
}

pub(super) fn summary_reports_minecraft_exit(
    log_dir: &Path,
) -> Result<Option<(bool, Option<String>)>> {
    let lines = read_log_tail(&log_dir.join("summary.log"), 30)?;
    let mut exit_success = None;
    let mut exit_code = None;

    for line in lines {
        if let Some(value) = line.strip_prefix("minecraft_exit_success=") {
            exit_success = Some(value.trim() == "true");
        } else if let Some(value) = line.strip_prefix("minecraft_exit_code=") {
            let trimmed = value.trim();
            if trimmed != "none" {
                exit_code = Some(trimmed.to_string());
            }
        }
    }

    Ok(exit_success.map(|success| (success, exit_code)))
}

pub(super) fn summarize_launch_failure(lines: &[String]) -> Option<(String, String)> {
    let joined = lines.join("\n");
    let lowercase = joined.to_ascii_lowercase();

    if lowercase.contains("requires version")
        && lowercase.contains("which is missing")
        && lowercase.contains("mod '")
    {
        return Some((
            "missing_dependency".to_string(),
            "Fabric reported a missing required dependency in the selected mod set.".to_string(),
        ));
    }

    if lowercase.contains("requires version")
        && lowercase.contains("java")
        && lowercase.contains("wrong version is present")
    {
        return Some((
            "wrong_java".to_string(),
            "A selected mod requires a newer Java runtime than the launcher used.".to_string(),
        ));
    }

    if lowercase.contains("noclassdeffounderror")
        || (lowercase.contains("classnotfoundexception")
            && lowercase.contains("caused by:")
            && !lowercase.contains("error loading class:"))
    {
        return Some((
            "missing_class".to_string(),
            "Minecraft crashed because a required class was missing at runtime.".to_string(),
        ));
    }

    if lowercase.contains("nosuchmethoderror") {
        return Some((
            "method_mismatch".to_string(),
            "Minecraft crashed because two mods loaded incompatible method signatures.".to_string(),
        ));
    }

    if lowercase.contains("could not execute entrypoint stage")
        || lowercase.contains("exception caught from launcher")
    {
        return Some((
            "entrypoint_crash".to_string(),
            "Minecraft crashed while loading a mod entrypoint.".to_string(),
        ));
    }

    if lowercase.contains("incompatible mods found") {
        return Some((
            "incompatible_mods".to_string(),
            "Fabric rejected the selected mod set as incompatible.".to_string(),
        ));
    }

    None
}

pub(super) fn launch_appears_healthy(lines: &[String]) -> bool {
    if lines.is_empty() || summarize_launch_failure(lines).is_some() {
        return false;
    }

    lines.iter().any(|line| {
        line.contains("[Render thread/")
            || line.contains("Loading Minecraft")
            || line.contains("OpenAL initialized")
            || line.contains("Reloading ResourceManager")
    })
}

pub(super) fn finalize_verification_result(
    result: LaunchVerificationResult,
) -> Result<LaunchVerificationResult> {
    if let Some(log_dir) = &result.launch_log_dir {
        let path = PathBuf::from(log_dir);
        write_verification_result(&path, &result)?;
        if let Some(session) = current_launch_log_session() {
            let _ =
                session.append_summary_line(&format!("verification_success={}", result.success));
            let _ = session.append_summary_line(&format!("verification_state={}", result.state));
            if let Some(failure_kind) = &result.failure_kind {
                let _ = session
                    .append_summary_line(&format!("verification_failure_kind={failure_kind}"));
            }
            if let Some(failure_summary) = &result.failure_summary {
                let _ = session.append_summary_line(&format!(
                    "verification_failure_summary={failure_summary}"
                ));
            }
        }
    }

    Ok(result)
}

pub(super) fn build_failed_verification_result(
    elapsed: Duration,
    session: Option<Arc<LaunchLogSession>>,
    pid: Option<u32>,
    state: &str,
    failure_kind: &str,
    failure_summary: &str,
) -> Result<LaunchVerificationResult> {
    let launch_log_dir = session
        .as_ref()
        .map(|entry| entry.dir().display().to_string());
    let minecraft_log_tail = if let Some(entry) = &session {
        read_log_tail(&entry.dir().join("minecraft.log"), 80)?
    } else {
        Vec::new()
    };
    let result = LaunchVerificationResult {
        started: false,
        success: false,
        state: state.to_string(),
        pid,
        launch_log_dir,
        duration_ms: elapsed.as_millis() as u64,
        failure_kind: Some(failure_kind.to_string()),
        failure_summary: Some(failure_summary.to_string()),
        minecraft_log_tail,
    };
    finalize_verification_result(result)
}

pub(super) fn write_automation_output_json(path: &Path, json: Result<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let json = match json {
        Ok(value) => value,
        Err(error) => serde_json::to_string_pretty(&serde_json::json!({
            "started": false,
            "success": false,
            "state": "automation_error",
            "failureKind": "automation_error",
            "failureSummary": format!("{error:#}")
        }))
        .context("failed to serialize automation verification failure")?,
    };

    std::fs::write(path, json)
        .with_context(|| format!("failed to write automation result to {}", path.display()))
}

fn write_verification_result(log_dir: &Path, result: &LaunchVerificationResult) -> Result<()> {
    let json =
        serde_json::to_string_pretty(result).context("failed to serialize verification result")?;
    std::fs::write(log_dir.join("verification.json"), json).with_context(|| {
        format!(
            "failed to write {}",
            log_dir.join("verification.json").display()
        )
    })
}
