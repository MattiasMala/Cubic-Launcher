use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::config_attribution::ConfigAttributionLaunchConfig;
use crate::loader_metadata::LoaderMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaLaunchSettings {
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub custom_jvm_args: String,
    pub profiler: Option<FlightRecording>,
    pub wrapper_command: Option<String>,
}

/// Il profiler è JFR, dentro la JVM (D108): non serve una libreria né un agente, quindi
/// porta solo quello che JFR deve sapere, cioè dove scrivere il file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlightRecording {
    pub output_file: PathBuf,
}

/// Il nome del file nella cartella del lancio: la cartella è già unica per lancio (D109).
const FLIGHT_RECORDING_FILE_NAME: &str = "profile.jfr";

/// `-XX:StartFlightRecording` non esiste in Java 8 prima della 8u262, e un flag JVM non
/// riconosciuto non fa partire la JVM: sotto questa major il profiler non si aggiunge (V3).
const FLIGHT_RECORDING_MIN_JAVA: u32 = 11;

/// Gli inizi di argomento che dicono «l'utente sta già profilando» (V2). Insieme chiuso, che
/// sbaglia nella direzione voluta: un flag che non riconosce porta ad aggiungere JFR, mai a
/// toccare un argomento scritto dall'utente.
const USER_PROFILING_ARGUMENT_PREFIXES: [&str; 6] = [
    "-XX:StartFlightRecording",
    "-XX:FlightRecorderOptions",
    "-XX:+FlightRecorder",
    "-agentpath:",
    "-agentlib:",
    "-javaagent:",
];

/// Cosa fa il profiler in un lancio, con il motivo quando non registra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfilerDecision {
    Off,
    Record(FlightRecording),
    /// D111: i JVM args dell'utente contengono già `argument`, e vincono loro.
    UserArgumentsProfile {
        argument: String,
    },
    /// V3: il lancio usa una Java sotto `FLIGHT_RECORDING_MIN_JAVA`.
    JavaTooOld {
        java_major: u32,
    },
}

pub fn decide_profiler(
    enabled: bool,
    custom_jvm_args: &str,
    java_major: u32,
    launch_log_dir: &Path,
) -> ProfilerDecision {
    if !enabled {
        return ProfilerDecision::Off;
    }

    if let Some(argument) = split_argument_string(custom_jvm_args)
        .into_iter()
        .find(|argument| {
            USER_PROFILING_ARGUMENT_PREFIXES
                .iter()
                .any(|prefix| argument.starts_with(prefix))
        })
    {
        return ProfilerDecision::UserArgumentsProfile { argument };
    }

    if java_major < FLIGHT_RECORDING_MIN_JAVA {
        return ProfilerDecision::JavaTooOld { java_major };
    }

    ProfilerDecision::Record(FlightRecording {
        output_file: launch_log_dir.join(FLIGHT_RECORDING_FILE_NAME),
    })
}

impl ProfilerDecision {
    pub fn into_recording(self) -> Option<FlightRecording> {
        match self {
            Self::Record(recording) => Some(recording),
            Self::Off | Self::UserArgumentsProfile { .. } | Self::JavaTooOld { .. } => None,
        }
    }

    /// La riga per il log del lancio: un profiler chiesto e non aggiunto lo dice sempre.
    pub fn launch_log_line(&self) -> Option<String> {
        match self {
            Self::Off => None,
            Self::Record(recording) => Some(format!(
                "[Profiler] Recording with JFR to {}",
                recording.output_file.display()
            )),
            Self::UserArgumentsProfile { argument } => Some(format!(
                "[Profiler] Not added: your JVM arguments already contain {argument}, and they win."
            )),
            Self::JavaTooOld { java_major } => Some(format!(
                "[Profiler] Skipped: JFR needs Java {FLIGHT_RECORDING_MIN_JAVA}, this launch uses Java {java_major}."
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaLaunchRequest {
    pub java_binary_path: PathBuf,
    pub working_directory: PathBuf,
    pub classpath_entries: Vec<PathBuf>,
    pub loader_metadata: LoaderMetadata,
    pub launch_settings: JavaLaunchSettings,
    pub additional_game_arguments: Vec<String>,
    pub config_attribution: Option<ConfigAttributionLaunchConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunchCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
}

pub fn build_launch_command(request: &JavaLaunchRequest) -> Result<PreparedLaunchCommand> {
    if request.launch_settings.min_ram_mb == 0 {
        bail!("min_ram_mb must be greater than zero");
    }

    if request.launch_settings.max_ram_mb == 0 {
        bail!("max_ram_mb must be greater than zero");
    }

    if request.launch_settings.min_ram_mb > request.launch_settings.max_ram_mb {
        bail!("min_ram_mb cannot exceed max_ram_mb");
    }

    if request.classpath_entries.is_empty() {
        bail!("classpath_entries cannot be empty");
    }

    let java_invocation_args = build_java_invocation_args(request);

    #[cfg(target_os = "linux")]
    {
        if let Some(wrapper_command) = request.launch_settings.wrapper_command.as_deref() {
            let wrapper_parts = split_argument_string(wrapper_command);
            if let Some((program, wrapper_args)) = wrapper_parts.split_first() {
                let mut args = wrapper_args.to_vec();
                args.push(request.java_binary_path.to_string_lossy().to_string());
                args.extend(java_invocation_args);

                return Ok(PreparedLaunchCommand {
                    program: PathBuf::from(program),
                    args,
                    current_dir: request.working_directory.clone(),
                });
            }
        }
    }

    Ok(PreparedLaunchCommand {
        program: request.java_binary_path.clone(),
        args: java_invocation_args,
        current_dir: request.working_directory.clone(),
    })
}

fn build_java_invocation_args(request: &JavaLaunchRequest) -> Vec<String> {
    let mut args = Vec::new();

    args.extend(request.loader_metadata.jvm_arguments.clone());
    args.push(format!("-Xms{}M", request.launch_settings.min_ram_mb));
    args.push(format!("-Xmx{}M", request.launch_settings.max_ram_mb));
    args.extend(split_argument_string(
        &request.launch_settings.custom_jvm_args,
    ));

    if let Some(profiler) = &request.launch_settings.profiler {
        args.push(profiler.to_jvm_arg());
    }

    if let Some(config_attribution) = &request.config_attribution {
        args.extend(config_attribution.to_jvm_args());
    }

    args.push("-cp".to_string());
    args.push(join_classpath_entries(&request.classpath_entries));
    args.push(request.loader_metadata.main_class.clone());
    args.extend(request.loader_metadata.game_arguments.clone());
    args.extend(request.additional_game_arguments.clone());

    args
}

impl FlightRecording {
    /// V1: `settings=profile` per il dettaglio; `dumponexit=true` perché senza di lui una
    /// chiusura normale del gioco può non lasciare niente sul disco.
    pub fn to_jvm_arg(&self) -> String {
        format!(
            "-XX:StartFlightRecording=settings=profile,dumponexit=true,filename={}",
            jfr_option_value(&self.output_file.to_string_lossy())
        )
    }
}

/// Un valore per le opzioni di `-XX:StartFlightRecording`, che JFR separa con «,».
///
/// todo.md:1812: con una virgola nel percorso dei dati (una home «Rossi, Mario») il nome
/// del file si spezzava: la JVM partiva ma scriveva il file altrove, e il bottone Profile
/// non compariva. Il parser delle opzioni JFR (quello dei comandi diagnostici) accetta un
/// valore fra virgolette doppie o semplici, e dentro la virgola non separa più: provato su
/// Java 17 e 25. Senza virgola il valore resta com'era.
fn jfr_option_value(value: &str) -> String {
    if !value.contains(',') {
        value.to_string()
    } else if !value.contains('"') {
        format!("\"{value}\"")
    } else {
        format!("'{value}'")
    }
}

pub fn split_argument_string(arguments: &str) -> Vec<String> {
    let mut parsed_arguments = Vec::new();
    let mut current = String::new();
    let mut in_quote = None;
    let mut has_token = false;

    for character in arguments.chars() {
        match in_quote {
            Some(quote) if character == quote => {
                in_quote = None;
            }
            Some(_) => current.push(character),
            None if matches!(character, '"' | '\'') => {
                in_quote = Some(character);
                has_token = true;
            }
            None if character.is_whitespace() => {
                if has_token {
                    parsed_arguments.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(character);
                has_token = true;
            }
        }
    }

    if has_token {
        parsed_arguments.push(current);
    }

    parsed_arguments
}

pub fn join_classpath_entries(classpath_entries: &[PathBuf]) -> String {
    let separator = if cfg!(target_os = "windows") {
        ';'
    } else {
        ':'
    };

    classpath_entries
        .iter()
        .map(|entry| entry.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(&separator.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config_attribution::ConfigAttributionLaunchConfig;
    use crate::loader_metadata::LoaderMetadata;
    use crate::resolver::ModLoader;

    use super::{
        build_launch_command, join_classpath_entries, split_argument_string, FlightRecording,
        JavaLaunchRequest, JavaLaunchSettings, PreparedLaunchCommand,
    };

    /// todo.md:1812: con una virgola nel percorso il nome del file va fra virgolette, o
    /// JFR prende il pezzo dopo la virgola per un'altra opzione.
    #[test]
    fn a_comma_in_the_jfr_path_is_quoted_and_a_plain_path_is_not() {
        let plain = FlightRecording {
            output_file: PathBuf::from("/home/mario/logs/launches/1-pack/profile.jfr"),
        };
        assert_eq!(
            plain.to_jvm_arg(),
            "-XX:StartFlightRecording=settings=profile,dumponexit=true,filename=/home/mario/logs/launches/1-pack/profile.jfr"
        );

        let with_comma = FlightRecording {
            output_file: PathBuf::from("/home/Rossi, Mario/logs/launches/1-pack/profile.jfr"),
        };
        assert_eq!(
            with_comma.to_jvm_arg(),
            "-XX:StartFlightRecording=settings=profile,dumponexit=true,filename=\"/home/Rossi, Mario/logs/launches/1-pack/profile.jfr\""
        );

        assert_eq!(super::jfr_option_value("a,\"b"), "'a,\"b'");
    }

    fn sample_loader_metadata() -> LoaderMetadata {
        LoaderMetadata {
            mod_loader: ModLoader::Fabric,
            minecraft_version: "1.21.1".into(),
            loader_version: "0.16.14".into(),
            main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".into(),
            libraries: Vec::new(),
            maven_files: Vec::new(),
            jvm_arguments: vec!["-Dfabric.example=true".into()],
            game_arguments: vec!["--launchTarget".into(), "fabric_client".into()],
            min_java_version: Some(8),
        }
    }

    fn sample_request() -> JavaLaunchRequest {
        JavaLaunchRequest {
            java_binary_path: PathBuf::from("C:/Java/bin/java.exe"),
            working_directory: PathBuf::from("mod-lists/Pack/instances/1.21.1-Fabric"),
            classpath_entries: vec![
                PathBuf::from("libraries/fabric-loader.jar"),
                PathBuf::from("minecraft/client.jar"),
            ],
            loader_metadata: sample_loader_metadata(),
            launch_settings: JavaLaunchSettings {
                min_ram_mb: 2048,
                max_ram_mb: 4096,
                custom_jvm_args: "-XX:+UseG1GC -Dcustom=true".into(),
                profiler: Some(FlightRecording {
                    output_file: PathBuf::from("launch-logs/1-pack/profile.jfr"),
                }),
                wrapper_command: None,
            },
            additional_game_arguments: vec!["--username".into(), "PlayerOne".into()],
            config_attribution: Some(ConfigAttributionLaunchConfig {
                agent_jar_path: PathBuf::from("java-agent/build/libs/config-agent.jar"),
                output_file_path: PathBuf::from("temp/config-attribution.ndjson"),
                mods_cache_dir: PathBuf::from("cache/mods"),
            }),
        }
    }

    #[test]
    fn splits_argument_strings_by_whitespace() {
        assert_eq!(
            split_argument_string("  -Xmx4G   -Dfoo=bar  "),
            vec!["-Xmx4G", "-Dfoo=bar"]
        );
    }

    #[test]
    fn split_argument_string_preserves_quoted_spaces() {
        assert_eq!(split_argument_string("-Dfoo=\"a b\""), vec!["-Dfoo=a b"]);
        assert_eq!(
            split_argument_string("-Xmx2G -Dbar=baz"),
            vec!["-Xmx2G", "-Dbar=baz"]
        );
        assert_eq!(
            split_argument_string("--arg 'x y z'"),
            vec!["--arg", "x y z"]
        );
        assert_eq!(split_argument_string("   "), Vec::<String>::new());
        assert_eq!(split_argument_string(""), Vec::<String>::new());
    }

    #[test]
    fn joins_classpath_entries_with_platform_separator() {
        let classpath = join_classpath_entries(&[
            PathBuf::from("libraries/loader.jar"),
            PathBuf::from("minecraft/client.jar"),
        ]);

        if cfg!(target_os = "windows") {
            assert_eq!(classpath, "libraries/loader.jar;minecraft/client.jar");
        } else {
            assert_eq!(classpath, "libraries/loader.jar:minecraft/client.jar");
        }
    }

    #[test]
    fn builds_java_launch_command_with_loader_profiler_and_agent_inputs() {
        let request = sample_request();

        let command = build_launch_command(&request).expect("launch command should build");

        assert_eq!(command.program, PathBuf::from("C:/Java/bin/java.exe"));
        assert_eq!(
            command.current_dir,
            PathBuf::from("mod-lists/Pack/instances/1.21.1-Fabric")
        );
        assert!(command.args.contains(&"-Dfabric.example=true".to_string()));
        assert!(command.args.contains(&"-Xms2048M".to_string()));
        assert!(command.args.contains(&"-Xmx4096M".to_string()));
        assert!(command.args.contains(
            &"-XX:StartFlightRecording=settings=profile,dumponexit=true,filename=launch-logs/1-pack/profile.jfr"
                .to_string()
        ));
        assert!(command
            .args
            .contains(&"-javaagent:java-agent/build/libs/config-agent.jar".to_string()));
        assert!(command
            .args
            .contains(&"net.fabricmc.loader.impl.launch.knot.KnotClient".to_string()));
        assert!(command.args.contains(&"--launchTarget".to_string()));
        assert!(command.args.contains(&"fabric_client".to_string()));
        assert!(command.args.contains(&"--username".to_string()));
        assert!(command.args.contains(&"PlayerOne".to_string()));
    }

    #[test]
    fn rejects_invalid_memory_configuration() {
        let mut request = sample_request();
        request.launch_settings.min_ram_mb = 8192;
        request.launch_settings.max_ram_mb = 4096;

        let error = build_launch_command(&request).expect_err("command should fail");

        assert!(error
            .to_string()
            .contains("min_ram_mb cannot exceed max_ram_mb"));
    }

    #[test]
    fn rejects_empty_classpath() {
        let mut request = sample_request();
        request.classpath_entries.clear();

        let error = build_launch_command(&request).expect_err("command should fail");

        assert!(error
            .to_string()
            .contains("classpath_entries cannot be empty"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn ignores_wrapper_command_outside_linux() {
        let mut request = sample_request();
        request.launch_settings.wrapper_command = Some("gamemoderun mangohud".into());

        let command = build_launch_command(&request).expect("launch command should build");

        assert_eq!(command.program, PathBuf::from("C:/Java/bin/java.exe"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepends_wrapper_command_on_linux() {
        let mut request = sample_request();
        request.java_binary_path = PathBuf::from("/usr/bin/java");
        request.launch_settings.wrapper_command = Some("gamemoderun mangohud".into());

        let command = build_launch_command(&request).expect("launch command should build");

        assert_eq!(
            command,
            PreparedLaunchCommand {
                program: PathBuf::from("gamemoderun"),
                args: {
                    let mut args = vec!["mangohud".to_string(), "/usr/bin/java".to_string()];
                    args.extend(super::build_java_invocation_args(&request));
                    args
                },
                current_dir: PathBuf::from("mod-lists/Pack/instances/1.21.1-Fabric"),
            }
        );
    }
}
