use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tauri::Emitter;

use crate::instance_mods::CachedModJar;
use crate::launch_command::FlightRecording;
use crate::launcher_paths::LauncherPaths;
use crate::mod_cache::{ModAcquisitionPlan, ModCacheRecord};
use crate::modrinth::ModrinthVersion;
use crate::process_streaming::{
    ProcessEventSink, ProcessExitEvent, ProcessLogEvent, ProcessLogStream, TauriProcessEventSink,
    MINECRAFT_LOG_EVENT,
};
use crate::resolver::ResolutionTarget;

use super::{EffectiveLaunchSettings, LaunchProgressEvent, LauncherErrorEvent, SelectedMod};

const LAUNCHER_ERROR_EVENT: &str = "launcher-error";

static ACTIVE_LAUNCH_LOG_SESSION: Mutex<Option<Arc<LaunchLogSession>>> = Mutex::new(None);

/// Il file JFR che l'ultimo lancio ha chiesto alla JVM (D109). Si scrive prima dello spawn e
/// l'uscita del gioco non lo azzera: chi lo chiede dopo `minecraft-exit` non corre contro
/// `set_active_launch_log_session(None)`, che invece azzera la sessione proprio lì.
static LAST_PROFILE_RECORDING: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Monotonic counter guaranteeing distinct launch log directories even for two
/// launches of the same (modlist, mc version, loader) triple within one second.
static LAUNCH_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(super) struct LaunchLogSession {
    dir: PathBuf,
}

#[derive(Clone)]
pub(super) struct LoggingProcessEventSink {
    inner: TauriProcessEventSink,
    session: Arc<LaunchLogSession>,
    ui_throttle: Arc<Mutex<LogUiThrottleState>>,
}

impl LaunchLogSession {
    pub(super) fn create(
        launcher_paths: &LauncherPaths,
        modlist_name: &str,
        target: &ResolutionTarget,
    ) -> Result<Arc<Self>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let sequence = LAUNCH_LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let launch_id = format!(
            "{}-{}-{}-{}-{}-{}",
            now.as_secs(),
            sanitize_log_path_component(modlist_name),
            sanitize_log_path_component(&target.minecraft_version),
            sanitize_log_path_component(target.mod_loader.as_modrinth_loader()),
            now.subsec_nanos(),
            sequence,
        );
        let dir = launcher_paths.launch_logs_dir().join(launch_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create launch log directory {}", dir.display()))?;

        let session = Arc::new(Self { dir });
        session.append_summary_line(&format!("modlist={modlist_name}"))?;
        session.append_summary_line(&format!("minecraft_version={}", target.minecraft_version))?;
        session.append_summary_line(&format!(
            "mod_loader={}",
            target.mod_loader.as_modrinth_loader()
        ))?;
        session.append_summary_line(&format!("log_dir={}", session.dir.display()))?;
        Ok(session)
    }

    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    fn append_launcher_line(&self, stream: ProcessLogStream, line: &str) -> Result<()> {
        let stream_label = match stream {
            ProcessLogStream::Stdout => "stdout",
            ProcessLogStream::Stderr => "stderr",
        };
        self.append_line("all.log", &format!("[launcher:{stream_label}] {line}"))?;
        self.append_line("launcher.log", line)?;

        if let Some(category_file) = launcher_category_file(line) {
            self.append_line(category_file, line)?;
        }

        Ok(())
    }

    fn append_minecraft_log(&self, event: &ProcessLogEvent) -> Result<()> {
        let stream_label = match event.stream {
            ProcessLogStream::Stdout => "stdout",
            ProcessLogStream::Stderr => "stderr",
        };
        self.append_line(
            "all.log",
            &format!("[minecraft:{stream_label}] {}", event.line),
        )?;
        self.append_line("minecraft.log", &format!("[{stream_label}] {}", event.line))
    }

    fn append_exit_event(&self, event: &ProcessExitEvent) -> Result<()> {
        self.append_summary_line(&format!("minecraft_exit_success={}", event.success))?;
        self.append_summary_line(&format!(
            "minecraft_exit_code={}",
            event
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string())
        ))?;
        self.append_line(
            "all.log",
            &format!(
                "[minecraft:exit] success={} exit_code={}",
                event.success,
                event
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".to_string())
            ),
        )
    }

    pub(super) fn write_selected_mods(&self, selected_mods: &[SelectedMod]) -> Result<()> {
        let mut lines = Vec::with_capacity(selected_mods.len() + 1);
        lines.push(format!("count={}", selected_mods.len()));
        for selected in selected_mods {
            lines.push(format!(
                "{} | source={}",
                selected.mod_id,
                selected.source_label()
            ));
        }
        self.write_file("selected_mods.log", &lines)
    }

    /// `restored_records` are cache rows whose jar is gone and that the launch
    /// re-downloads at that same version: they belong to the resolved set, and
    /// the marker is what tells them apart from a jar that was already there.
    pub(super) fn write_resolved_versions(
        &self,
        remote_versions: &[ModrinthVersion],
        cached_records: &[ModCacheRecord],
        restored_records: &[ModCacheRecord],
    ) -> Result<()> {
        let mut lines = vec![
            format!("live_versions={}", remote_versions.len()),
            format!(
                "cached_records={}",
                cached_records.len() + restored_records.len()
            ),
            String::new(),
            "[live_versions]".to_string(),
        ];

        for version in remote_versions {
            let primary_file = version.primary_file();
            lines.push(format!(
                "{} | version_id={} | version_number={} | file={}",
                version.project_id,
                version.id,
                version.version_number,
                primary_file
                    .map(|file| file.filename.as_str())
                    .unwrap_or("<missing>")
            ));
        }

        lines.push(String::new());
        lines.push("[cached_records]".to_string());
        for (record, restored) in cached_records
            .iter()
            .map(|record| (record, false))
            .chain(restored_records.iter().map(|record| (record, true)))
        {
            lines.push(format!(
                "{} | version_id={} | jar={} | local={}{}",
                record.modrinth_project_id,
                record.modrinth_version_id,
                record.jar_filename,
                record.is_local,
                if restored { " | restored=true" } else { "" }
            ));
        }

        self.write_file("resolved_versions.log", &lines)
    }

    pub(super) fn write_cache_plan(&self, plan: &ModAcquisitionPlan) -> Result<()> {
        let mut lines = vec![
            format!("cached={}", plan.cached.len()),
            format!("to_download={}", plan.to_download.len()),
            String::new(),
            "[cached]".to_string(),
        ];

        for record in &plan.cached {
            lines.push(format!(
                "{} | version_id={} | jar={}",
                record.modrinth_project_id, record.modrinth_version_id, record.jar_filename
            ));
        }

        lines.push(String::new());
        lines.push("[to_download]".to_string());
        for pending in &plan.to_download {
            lines.push(format!(
                "{} | version_id={} | jar={}",
                pending.modrinth_project_id, pending.modrinth_version_id, pending.jar_filename
            ));
        }

        self.write_file("cache_plan.log", &lines)
    }

    pub(super) fn write_final_mod_set(&self, jars: &[CachedModJar]) -> Result<()> {
        let mut lines = vec![format!("count={}", jars.len())];
        for jar in jars {
            lines.push(jar.jar_filename.clone());
        }
        self.write_file("final_mod_set.log", &lines)
    }

    pub(super) fn append_summary_line(&self, line: &str) -> Result<()> {
        self.append_line("summary.log", line)
    }

    pub(super) fn append_issue_line(
        &self,
        title: &str,
        message: &str,
        detail: &str,
        severity: &str,
    ) -> Result<()> {
        self.append_line(
            "issues.log",
            &format!("[{severity}] {title} | {message} | {detail}"),
        )
    }

    fn append_line(&self, file_name: &str, line: &str) -> Result<()> {
        let path = self.dir.join(file_name);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        writeln!(file, "{line}").with_context(|| format!("failed to write {}", path.display()))
    }

    fn write_file(&self, file_name: &str, lines: &[String]) -> Result<()> {
        let path = self.dir.join(file_name);
        let mut content = lines.join("\n");
        content.push('\n');
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

impl LoggingProcessEventSink {
    pub(super) fn new(app_handle: tauri::AppHandle, session: Arc<LaunchLogSession>) -> Self {
        Self {
            inner: TauriProcessEventSink::new(app_handle),
            session,
            ui_throttle: Arc::new(Mutex::new(LogUiThrottleState::default())),
        }
    }
}

impl ProcessEventSink for LoggingProcessEventSink {
    fn emit_log(&self, event: ProcessLogEvent) -> Result<()> {
        let _ = self.session.append_minecraft_log(&event);
        let events = self.ui_throttle.lock().unwrap().events_for_incoming(event);
        for event in events {
            self.inner.emit_log(event)?;
        }
        Ok(())
    }

    fn emit_exit(&self, event: ProcessExitEvent) -> Result<()> {
        let events = self.ui_throttle.lock().unwrap().flush();
        for event in events {
            self.inner.emit_log(event)?;
        }
        let _ = self.session.append_exit_event(&event);
        self.inner.emit_exit(event)
    }
}

#[derive(Default)]
pub(super) struct LogUiThrottleState {
    last_emitted_line: Option<ProcessLogEvent>,
    repeated_line_count: usize,
    suppressed_forge_patch_count: usize,
}

impl LogUiThrottleState {
    pub(super) fn events_for_incoming(&mut self, event: ProcessLogEvent) -> Vec<ProcessLogEvent> {
        let mut events = Vec::new();

        if is_forge_installer_patch_line(&event.line) {
            self.flush_repeated_line(&mut events);
            self.suppressed_forge_patch_count += 1;
            if self.suppressed_forge_patch_count % 1000 == 0 {
                events.push(ProcessLogEvent {
                    stream: ProcessLogStream::Stdout,
                    line: format!(
                        "[Launch] Suppressed {} Forge installer patch progress lines in the UI; full output is still written to launch logs.",
                        self.suppressed_forge_patch_count
                    ),
                });
                self.suppressed_forge_patch_count = 0;
            }
            return events;
        }

        self.flush_forge_patch_lines(&mut events);

        if self
            .last_emitted_line
            .as_ref()
            .map(|last| last.stream == event.stream && last.line == event.line)
            .unwrap_or(false)
        {
            self.repeated_line_count += 1;
            return events;
        }

        self.flush_repeated_line(&mut events);
        self.last_emitted_line = Some(event.clone());
        events.push(event);
        events
    }

    pub(super) fn flush(&mut self) -> Vec<ProcessLogEvent> {
        let mut events = Vec::new();
        self.flush_forge_patch_lines(&mut events);
        self.flush_repeated_line(&mut events);
        events
    }

    fn flush_forge_patch_lines(&mut self, events: &mut Vec<ProcessLogEvent>) {
        if self.suppressed_forge_patch_count == 0 {
            return;
        }

        events.push(ProcessLogEvent {
            stream: ProcessLogStream::Stdout,
            line: format!(
                "[Launch] Suppressed {} Forge installer patch progress lines in the UI; full output is still written to launch logs.",
                self.suppressed_forge_patch_count
            ),
        });
        self.suppressed_forge_patch_count = 0;
    }

    fn flush_repeated_line(&mut self, events: &mut Vec<ProcessLogEvent>) {
        if self.repeated_line_count == 0 {
            return;
        }

        let Some(last) = self.last_emitted_line.as_ref() else {
            self.repeated_line_count = 0;
            return;
        };

        events.push(ProcessLogEvent {
            stream: last.stream,
            line: format!(
                "[Launch] Previous launch log line repeated {} additional time{}.",
                self.repeated_line_count,
                if self.repeated_line_count == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        });
        self.repeated_line_count = 0;
    }
}

fn is_forge_installer_patch_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Patching net/minecraft/") && trimmed.ends_with(" 1/1")
}

pub(super) fn set_active_launch_log_session(session: Option<Arc<LaunchLogSession>>) {
    *ACTIVE_LAUNCH_LOG_SESSION.lock().unwrap() = session;
}

pub(super) fn current_launch_log_session() -> Option<Arc<LaunchLogSession>> {
    ACTIVE_LAUNCH_LOG_SESSION.lock().unwrap().clone()
}

/// Decide il profiler del lancio, lo scrive nel log del lancio e mette da parte il file per
/// il bottone di fine lancio. Ogni lancio sovrascrive il valore del precedente.
pub(super) fn launch_profiler(
    app_handle: &tauri::AppHandle,
    settings: &EffectiveLaunchSettings,
    java_major: u32,
    launch_log_dir: &Path,
) -> Result<Option<FlightRecording>> {
    let decision = settings.profiler_decision(java_major, launch_log_dir);
    if let Some(line) = decision.launch_log_line() {
        emit_log(app_handle, ProcessLogStream::Stdout, line)?;
    }
    let recording = decision.into_recording();
    remember_profile_recording(
        recording
            .as_ref()
            .map(|recording| recording.output_file.clone()),
    );
    Ok(recording)
}

fn remember_profile_recording(output_file: Option<PathBuf>) {
    *LAST_PROFILE_RECORDING.lock() = output_file;
}

/// Il file JFR dell'ultimo lancio, solo se esiste davvero: una JVM uccisa senza chiudersi
/// può non averlo scritto, e allora il bottone non deve comparire.
pub(super) fn last_profile_recording_on_disk() -> Option<PathBuf> {
    LAST_PROFILE_RECORDING
        .lock()
        .clone()
        .filter(|output_file| output_file.is_file())
}

pub(super) fn emit_progress(
    app_handle: &tauri::AppHandle,
    state: &str,
    progress: u8,
    stage: &str,
    detail: &str,
) -> Result<()> {
    app_handle
        .emit(
            super::LAUNCH_PROGRESS_EVENT,
            LaunchProgressEvent {
                state: state.to_string(),
                progress,
                stage: stage.to_string(),
                detail: detail.to_string(),
            },
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

pub(super) fn emit_log(
    app_handle: &tauri::AppHandle,
    stream: ProcessLogStream,
    line: String,
) -> Result<()> {
    if let Some(session) = current_launch_log_session() {
        let _ = session.append_launcher_line(stream, &line);
    }
    app_handle
        .emit(MINECRAFT_LOG_EVENT, ProcessLogEvent { stream, line })
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

pub(super) fn emit_launcher_issue(
    app_handle: &tauri::AppHandle,
    title: &str,
    message: &str,
    detail: &str,
    severity: &str,
    scope: &str,
) -> Result<()> {
    if let Some(session) = current_launch_log_session() {
        let _ = session.append_issue_line(title, message, detail, severity);
    }
    app_handle
        .emit(
            LAUNCHER_ERROR_EVENT,
            LauncherErrorEvent {
                id: unique_error_id(),
                title: title.to_string(),
                message: message.to_string(),
                detail: detail.to_string(),
                severity: severity.to_string(),
                scope: scope.to_string(),
            },
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn sanitize_log_path_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => character,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase();

    if sanitized.is_empty() {
        "launch".to_string()
    } else {
        sanitized
    }
}

fn launcher_category_file(line: &str) -> Option<&'static str> {
    if line.starts_with("[Resolver]") {
        Some("resolver.log")
    } else if line.starts_with("[Cache]") {
        Some("cache.log")
    } else {
        None
    }
}

fn unique_error_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("launch-error-{timestamp}")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{LaunchLogSession, LogUiThrottleState, ProcessLogEvent, ProcessLogStream};
    use crate::launcher_paths::LauncherPaths;
    use crate::resolver::{ModLoader, ResolutionTarget};

    fn unique_test_root() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cubic-launch-log-test-{nanos}"))
    }

    #[test]
    fn create_disambiguates_same_triple_within_same_second() {
        let root = unique_test_root();
        let launcher_paths = LauncherPaths::new(&root);
        launcher_paths
            .create_required_directories()
            .expect("dirs should create");
        let target = ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        };

        // Two launches of the identical (modlist, mc version, loader) triple
        // back-to-back (same wall-clock second) must not share a log dir.
        let first = LaunchLogSession::create(&launcher_paths, "My Pack", &target)
            .expect("first session should create");
        let second = LaunchLogSession::create(&launcher_paths, "My Pack", &target)
            .expect("second session should create");

        assert_ne!(
            first.dir(),
            second.dir(),
            "same-second launches of the same triple must get distinct log dirs"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    fn stdout(line: &str) -> ProcessLogEvent {
        ProcessLogEvent {
            stream: ProcessLogStream::Stdout,
            line: line.to_string(),
        }
    }

    #[test]
    fn ui_throttle_summarizes_forge_patch_lines() {
        let mut throttle = LogUiThrottleState::default();

        assert!(throttle
            .events_for_incoming(stdout("  Patching net/minecraft/client/Minecraft 1/1"))
            .is_empty());
        assert!(throttle
            .events_for_incoming(stdout("  Patching net/minecraft/world/World 1/1"))
            .is_empty());

        let events = throttle.events_for_incoming(stdout("[22:19:36] [main/INFO] Done"));
        assert_eq!(events.len(), 2);
        assert!(events[0]
            .line
            .contains("Suppressed 2 Forge installer patch"));
        assert_eq!(events[1].line, "[22:19:36] [main/INFO] Done");
    }

    #[test]
    fn ui_throttle_summarizes_consecutive_repeated_lines() {
        let mut throttle = LogUiThrottleState::default();

        assert_eq!(
            throttle.events_for_incoming(stdout("%tEx}")).len(),
            1,
            "first occurrence should be emitted"
        );
        assert!(throttle.events_for_incoming(stdout("%tEx}")).is_empty());
        assert!(throttle.events_for_incoming(stdout("%tEx}")).is_empty());

        let events = throttle.events_for_incoming(stdout("next"));
        assert_eq!(events.len(), 2);
        assert!(events[0]
            .line
            .contains("Previous launch log line repeated 2 additional times"));
        assert_eq!(events[1].line, "next");
    }
}
