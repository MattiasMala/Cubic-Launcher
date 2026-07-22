use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::config_attribution::ConfigAttributionLaunchConfig;
use crate::loader_metadata::LoaderMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaLaunchSettings {
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub custom_jvm_args: String,
    pub profiler: Option<ProfilerConfig>,
    pub wrapper_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilerConfig {
    pub agent_library_path: PathBuf,
    pub options: Option<String>,
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

impl ProfilerConfig {
    pub fn to_jvm_arg(&self) -> String {
        match self
            .options
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(options) => format!(
                "-agentpath:{}={}",
                self.agent_library_path.display(),
                options
            ),
            None => format!("-agentpath:{}", self.agent_library_path.display()),
        }
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
        build_launch_command, join_classpath_entries, split_argument_string, JavaLaunchRequest,
        JavaLaunchSettings, PreparedLaunchCommand, ProfilerConfig,
    };

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
                profiler: Some(ProfilerConfig {
                    agent_library_path: PathBuf::from("profilers/jprofiler.dll"),
                    options: Some("port=8849".into()),
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
        assert_eq!(
            split_argument_string("-Dfoo=\"a b\""),
            vec!["-Dfoo=a b"]
        );
        assert_eq!(
            split_argument_string("-Xmx2G -Dbar=baz"),
            vec!["-Xmx2G", "-Dbar=baz"]
        );
        assert_eq!(
            split_argument_string("--arg 'x y z'"),
            vec!["--arg", "x y z"]
        );
        assert_eq!(
            split_argument_string("   "),
            Vec::<String>::new()
        );
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
        assert!(command
            .args
            .contains(&"-agentpath:profilers/jprofiler.dll=port=8849".to_string()));
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
