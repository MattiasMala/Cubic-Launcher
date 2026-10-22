use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::launch_command::PreparedLaunchCommand;

pub const MINECRAFT_LOG_EVENT: &str = "minecraft-log";
pub const MINECRAFT_EXIT_EVENT: &str = "minecraft-exit";

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessLogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessLogEvent {
    pub stream: ProcessLogStream,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExitEvent {
    pub success: bool,
    pub exit_code: Option<i32>,
}

pub trait ProcessEventSink: Send + Sync {
    fn emit_log(&self, event: ProcessLogEvent) -> Result<()>;
    fn emit_exit(&self, event: ProcessExitEvent) -> Result<()>;
}

#[derive(Clone)]
pub struct TauriProcessEventSink {
    app_handle: tauri::AppHandle,
}

impl TauriProcessEventSink {
    pub fn new(app_handle: tauri::AppHandle) -> Self {
        Self { app_handle }
    }
}

impl ProcessEventSink for TauriProcessEventSink {
    fn emit_log(&self, event: ProcessLogEvent) -> Result<()> {
        self.app_handle
            .emit(MINECRAFT_LOG_EVENT, event)
            .map_err(|error| anyhow!(error.to_string()))
    }

    fn emit_exit(&self, event: ProcessExitEvent) -> Result<()> {
        self.app_handle
            .emit(MINECRAFT_EXIT_EVENT, event)
            .map_err(|error| anyhow!(error.to_string()))
    }
}

pub struct ManagedProcess {
    pub pid: u32,
    exit_signal: ProcessExitSignal,
    monitor_thread: JoinHandle<Result<ProcessExitEvent>>,
}

impl ManagedProcess {
    pub fn wait(self) -> Result<ProcessExitEvent> {
        self.monitor_thread
            .join()
            .map_err(|_| anyhow!("process monitor thread panicked"))?
    }

    /// Chi ferma il processo lo usa per sapere quando è uscito davvero
    /// (windows-1), senza aspettare i lettori del suo output.
    pub fn exit_signal(&self) -> ProcessExitSignal {
        self.exit_signal.clone()
    }
}

/// «Il processo è uscito», segnato appena `wait` del figlio ritorna: prima dei
/// lettori dell'output, che possono restare aperti per un nipote che tiene la
/// pipe.
#[derive(Clone, Default)]
pub struct ProcessExitSignal(Arc<(Mutex<bool>, Condvar)>);

impl ProcessExitSignal {
    fn mark_exited(&self) {
        let (exited, changed) = &*self.0;
        *exited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        changed.notify_all();
    }

    pub fn has_exited(&self) -> bool {
        *self
            .0
             .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `true` se il processo esce entro `timeout`.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (exited, changed) = &*self.0;
        let guard = exited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (guard, _) = changed
            .wait_timeout_while(guard, timeout, |exited| !*exited)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard
    }
}

/// Quanto si aspetta, dopo l'uscita del processo, che i lettori finiscano di
/// svuotare stdout e stderr. Oltre, `minecraft-exit` parte lo stesso: un
/// nipote che tiene aperta la pipe non deve tenere il pannello su «running».
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub fn spawn_and_stream_process(
    command: PreparedLaunchCommand,
    event_sink: Arc<dyn ProcessEventSink>,
) -> Result<ManagedProcess> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(&command.current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn process '{}'", command.program.display()))?;

    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .context("spawned process is missing piped stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("spawned process is missing piped stderr")?;

    let stdout_thread =
        spawn_reader_thread(stdout, ProcessLogStream::Stdout, Arc::clone(&event_sink));
    let stderr_thread =
        spawn_reader_thread(stderr, ProcessLogStream::Stderr, Arc::clone(&event_sink));

    let exit_signal = ProcessExitSignal::default();
    let monitor_exit_signal = exit_signal.clone();
    let monitor_thread = thread::spawn(move || -> Result<ProcessExitEvent> {
        // concorrenza-10: `minecraft-exit` parte sempre, qualunque cosa sia
        // successa ai lettori o all'attesa. Prima un lettore fallito (una riga
        // non UTF-8) usciva con `?` prima di `emit_exit`, e il pannello restava
        // su «running» senza offrire il profilo JFR.
        let status = child.wait();
        monitor_exit_signal.mark_exited();

        let deadline = Instant::now() + READER_DRAIN_TIMEOUT;
        for (handle, name) in [(stdout_thread, "stdout"), (stderr_thread, "stderr")] {
            if let Err(error) = join_reader_thread(handle, name, deadline) {
                let _ = event_sink.emit_log(ProcessLogEvent {
                    stream: ProcessLogStream::Stderr,
                    line: format!("[Launch] Game output capture stopped early: {error:#}"),
                });
            }
        }

        let exit_event = match &status {
            Ok(status) => ProcessExitEvent {
                success: status.success(),
                exit_code: status.code(),
            },
            Err(_) => ProcessExitEvent {
                success: false,
                exit_code: None,
            },
        };
        let emitted = event_sink.emit_exit(exit_event.clone());

        status.context("failed while waiting for spawned process")?;
        emitted?;
        Ok(exit_event)
    });

    Ok(ManagedProcess {
        pid,
        exit_signal,
        monitor_thread,
    })
}

fn spawn_reader_thread<R: Read + Send + 'static>(
    stream: R,
    log_stream: ProcessLogStream,
    event_sink: Arc<dyn ProcessEventSink>,
) -> JoinHandle<Result<()>> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut decoder = Log4jEventDecoder::default();
        // Un evento che non si consegna non ferma la lettura: un lettore che
        // smette lascia la pipe piena, e il gioco si blocca alla prossima
        // scrittura.
        let emit = |line: String| {
            let _ = event_sink.emit_log(ProcessLogEvent {
                stream: log_stream,
                line,
            });
        };
        let mut read_result = Ok(());
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    for line in decoder.push(decode_output_line(&buffer)) {
                        emit(line);
                    }
                }
                Err(error) => {
                    read_result =
                        Err(anyhow::Error::new(error).context("failed to read process output"));
                    break;
                }
            }
        }
        for line in decoder.finish() {
            emit(line);
        }

        read_result
    })
}

/// Una riga dell'output del gioco come testo, senza il fine riga.
///
/// concorrenza-10: i byte, non `lines()`. La JVM scrive nella code page del
/// sistema (Cp1252 su un Windows italiano), e «perché», `C:\Users\Niccolò` o
/// «§» non sono UTF-8: con `lines()` la prima di quelle righe era un errore
/// che fermava il lettore, e il resto del log non arrivava più né al pannello
/// né ai file. I byte che non sono UTF-8 diventano «�», la riga resta.
pub fn decode_output_line(raw: &[u8]) -> String {
    let line = raw.strip_suffix(b"\n").unwrap_or(raw);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

/// Quanto aspetta STOP che il gioco si chiuda da sé, salvando il mondo, prima
/// di ucciderlo (windows-1). L'autosave di Minecraft e lo spegnimento del
/// server integrato stanno largamente dentro questo margine.
pub const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Dopo l'uccisione si aspetta ancora un poco che l'uscita arrivi al monitor.
const FORCED_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Era già uscito: non è stato mandato niente.
    AlreadyExited,
    /// Si è chiuso da sé dopo la richiesta gentile.
    Graceful,
    /// Non si è chiuso in tempo (o la richiesta gentile è fallita): ucciso.
    Forced,
}

/// Ferma il processo `pid` prima con garbo e solo dopo `grace` con la forza.
///
/// windows-1: su Windows `taskkill /F` è TerminateProcess, che non fa partire
/// gli shutdown hook, e il mondo perdeva gli ultimi minuti. Il passo gentile
/// è `taskkill /T /PID` senza `/F`, che manda WM_CLOSE alla finestra del
/// gioco; su Linux è SIGTERM. Se il gioco non esce entro `grace` si ripiega
/// su `taskkill /F /T` o SIGKILL.
pub fn stop_process(
    pid: u32,
    exit_signal: &ProcessExitSignal,
    grace: Duration,
) -> Result<StopOutcome> {
    if exit_signal.has_exited() {
        return Ok(StopOutcome::AlreadyExited);
    }

    let gentle = request_graceful_stop(pid);
    if gentle.is_ok() && exit_signal.wait_timeout(grace) {
        return Ok(StopOutcome::Graceful);
    }
    if exit_signal.has_exited() {
        return Ok(StopOutcome::Graceful);
    }

    force_stop(pid).with_context(|| match &gentle {
        Ok(()) => format!("process {pid} did not exit within {} s", grace.as_secs()),
        Err(error) => format!("process {pid} refused to close ({error:#})"),
    })?;
    exit_signal.wait_timeout(FORCED_STOP_TIMEOUT);
    Ok(StopOutcome::Forced)
}

#[cfg(target_os = "windows")]
fn request_graceful_stop(pid: u32) -> Result<()> {
    run_taskkill(&["/T", "/PID", &pid.to_string()])
}

#[cfg(target_os = "windows")]
fn force_stop(pid: u32) -> Result<()> {
    run_taskkill(&["/F", "/T", "/PID", &pid.to_string()])
}

#[cfg(target_os = "windows")]
fn run_taskkill(args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = Command::new("taskkill")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to run taskkill")?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(anyhow!(
            "taskkill {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "taskkill returned a non-zero exit code".to_string()
            } else {
                stderr
            }
        ))
    }
}

#[cfg(not(target_os = "windows"))]
fn request_graceful_stop(pid: u32) -> Result<()> {
    send_signal(pid, libc::SIGTERM)
}

#[cfg(not(target_os = "windows"))]
fn force_stop(pid: u32) -> Result<()> {
    send_signal(pid, libc::SIGKILL)
}

#[cfg(not(target_os = "windows"))]
fn send_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let pid = libc::pid_t::try_from(pid).context("process id out of range")?;
    // SAFETY: `kill` only reads its two integer arguments.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // Già uscito fra il controllo e il segnale: è quello che si voleva.
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(anyhow::Error::new(error).context(format!("failed to signal process {pid}")))
}

const LOG4J_EVENT_OPEN: &str = "<log4j:Event ";
const LOG4J_EVENT_CLOSE: &str = "</log4j:Event>";
/// Oltre questo un evento che non si chiude non è un evento: le righe tornano
/// com'erano, invece di restare in memoria per sempre.
const LOG4J_EVENT_MAX_LINES: usize = 10_000;

/// L'output del gioco riga per riga come lo scrive la `PatternLayout` di
/// Minecraft (`[HH:mm:ss] [thread/LEVEL]: messaggio`), anche quando la console
/// è in XML.
///
/// La configurazione di log4j del version.json (download-1) è quella del
/// launcher ufficiale, che legge la console come eventi XML
/// (`<log4j:Event …><log4j:Message><![CDATA[…]]></log4j:Message></log4j:Event>`,
/// su più righe). Senza questa decodifica il pannello dei log, i file del
/// lancio e il verificatore, che cerca `[Render thread/`, vedrebbero XML. Le
/// righe fuori da un evento passano come sono; un evento che non si decodifica
/// torna com'era.
#[derive(Debug, Default)]
pub struct Log4jEventDecoder {
    pending: Vec<String>,
}

impl Log4jEventDecoder {
    /// Le righe pronte dopo `line`: nessuna mentre un evento è aperto.
    pub fn push(&mut self, line: String) -> Vec<String> {
        if self.pending.is_empty() && !line.trim_start().starts_with(LOG4J_EVENT_OPEN) {
            return vec![line];
        }
        let closes = line.contains(LOG4J_EVENT_CLOSE);
        self.pending.push(line);
        if closes {
            let raw = std::mem::take(&mut self.pending);
            return decode_log4j_event(&raw.join("\n")).unwrap_or(raw);
        }
        if self.pending.len() >= LOG4J_EVENT_MAX_LINES {
            return std::mem::take(&mut self.pending);
        }
        Vec::new()
    }

    /// Quello che resta di un evento interrotto dalla fine dell'output.
    pub fn finish(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }
}

fn decode_log4j_event(event: &str) -> Option<Vec<String>> {
    let start = event.find(LOG4J_EVENT_OPEN)?;
    let header_end = start + event[start..].find('>')?;
    let header = &event[start + LOG4J_EVENT_OPEN.len()..header_end];
    let level = xml_attribute(header, "level")?;
    let thread = xml_attribute(header, "thread").unwrap_or_default();
    let time = xml_attribute(header, "timestamp")
        .and_then(|millis| millis.parse::<i64>().ok())
        .and_then(local_time_of_day);
    let body = &event[header_end + 1..];
    let message = xml_element_text(body, "log4j:Message").unwrap_or_default();
    let throwable = xml_element_text(body, "log4j:Throwable");

    let mut message_lines = message.lines();
    let first = message_lines.next().unwrap_or_default();
    let mut lines = vec![match time {
        Some(time) => format!("[{time}] [{thread}/{level}]: {first}"),
        None => format!("[{thread}/{level}]: {first}"),
    }];
    lines.extend(message_lines.map(str::to_string));
    if let Some(throwable) = throwable {
        lines.extend(
            throwable
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string),
        );
    }
    Some(lines)
}

/// `HH:mm:ss` locale, come `%d{HH:mm:ss}` della `PatternLayout` del gioco.
fn local_time_of_day(millis: i64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
}

fn xml_attribute(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let mut from = 0;
    loop {
        let position = from + header[from..].find(&needle)?;
        if position == 0 || header[..position].ends_with(char::is_whitespace) {
            let value_start = position + needle.len();
            let value_end = value_start + header[value_start..].find('"')?;
            return Some(unescape_xml(&header[value_start..value_end]));
        }
        from = position + needle.len();
    }
}

/// Il testo di `<tag>…</tag>`: le sezioni CDATA come sono, il resto senza
/// entità. Un `</tag>` dentro una CDATA non chiude l'elemento.
fn xml_element_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut rest = &body[body.find(&open)? + open.len()..];
    let mut text = String::new();
    loop {
        let next_close = rest.find(&close)?;
        match rest.find("<![CDATA[") {
            Some(cdata) if cdata < next_close => {
                push_xml_text(&mut text, &rest[..cdata]);
                let inside = &rest[cdata + "<![CDATA[".len()..];
                let end = inside.find("]]>")?;
                text.push_str(&inside[..end]);
                rest = &inside[end + "]]>".len()..];
            }
            _ => {
                push_xml_text(&mut text, &rest[..next_close]);
                return Some(text);
            }
        }
    }
}

/// Text between tags; the indentation of the layout is not part of it.
fn push_xml_text(text: &mut String, raw: &str) {
    if !raw.trim().is_empty() {
        text.push_str(&unescape_xml(raw));
    }
}

fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Il lettore `name`, se finisce entro `deadline`; altrimenti resta staccato
/// e l'errore lo dice.
fn join_reader_thread(handle: JoinHandle<Result<()>>, name: &str, deadline: Instant) -> Result<()> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "the {name} reader was still open {} s after the process exited",
                READER_DRAIN_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    handle
        .join()
        .map_err(|_| anyhow!("{} reader thread panicked", name))?
        .with_context(|| format!("{} reader thread failed", name))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use crate::launch_command::PreparedLaunchCommand;

    use super::{
        decode_output_line, spawn_and_stream_process, Log4jEventDecoder, ProcessEventSink,
        ProcessExitEvent, ProcessLogEvent, ProcessLogStream,
    };

    fn decode_all(lines: &[&str]) -> Vec<String> {
        let mut decoder = Log4jEventDecoder::default();
        let mut decoded = lines
            .iter()
            .flat_map(|line| decoder.push(line.to_string()))
            .collect::<Vec<_>>();
        decoded.extend(decoder.finish());
        decoded
    }

    /// download-1: con la configurazione di log4j di Mojang la console è XML
    /// (`LegacyXMLLayout`); le righe tornano nella forma del gioco, con il
    /// `[Render thread/` che il verificatore cerca.
    #[test]
    fn log4j_xml_events_become_pattern_layout_lines() {
        let decoded = decode_all(&[
            "<log4j:Event logger=\"ezb\" timestamp=\"1700000000000\" level=\"INFO\" thread=\"Render thread\">",
            "  <log4j:Message><![CDATA[Setting user: Player & <friends>]]></log4j:Message>",
            "</log4j:Event>",
            "plain line printed outside log4j",
        ]);

        assert_eq!(decoded.len(), 2, "{decoded:?}");
        assert!(decoded[0].starts_with('['), "{decoded:?}");
        assert!(
            decoded[0].ends_with("] [Render thread/INFO]: Setting user: Player & <friends>"),
            "{decoded:?}"
        );
        assert_eq!(decoded[1], "plain line printed outside log4j");
    }

    #[test]
    fn log4j_xml_event_keeps_multiline_messages_and_throwables() {
        let decoded = decode_all(&[
            "<log4j:Event logger=\"net.minecraft\" timestamp=\"not-a-number\" level=\"ERROR\" thread=\"Server &quot;main&quot;\">",
            "  <log4j:Message><![CDATA[first line",
            "second line with ]]]]><![CDATA[> inside]]></log4j:Message>",
            "  <log4j:Throwable><![CDATA[java.lang.IllegalStateException: boom",
            "\tat net.minecraft.Main.main(Main.java:1)",
            "]]></log4j:Throwable>",
            "</log4j:Event>",
        ]);

        assert_eq!(
            decoded,
            vec![
                "[Server \"main\"/ERROR]: first line".to_string(),
                "second line with ]]> inside".to_string(),
                "java.lang.IllegalStateException: boom".to_string(),
                "\tat net.minecraft.Main.main(Main.java:1)".to_string(),
            ]
        );
    }

    #[test]
    fn an_unfinished_log4j_event_comes_back_as_it_was() {
        let decoded = decode_all(&[
            "<log4j:Event logger=\"x\" timestamp=\"1\" level=\"INFO\" thread=\"main\">",
            "  <log4j:Message><![CDATA[cut by the exit",
        ]);

        assert_eq!(
            decoded,
            vec![
                "<log4j:Event logger=\"x\" timestamp=\"1\" level=\"INFO\" thread=\"main\">"
                    .to_string(),
                "  <log4j:Message><![CDATA[cut by the exit".to_string(),
            ]
        );
    }

    #[derive(Default)]
    struct RecordingProcessEventSink {
        logs: Mutex<Vec<ProcessLogEvent>>,
        exits: Mutex<Vec<ProcessExitEvent>>,
    }

    impl RecordingProcessEventSink {
        fn logs(&self) -> Vec<ProcessLogEvent> {
            self.logs.lock().expect("logs mutex poisoned").clone()
        }

        fn exits(&self) -> Vec<ProcessExitEvent> {
            self.exits.lock().expect("exits mutex poisoned").clone()
        }
    }

    impl ProcessEventSink for RecordingProcessEventSink {
        fn emit_log(&self, event: ProcessLogEvent) -> anyhow::Result<()> {
            self.logs.lock().expect("logs mutex poisoned").push(event);
            Ok(())
        }

        fn emit_exit(&self, event: ProcessExitEvent) -> anyhow::Result<()> {
            self.exits.lock().expect("exits mutex poisoned").push(event);
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn shell_command(script: &str) -> PreparedLaunchCommand {
        PreparedLaunchCommand {
            program: PathBuf::from("powershell"),
            args: vec!["-NoProfile".into(), "-Command".into(), script.into()],
            current_dir: std::env::temp_dir(),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn shell_command(script: &str) -> PreparedLaunchCommand {
        PreparedLaunchCommand {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), script.into()],
            current_dir: std::env::temp_dir(),
        }
    }

    #[cfg(target_os = "windows")]
    const STREAM_SCRIPT: &str =
        "Write-Output 'stdout-line'; [Console]::Error.WriteLine('stderr-line'); exit 7";

    #[cfg(not(target_os = "windows"))]
    const STREAM_SCRIPT: &str = "printf 'stdout-line\n'; printf 'stderr-line\n' 1>&2; exit 7";

    #[test]
    fn streams_stdout_stderr_and_exit_events() {
        let sink = std::sync::Arc::new(RecordingProcessEventSink::default());
        let process = spawn_and_stream_process(shell_command(STREAM_SCRIPT), sink.clone())
            .expect("process should spawn");

        let exit_event = process.wait().expect("process should complete");
        let logs = sink.logs();
        let exits = sink.exits();

        assert!(!exit_event.success);
        assert_eq!(exit_event.exit_code, Some(7));
        assert_eq!(exits, vec![exit_event.clone()]);
        assert!(logs.contains(&ProcessLogEvent {
            stream: ProcessLogStream::Stdout,
            line: "stdout-line".into(),
        }));
        assert!(logs.contains(&ProcessLogEvent {
            stream: ProcessLogStream::Stderr,
            line: "stderr-line".into(),
        }));
    }

    /// concorrenza-10: i byte di una riga Cp1252 non sono UTF-8; la riga resta,
    /// con «�» al posto di quello che non si legge, e il fine riga di Windows
    /// non entra nel testo.
    #[test]
    fn output_lines_are_decoded_lossily_without_their_line_ending() {
        assert_eq!(decode_output_line(b"plain\n"), "plain");
        assert_eq!(decode_output_line(b"windows\r\n"), "windows");
        assert_eq!(
            decode_output_line(b"no newline at eof"),
            "no newline at eof"
        );
        assert_eq!(
            decode_output_line(b"C:\\Users\\Niccol\xf2 perch\xe9 \xa7\r\n"),
            "C:\\Users\\Niccol\u{FFFD} perch\u{FFFD} \u{FFFD}"
        );
    }

    /// concorrenza-10: prima la riga non UTF-8 fermava il lettore con un
    /// errore, il resto dell'output andava perso e `minecraft-exit` non
    /// partiva più.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_non_utf8_line_neither_stops_the_capture_nor_the_exit_event() {
        let sink = std::sync::Arc::new(RecordingProcessEventSink::default());
        let process = spawn_and_stream_process(
            shell_command("printf 'before\\n'; printf 'Niccol\\362\\n'; printf 'after\\n'; exit 3"),
            sink.clone(),
        )
        .expect("process should spawn");

        let exit_event = process.wait().expect("process should complete");
        let lines = sink
            .logs()
            .into_iter()
            .map(|event| event.line)
            .collect::<Vec<_>>();

        assert_eq!(exit_event.exit_code, Some(3));
        assert_eq!(sink.exits(), vec![exit_event]);
        assert_eq!(lines, vec!["before", "Niccol\u{FFFD}", "after"]);
    }

    /// windows-1, il ramo Linux: SIGTERM basta a un processo che lo ascolta,
    /// e il PID non serve più dopo.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn stop_asks_first_and_a_process_that_listens_exits_gracefully() {
        let sink = std::sync::Arc::new(RecordingProcessEventSink::default());
        let process = spawn_and_stream_process(shell_command("exec sleep 30"), sink.clone())
            .expect("process should spawn");

        let outcome = super::stop_process(
            process.pid,
            &process.exit_signal(),
            std::time::Duration::from_secs(10),
        )
        .expect("stop should succeed");
        let exit_event = process.wait().expect("process should complete");

        assert_eq!(outcome, super::StopOutcome::Graceful);
        assert!(!exit_event.success);
        assert_eq!(sink.exits(), vec![exit_event]);
    }

    /// windows-1, il ripiego: un processo che ignora SIGTERM viene ucciso
    /// dopo il margine, e `minecraft-exit` parte comunque.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn stop_forces_a_process_that_ignores_the_request() {
        let sink = std::sync::Arc::new(RecordingProcessEventSink::default());
        let process = spawn_and_stream_process(
            shell_command("trap '' TERM; echo ready; exec sleep 30"),
            sink.clone(),
        )
        .expect("process should spawn");
        // Il segnale prima della `trap` ucciderebbe la shell al primo colpo.
        let ready_by = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !sink.logs().iter().any(|event| event.line == "ready") {
            assert!(
                std::time::Instant::now() < ready_by,
                "the shell never got ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let started = std::time::Instant::now();
        let outcome = super::stop_process(
            process.pid,
            &process.exit_signal(),
            std::time::Duration::from_millis(300),
        )
        .expect("stop should succeed");
        let exit_event = process.wait().expect("process should complete");

        assert_eq!(outcome, super::StopOutcome::Forced);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(sink.exits(), vec![exit_event]);
        assert_eq!(
            super::stop_process(
                1,
                &{
                    let signal = super::ProcessExitSignal::default();
                    signal.mark_exited();
                    signal
                },
                std::time::Duration::from_secs(1),
            )
            .expect("an exited process needs nothing"),
            super::StopOutcome::AlreadyExited
        );
    }
}
