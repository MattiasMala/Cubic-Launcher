use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::app_shell::{ShellGlobalSettings, ShellModListOverrides};
use crate::launch_command::{build_launch_command, split_argument_string, JavaLaunchRequest};
use crate::modrinth::{ModrinthFile, ModrinthVersion};
use crate::resolver::{ModLoader, ResolutionTarget};

use super::{
    active_account_plays_offline, automation_exit_code, build_instance_root,
    build_modded_classpath_entries, contained_loader_library_path, embedded_min_java_requirement,
    fabric_dependency_predicates_match, filter_minecraft_launch_game_arguments,
    forge_wrapper_installer_artifact, installed_java_for_launch,
    load_active_account_for_launch_with_diagnostics, load_modlist, local_mod_jar_path,
    maven_artifact_relative_path, merge_minecraft_and_loader_game_arguments,
    merge_minecraft_and_loader_jvm_arguments, minecraft_version_predicate_matches,
    minimum_java_version_for_predicate, parse_mod_loader, relative_loader_library_path,
    split_installations_by_host_architecture, substitute_known_placeholders,
    validate_content_filename, EffectiveLaunchSettings, EmbeddedFabricRequirementSet,
    EmbeddedFabricRequirements, LaunchDirectories, LaunchPlaceholders, LaunchRequest,
    LaunchResolutionPath, LaunchVerificationRequest, LaunchVerificationResult, PlayerIdentity,
};
use crate::java_runtime::{JavaInstallation, JavaInstallationSource};
use crate::loader_metadata::{LibraryDownloadArtifact, LoaderLibrary, LoaderMetadata};

#[test]
fn launch_account_keeps_identity_when_token_key_is_unavailable() {
    struct UnavailableSecretStore;

    impl crate::token_storage::SecretStore for UnavailableSecretStore {
        fn get_secret(&self, _key: &str) -> anyhow::Result<Option<String>> {
            Err(anyhow::anyhow!("keyring unavailable"))
        }

        fn set_secret(&self, _key: &str, _secret: &str) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("keyring unavailable"))
        }
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let database_path = std::env::temp_dir().join(format!(
        "cubic-launch-account-{}-{timestamp}.db",
        std::process::id()
    ));
    crate::database::initialize_database(&database_path).expect("initialize database");
    let connection = rusqlite::Connection::open(&database_path).expect("open database");
    crate::microsoft_auth::AccountsRepository::new(&connection)
        .upsert_account(&crate::microsoft_auth::AccountRecord {
            microsoft_id: "microsoft-id".into(),
            xbox_gamertag: Some("StoredPlayer".into()),
            minecraft_uuid: Some("123456781234123412341234567890ab".into()),
            access_token_enc: Some(vec![1, 2, 3]),
            refresh_token_enc: Some(vec![1, 2, 3]),
            profile_data: Some(r#"{"username":"ProfilePlayer"}"#.into()),
            is_active: true,
            plays_offline: false,
        })
        .expect("insert active account");

    let (loaded, refresh_token_error) =
        load_active_account_for_launch_with_diagnostics(&connection, UnavailableSecretStore)
            .expect("load active account");
    let loaded = loaded.expect("active account");

    assert_eq!(loaded.xbox_gamertag.as_deref(), Some("StoredPlayer"));
    assert_eq!(
        loaded.profile_data.as_deref(),
        Some(r#"{"username":"ProfilePlayer"}"#)
    );
    assert_eq!(
        loaded.minecraft_uuid.as_deref(),
        Some("123456781234123412341234567890ab")
    );
    assert_eq!(loaded.refresh_token, None);
    // The unusable credential key must now be reported, not silently swallowed.
    assert!(refresh_token_error.is_some());
    drop(connection);
    std::fs::remove_file(database_path).expect("remove database");
}

/// D115: the choice is read from the database row of the active account, and
/// it decides the path by itself — no map from the pre-check is needed, and a
/// map that arrives anyway is not followed, because the preresolved path asks
/// Modrinth for the versions it does not have.
#[test]
fn an_active_account_that_plays_offline_launches_on_the_zero_request_path() {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root_dir = std::env::temp_dir().join(format!(
        "cubic-launch-offline-account-{}-{timestamp}",
        std::process::id()
    ));
    let launcher_paths = crate::launcher_paths::LauncherPaths::new(&root_dir);
    crate::database::initialize_database(launcher_paths.database_path())
        .expect("initialize database");
    let connection =
        rusqlite::Connection::open(launcher_paths.database_path()).expect("open database");
    let accounts = crate::microsoft_auth::AccountsRepository::new(&connection);
    accounts
        .upsert_account(&crate::microsoft_auth::AccountRecord {
            microsoft_id: "microsoft-id".into(),
            xbox_gamertag: Some("StoredPlayer".into()),
            minecraft_uuid: Some("123456781234123412341234567890ab".into()),
            access_token_enc: Some(vec![1, 2, 3]),
            refresh_token_enc: Some(vec![1, 2, 3]),
            profile_data: None,
            is_active: true,
            plays_offline: false,
        })
        .expect("insert active account");

    // Before the choice: today's path, chosen by the map.
    let plays_offline = active_account_plays_offline(&launcher_paths).expect("read the choice");
    assert!(!plays_offline);
    assert_eq!(
        LaunchResolutionPath::for_active_account(plays_offline, None),
        LaunchResolutionPath::Resolve
    );

    accounts
        .set_plays_offline("microsoft-id", true)
        .expect("save the choice");
    let plays_offline = active_account_plays_offline(&launcher_paths).expect("read the choice");
    assert!(plays_offline);
    let path = LaunchResolutionPath::for_active_account(plays_offline, None);
    assert_eq!(path, LaunchResolutionPath::Offline);
    assert_eq!(path.preresolved(), None);
    assert_eq!(path.summary_label(), "offline");

    let map = HashMap::from([("sodium".to_string(), "version-id".to_string())]);
    assert_eq!(
        LaunchResolutionPath::for_active_account(plays_offline, Some(&map)),
        LaunchResolutionPath::Offline
    );

    drop(connection);
    std::fs::remove_dir_all(root_dir).expect("remove root");
}

fn global_settings() -> ShellGlobalSettings {
    ShellGlobalSettings {
        min_ram_mb: 2048,
        max_ram_mb: 4096,
        custom_jvm_args: "-Dglobal=true".into(),
        profiler_enabled: false,
        update_notifications_enabled: true,
        update_notifications_resource_packs: true,
        update_notifications_data_packs: true,
        update_notifications_shaders: true,
        wrapper_command: "gamemoderun".into(),
        java_path_override: "/custom/java".into(),
    }
}

fn modlist_overrides() -> ShellModListOverrides {
    ShellModListOverrides {
        modlist_name: Some("Pack".into()),
        min_ram_mb: Some(3072),
        max_ram_mb: Some(6144),
        custom_jvm_args: Some("-Dmodlist=true".into()),
        profiler_enabled: Some(false),
        wrapper_command: Some("mangohud".into()),
        minecraft_version: None,
        mod_loader: None,
    }
}

#[test]
fn parses_supported_mod_loader_names() {
    assert_eq!(parse_mod_loader("Fabric").unwrap(), ModLoader::Fabric);
    assert_eq!(parse_mod_loader("Forge").unwrap(), ModLoader::Forge);
    assert_eq!(parse_mod_loader("NeoForge").unwrap(), ModLoader::NeoForge);
    assert!(parse_mod_loader("quilt").is_err());
}

#[test]
fn effective_launch_settings_prefer_modlist_overrides() {
    let settings =
        EffectiveLaunchSettings::from_shell_settings(&global_settings(), &modlist_overrides());

    assert_eq!(settings.min_ram_mb, 3072);
    assert_eq!(settings.max_ram_mb, 6144);
    assert_eq!(settings.custom_jvm_args, "-Dmodlist=true");
    assert_eq!(settings.wrapper_command, Some("mangohud".into()));
    assert_eq!(
        settings.java_path_override,
        Some(PathBuf::from("/custom/java"))
    );
}

/// shell-5 (audit 101): min e max si combinano ognuno per conto suo, quindi
/// il controllo guarda la coppia effettiva. Globali 2048/4096 con il solo
/// «Max RAM» della mod list a 1536 è il caso dell'audit; l'errore dice da dove
/// viene ciascun valore.
#[test]
fn effective_ram_is_validated_as_a_pair_of_global_and_override_values() {
    use crate::app_shell::validate_effective_ram;

    let error = validate_effective_ram(2048, 4096, None, Some(1536))
        .expect_err("an override max below the global min is invalid");
    assert_eq!(
        error.to_string(),
        "Minimum RAM (2048 MB, from the global settings) cannot exceed maximum RAM (1536 MB, set for this mod list). Change one of them in Settings."
    );
    let error = validate_effective_ram(2048, 4096, Some(8192), None)
        .expect_err("an override min above the global max is invalid");
    assert!(error.to_string().contains("8192 MB, set for this mod list"));
    let error =
        validate_effective_ram(2048, 4096, Some(0), None).expect_err("zero minimum RAM is invalid");
    assert!(error.to_string().contains("greater than zero"), "{error}");
    let error =
        validate_effective_ram(2048, 4096, None, Some(0)).expect_err("zero maximum RAM is invalid");
    assert!(error.to_string().contains("Maximum RAM"), "{error}");

    validate_effective_ram(2048, 4096, None, None).expect("the globals alone are valid");
    validate_effective_ram(2048, 4096, Some(4096), None).expect("min equal to max is valid");
    validate_effective_ram(2048, 4096, Some(8192), Some(12288))
        .expect("both overrides, consistent, are valid");
    let settings =
        EffectiveLaunchSettings::from_shell_settings(&global_settings(), &modlist_overrides());
    validate_effective_ram(
        global_settings().min_ram_mb,
        global_settings().max_ram_mb,
        modlist_overrides().min_ram_mb,
        modlist_overrides().max_ram_mb,
    )
    .expect("the fixture is a valid launch");
    assert!(settings.min_ram_mb <= settings.max_ram_mb);
}

/// shell-5: il salvataggio degli override rifiuta una coppia effettiva
/// sbagliata e non scrive niente; una coppia giusta si salva.
#[test]
fn saving_modlist_overrides_rejects_an_invalid_effective_ram_pair() {
    use crate::app_shell::{
        save_global_settings, save_modlist_overrides, ShellGlobalSettingsInput,
        ShellModListOverridesInput,
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let database_path = std::env::temp_dir().join(format!(
        "cubic-shell-5-{}-{timestamp}.db",
        std::process::id()
    ));
    crate::database::initialize_database(&database_path).expect("initialize database");
    let connection = rusqlite::Connection::open(&database_path).expect("open database");
    save_global_settings(
        &connection,
        &ShellGlobalSettingsInput {
            min_ram_mb: 2048,
            max_ram_mb: 4096,
            custom_jvm_args: String::new(),
            profiler_enabled: false,
            update_notifications_enabled: true,
            update_notifications_resource_packs: true,
            update_notifications_data_packs: true,
            update_notifications_shaders: true,
            wrapper_command: String::new(),
            java_path_override: String::new(),
        },
    )
    .expect("global settings");
    let overrides = |min_ram_mb, max_ram_mb| ShellModListOverridesInput {
        modlist_name: "Pack".into(),
        min_ram_mb,
        max_ram_mb,
        custom_jvm_args: Some("-Dkept=true".into()),
        profiler_enabled: None,
        wrapper_command: None,
        minecraft_version: None,
        mod_loader: None,
    };
    let stored = || -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM modlist_settings WHERE modlist_name = 'Pack'",
                [],
                |row| row.get(0),
            )
            .expect("count")
    };

    let error = save_modlist_overrides(&connection, &overrides(None, Some(1536)))
        .expect_err("max override below the global min");
    assert!(error.to_string().contains("cannot exceed"), "{error}");
    assert_eq!(stored(), 0, "nothing is written when the pair is invalid");

    save_modlist_overrides(&connection, &overrides(None, Some(3072)))
        .expect("max override above the global min");
    assert_eq!(stored(), 2);

    drop(connection);
    std::fs::remove_file(database_path).ok();
}

/// r2-percorsi-non-ascii-e-limiti-windows-5 (audit 101): «Copia come percorso»
/// di Windows mette il percorso tra virgolette, e con le virgolette ogni lancio
/// falliva con «is not runnable».
#[test]
fn java_path_override_loses_the_quotes_of_a_pasted_path() {
    let mut global = global_settings();
    for pasted in [
        r#""C:\Program Files\Java\jdk-21\bin\java.exe""#,
        r#"  "C:\Program Files\Java\jdk-21\bin\java.exe"  "#,
        r#"'C:\Program Files\Java\jdk-21\bin\java.exe'"#,
        "\t C:\\Program Files\\Java\\jdk-21\\bin\\java.exe \n",
    ] {
        global.java_path_override = pasted.into();
        assert_eq!(
            EffectiveLaunchSettings::from_shell_settings(&global, &modlist_overrides())
                .java_path_override,
            Some(PathBuf::from(r"C:\Program Files\Java\jdk-21\bin\java.exe")),
            "{pasted:?}"
        );
    }

    // Only quotes, or nothing: no override at all.
    for empty in [r#""""#, "  ", r#"" ""#] {
        global.java_path_override = empty.into();
        assert_eq!(
            EffectiveLaunchSettings::from_shell_settings(&global, &modlist_overrides())
                .java_path_override,
            None,
            "{empty:?}"
        );
    }
}

#[test]
fn effective_launch_settings_prefer_the_modlist_profiler_override() {
    // L'override vince in tutte e due le direzioni; senza override vale il globale.
    let mut global = global_settings();
    let mut overrides = modlist_overrides();

    global.profiler_enabled = true;
    overrides.profiler_enabled = Some(false);
    assert!(!EffectiveLaunchSettings::from_shell_settings(&global, &overrides).profiler_enabled);

    overrides.profiler_enabled = None;
    assert!(EffectiveLaunchSettings::from_shell_settings(&global, &overrides).profiler_enabled);

    global.profiler_enabled = false;
    overrides.profiler_enabled = Some(true);
    assert!(EffectiveLaunchSettings::from_shell_settings(&global, &overrides).profiler_enabled);
}

fn profiler_launch_dir() -> PathBuf {
    PathBuf::from("/launcher/launch-logs/1-pack-1-21-1-fabric-0-0")
}

/// Impostazioni col profiler globale acceso o spento, i JVM args dati e nessun override
/// per mod list: il profiler arriva agli argomenti per la strada vera, `from_shell_settings`.
fn settings_with_profiler(enabled: bool, custom_jvm_args: &str) -> EffectiveLaunchSettings {
    let mut global = global_settings();
    global.profiler_enabled = enabled;
    global.custom_jvm_args = custom_jvm_args.into();
    global.wrapper_command = String::new();
    let overrides = ShellModListOverrides {
        modlist_name: Some("Pack".into()),
        min_ram_mb: None,
        max_ram_mb: None,
        custom_jvm_args: None,
        profiler_enabled: None,
        wrapper_command: None,
        minecraft_version: None,
        mod_loader: None,
    };
    EffectiveLaunchSettings::from_shell_settings(&global, &overrides)
}

/// Gli argomenti che il lancio passa alla JVM con queste impostazioni e questa major di Java.
fn jvm_args_for(settings: &EffectiveLaunchSettings, java_major: u32) -> Vec<String> {
    let decision = settings.profiler_decision(java_major, &profiler_launch_dir());
    let request = JavaLaunchRequest {
        java_binary_path: PathBuf::from("/java/bin/java"),
        working_directory: PathBuf::from("/instance"),
        classpath_entries: vec![PathBuf::from("client.jar")],
        loader_metadata: LoaderMetadata {
            mod_loader: ModLoader::Fabric,
            minecraft_version: "1.21.1".into(),
            loader_version: "0.16.14".into(),
            main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".into(),
            libraries: Vec::new(),
            maven_files: Vec::new(),
            jvm_arguments: vec!["-Dloader=true".into()],
            game_arguments: Vec::new(),
            min_java_version: None,
        },
        launch_settings: settings.java_launch_settings(decision.into_recording()),
        additional_game_arguments: Vec::new(),
        config_attribution: None,
    };
    build_launch_command(&request)
        .expect("launch command should build")
        .args
}

fn is_profiling_flag(argument: &str) -> bool {
    argument.starts_with("-XX:StartFlightRecording") || argument.starts_with("-javaagent:")
}

#[test]
fn an_enabled_profiler_records_jfr_into_the_launch_log_dir() {
    let args = jvm_args_for(&settings_with_profiler(true, "-XX:+UseG1GC"), 21);

    let recordings = args
        .iter()
        .filter_map(|argument| argument.strip_prefix("-XX:StartFlightRecording="))
        .collect::<Vec<_>>();
    assert_eq!(recordings.len(), 1, "{args:?}");
    let options = recordings[0].split(',').collect::<Vec<_>>();
    let filename = options
        .iter()
        .find_map(|option| option.strip_prefix("filename="))
        .expect("the recording names its file");
    assert_eq!(
        Path::new(filename).parent(),
        Some(profiler_launch_dir().as_path())
    );
    // Senza `dumponexit=true` una chiusura normale del gioco può non lasciare niente.
    assert!(options.contains(&"dumponexit=true"), "{options:?}");
    assert!(options.contains(&"settings=profile"), "{options:?}");
}

#[test]
fn a_profiling_flag_in_the_user_jvm_args_wins_over_the_toggle() {
    for user_args in [
        "-XX:+UseG1GC -XX:StartFlightRecording=filename=/home/me/mine.jfr -Dkeep=true",
        "-javaagent:qualcosa.jar -Dkeep=true",
    ] {
        let args = jvm_args_for(&settings_with_profiler(true, user_args), 21);

        let profiling_flags = args.iter().filter(|argument| is_profiling_flag(argument));
        assert_eq!(profiling_flags.count(), 1, "{args:?}");
        let user_tokens = split_argument_string(user_args);
        assert!(
            args.windows(user_tokens.len())
                .any(|window| window == user_tokens.as_slice()),
            "{args:?}"
        );
        assert_eq!(
            args,
            jvm_args_for(&settings_with_profiler(false, user_args), 21)
        );
    }
}

#[test]
fn jfr_is_left_out_below_java_11_and_the_launch_log_says_so() {
    let settings = settings_with_profiler(true, "-XX:+UseG1GC");

    let args = jvm_args_for(&settings, 8);
    assert!(
        !args
            .iter()
            .any(|argument| argument.contains("StartFlightRecording")),
        "{args:?}"
    );
    assert_eq!(
        args,
        jvm_args_for(&settings_with_profiler(false, "-XX:+UseG1GC"), 8)
    );
    let line = settings
        .profiler_decision(8, &profiler_launch_dir())
        .launch_log_line()
        .expect("a skipped profiler is said in the launch log");
    assert!(
        line.contains("Java 11") && line.contains("Java 8"),
        "{line}"
    );

    assert!(jvm_args_for(&settings, 11)
        .iter()
        .any(|argument| argument.starts_with("-XX:StartFlightRecording=")));
}

#[test]
fn a_disabled_profiler_leaves_the_jvm_args_as_they_were() {
    let args = jvm_args_for(
        &settings_with_profiler(false, "-XX:+UseG1GC -Dcustom=\"a b\""),
        21,
    );

    assert_eq!(
        args,
        vec![
            "-Dloader=true",
            "-Xms2048M",
            "-Xmx4096M",
            "-XX:+UseG1GC",
            "-Dcustom=a b",
            "-cp",
            "client.jar",
            "net.fabricmc.loader.impl.launch.knot.KnotClient",
        ]
    );
}

#[test]
fn a_version_map_sends_the_launch_down_the_preresolved_path() {
    let map = HashMap::from([("sodium".to_string(), "abc123".to_string())]);

    assert_eq!(
        LaunchResolutionPath::for_launch(Some(&map)),
        LaunchResolutionPath::Preresolved(&map)
    );
}

#[test]
fn no_version_map_keeps_the_resolving_path() {
    assert_eq!(
        LaunchResolutionPath::for_launch(None),
        LaunchResolutionPath::Resolve
    );
}

#[test]
fn an_empty_version_map_is_not_a_decision() {
    let empty = HashMap::new();

    assert_eq!(
        LaunchResolutionPath::for_launch(Some(&empty)),
        LaunchResolutionPath::Resolve
    );
    assert_eq!(
        LaunchResolutionPath::for_launch(Some(&empty)).preresolved(),
        None
    );
}

#[test]
fn maven_coordinates_expand_to_standard_artifact_path() {
    assert_eq!(
        maven_artifact_relative_path("net.fabricmc:fabric-loader:0.16.14").unwrap(),
        PathBuf::from("net/fabricmc/fabric-loader/0.16.14/fabric-loader-0.16.14.jar")
    );
    assert_eq!(
        maven_artifact_relative_path("org.lwjgl:lwjgl:3.3.3:natives-windows").unwrap(),
        PathBuf::from("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar")
    );
}

#[test]
fn loader_library_path_traversal_is_contained() {
    let malicious = LoaderLibrary {
        name: "example:malicious:1.0".into(),
        url: None,
        download: Some(LibraryDownloadArtifact {
            url: "https://example.invalid/outside.jar".into(),
            path: Some("../../outside.jar".into()),
            sha1: None,
            size: None,
        }),
    };
    let relative_path = relative_loader_library_path(&malicious).unwrap();
    assert!(
        contained_loader_library_path(PathBuf::from("/libraries").as_path(), &relative_path)
            .is_err()
    );

    let valid = LoaderLibrary {
        name: "example:valid:1.0".into(),
        url: None,
        download: Some(LibraryDownloadArtifact {
            url: "https://example.invalid/valid.jar".into(),
            path: Some("org/example/valid.jar".into()),
            sha1: None,
            size: None,
        }),
    };
    let relative_path = relative_loader_library_path(&valid).unwrap();
    assert_eq!(
        contained_loader_library_path(PathBuf::from("/libraries").as_path(), &relative_path)
            .unwrap(),
        PathBuf::from("/libraries/org/example/valid.jar")
    );
}

#[test]
fn content_filename_traversal_is_rejected() {
    let mut version = sample_version("content-pack", "content-pack-1");
    version.files.push(ModrinthFile {
        hashes: HashMap::new(),
        url: "https://example.invalid/evil.jar".into(),
        filename: "../../evil.jar".into(),
        primary: true,
        size: 1,
    });

    let primary_file = version.primary_file().unwrap();
    assert!(validate_content_filename(&primary_file.filename).is_err());
    assert!(validate_content_filename("/absolute/evil.jar").is_err());

    version.files[0].filename = "content pack.zip".into();
    let primary_file = version.primary_file().unwrap();
    validate_content_filename(&primary_file.filename).unwrap();
}

#[test]
fn modlist_and_local_jar_path_traversal_is_rejected() {
    let launcher_paths = crate::launcher_paths::LauncherPaths::new("workspace-root");
    let target = ResolutionTarget {
        minecraft_version: "1.21.1".into(),
        mod_loader: ModLoader::Fabric,
    };

    assert!(load_modlist(&launcher_paths, "../Pack").is_err());
    assert!(build_instance_root(&launcher_paths, "../Pack", &target).is_err());
    assert!(local_mod_jar_path(&launcher_paths, "../Pack", "local.jar").is_err());
    assert!(local_mod_jar_path(&launcher_paths, "Pack", "../evil.jar").is_err());
    assert_eq!(
        local_mod_jar_path(&launcher_paths, "Pack", "local.jar").unwrap(),
        PathBuf::from("workspace-root/mod-lists/Pack/local-jars/local.jar")
    );
}

#[test]
fn forge_wrapper_installer_artifact_uses_loader_specific_maven_path() {
    let forge_metadata = LoaderMetadata {
        mod_loader: ModLoader::Forge,
        minecraft_version: "26.1.2".into(),
        loader_version: "64.0.5".into(),
        main_class: "io.github.zekerzhayard.forgewrapper.installer.Main".into(),
        libraries: vec![LoaderLibrary {
            name: "net.minecraftforge:forge:26.1.2-64.0.5:universal".into(),
            url: None,
            download: None,
        }],
        maven_files: Vec::new(),
        jvm_arguments: Vec::new(),
        game_arguments: Vec::new(),
        min_java_version: None,
    };
    let neoforge_metadata = LoaderMetadata {
        mod_loader: ModLoader::NeoForge,
        minecraft_version: "26.1.2".into(),
        loader_version: "26.1.2.29-beta".into(),
        main_class: "io.github.zekerzhayard.forgewrapper.installer.Main".into(),
        libraries: Vec::new(),
        maven_files: Vec::new(),
        jvm_arguments: Vec::new(),
        game_arguments: Vec::new(),
        min_java_version: None,
    };

    let forge_artifact = forge_wrapper_installer_artifact(&forge_metadata)
        .unwrap()
        .expect("Forge should require an installer artifact");
    let neoforge_artifact = forge_wrapper_installer_artifact(&neoforge_metadata)
        .unwrap()
        .expect("NeoForge should require an installer artifact");

    assert_eq!(
        forge_artifact.url,
        "https://maven.minecraftforge.net/net/minecraftforge/forge/26.1.2-64.0.5/forge-26.1.2-64.0.5-installer.jar"
    );
    assert_eq!(
        forge_artifact.relative_path,
        PathBuf::from("net/minecraftforge/forge/26.1.2-64.0.5/forge-26.1.2-64.0.5-installer.jar")
    );
    assert_eq!(
        neoforge_artifact.url,
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/26.1.2.29-beta/neoforge-26.1.2.29-beta-installer.jar"
    );
    assert_eq!(
        neoforge_artifact.relative_path,
        PathBuf::from(
            "net/neoforged/neoforge/26.1.2.29-beta/neoforge-26.1.2.29-beta-installer.jar"
        )
    );
}

#[test]
fn forge_wrapper_installer_artifact_neoforge_1_20_1_uses_forge_coordinate() {
    // NeoForge 1.20.1 (47.x fork era) publishes under net.neoforged:forge with
    // an MC-prefixed version, unlike 1.20.2+ which uses net.neoforged:neoforge.
    let metadata = LoaderMetadata {
        mod_loader: ModLoader::NeoForge,
        minecraft_version: "1.20.1".into(),
        loader_version: "47.1.106".into(),
        main_class: "io.github.zekerzhayard.forgewrapper.installer.Main".into(),
        libraries: Vec::new(),
        maven_files: Vec::new(),
        jvm_arguments: Vec::new(),
        game_arguments: Vec::new(),
        min_java_version: None,
    };

    let artifact = forge_wrapper_installer_artifact(&metadata)
        .unwrap()
        .expect("NeoForge 1.20.1 should require an installer artifact");

    assert_eq!(
        artifact.url,
        "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-installer.jar"
    );
    assert_eq!(
        artifact.relative_path,
        PathBuf::from("net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-installer.jar")
    );
}

#[test]
fn forge_wrapper_installer_artifact_can_use_prism_maven_files() {
    let metadata = LoaderMetadata {
        mod_loader: ModLoader::Forge,
        minecraft_version: "1.18".into(),
        loader_version: "38.0.17".into(),
        main_class: "io.github.zekerzhayard.forgewrapper.installer.Main".into(),
        libraries: vec![LoaderLibrary {
            name: "io.github.zekerzhayard:ForgeWrapper:prism-2025-12-07".into(),
            url: None,
            download: None,
        }],
        maven_files: vec![LoaderLibrary {
            name: "net.minecraftforge:forge:1.18-38.0.17:installer".into(),
            url: None,
            download: None,
        }],
        jvm_arguments: Vec::new(),
        game_arguments: Vec::new(),
        min_java_version: None,
    };

    let artifact = forge_wrapper_installer_artifact(&metadata)
        .unwrap()
        .expect("Forge installer artifact should be resolved from mavenFiles");

    assert_eq!(
        artifact.url,
        "https://maven.minecraftforge.net/net/minecraftforge/forge/1.18-38.0.17/forge-1.18-38.0.17-installer.jar"
    );
}

#[test]
fn instance_root_uses_version_and_loader_suffix() {
    let launcher_paths = crate::launcher_paths::LauncherPaths::new("workspace-root");
    let target = ResolutionTarget {
        minecraft_version: "1.21.1".into(),
        mod_loader: ModLoader::Fabric,
    };

    assert_eq!(
        build_instance_root(&launcher_paths, "Pack", &target).unwrap(),
        PathBuf::from("workspace-root")
            .join("mod-lists")
            .join("Pack")
            .join("instances")
            .join("1.21.1-fabric")
    );
}

#[test]
fn known_launch_placeholders_are_substituted() {
    let placeholders = LaunchPlaceholders::new(
        &PlayerIdentity {
            username: "PlayerOne".into(),
            uuid: "uuid-123".into(),
            access_token: "token-abc".into(),
            user_type: "offline".into(),
            version_type: "Cubic".into(),
        },
        "Pack",
        &ResolutionTarget {
            minecraft_version: "1.21.1".into(),
            mod_loader: ModLoader::Fabric,
        },
        &LaunchDirectories {
            instance_root: Path::new("game-dir"),
            assets_root: Path::new("assets-root"),
            asset_index_id: "1.21",
            library_directory: Path::new("libraries-root"),
            natives_directory: Path::new("natives-root"),
            is_virtual_assets: false,
        },
    );

    let substituted = substitute_known_placeholders(
        "${auth_player_name}:${auth_uuid}:${game_directory}:${version_name}:${resolution_width}x${resolution_height}:${game_assets}",
        &placeholders,
    );

    // The game directory is the working directory of the process: `.`.
    assert_eq!(
        substituted,
        "PlayerOne:uuid-123:.:Pack-1.21.1-fabric:854x480:assets-root"
    );
}

#[test]
fn legacy_virtual_assets_redirect_game_assets_and_session() {
    let placeholders = LaunchPlaceholders::new(
        &PlayerIdentity {
            username: "PlayerOne".into(),
            uuid: "uuid-123".into(),
            access_token: "token-abc".into(),
            user_type: "offline".into(),
            version_type: "Cubic".into(),
        },
        "Pack",
        &ResolutionTarget {
            minecraft_version: "1.6.4".into(),
            mod_loader: ModLoader::Vanilla,
        },
        &LaunchDirectories {
            instance_root: Path::new("game-dir"),
            assets_root: Path::new("assets-root"),
            asset_index_id: "legacy",
            library_directory: Path::new("libraries-root"),
            natives_directory: Path::new("natives-root"),
            is_virtual_assets: true,
        },
    );

    // Legacy: --assetsDir ${game_assets} must point at the materialized virtual
    // tree, and --session ${auth_session} maps to the access token.
    let substituted = substitute_known_placeholders(
        "--assetsDir ${game_assets} --session ${auth_session}",
        &placeholders,
    );
    assert_eq!(
        substituted,
        format!(
            "--assetsDir {} --session token-abc",
            PathBuf::from("assets-root")
                .join("virtual")
                .join("legacy")
                .display()
        )
    );
}

/// download-2 (audit 101): 1.7.10–1.8.9 portano `--userProperties
/// ${user_properties}`, e il Main lo passa a Gson; alla lettera il gioco esce
/// in pochi secondi. Nessun segnaposto deve sopravvivere, nemmeno quelli
/// facoltativi delle versioni nuove (`${clientid}`, `${auth_xuid}`).
#[test]
fn legacy_and_optional_placeholders_never_reach_the_game_literally() {
    let placeholders = LaunchPlaceholders::new(
        &PlayerIdentity {
            username: "PlayerOne".into(),
            uuid: "uuid-123".into(),
            access_token: "token-abc".into(),
            user_type: "offline".into(),
            version_type: "Cubic".into(),
        },
        "Pack",
        &ResolutionTarget {
            minecraft_version: "1.8.9".into(),
            mod_loader: ModLoader::Vanilla,
        },
        &LaunchDirectories {
            instance_root: Path::new("game-dir"),
            assets_root: Path::new("assets-root"),
            asset_index_id: "1.8",
            library_directory: Path::new("libraries-root"),
            natives_directory: Path::new("natives-root"),
            is_virtual_assets: false,
        },
    );
    // `minecraftArguments` di 1.8.9 com'è nel version.json di Mojang.
    let minecraft_arguments = "--username ${auth_player_name} --version ${version_name} --gameDir ${game_directory} --assetsDir ${assets_root} --assetIndex ${assets_index_name} --uuid ${auth_uuid} --accessToken ${auth_access_token} --userProperties ${user_properties} --userType ${user_type}";

    let substituted = minecraft_arguments
        .split_whitespace()
        .map(|argument| substitute_known_placeholders(argument, &placeholders))
        .collect::<Vec<_>>();

    assert!(
        substituted.iter().all(|argument| !argument.contains("${")),
        "{substituted:?}"
    );
    let user_properties = substituted
        .iter()
        .position(|argument| argument == "--userProperties")
        .expect("the option stays");
    assert_eq!(substituted[user_properties + 1], "{}");

    // 1.19+: `--clientId ${clientid} --xuid ${auth_xuid}`.
    assert_eq!(
        substitute_known_placeholders("${clientid}", &placeholders),
        ""
    );
    assert_eq!(
        substitute_known_placeholders("${auth_xuid}", &placeholders),
        ""
    );
}

#[test]
fn minecraft_launch_args_drop_quick_play_options() {
    let args = vec![
        "--username".to_string(),
        "${auth_player_name}".to_string(),
        "--quickPlayPath".to_string(),
        "${quickPlayPath}".to_string(),
        "--quickPlaySingleplayer".to_string(),
        "${quickPlaySingleplayer}".to_string(),
        "--demo".to_string(),
        "--gameDir".to_string(),
        "${game_directory}".to_string(),
    ];

    assert_eq!(
        filter_minecraft_launch_game_arguments(&args),
        vec![
            "--username".to_string(),
            "${auth_player_name}".to_string(),
            "--gameDir".to_string(),
            "${game_directory}".to_string(),
        ]
    );
}

#[test]
fn minecraft_launch_args_drop_options_already_supplied_by_loader() {
    let minecraft_args = vec![
        "--username".to_string(),
        "${auth_player_name}".to_string(),
        "--version".to_string(),
        "${version_name}".to_string(),
        "--gameDir".to_string(),
        "${game_directory}".to_string(),
        "--assetsDir".to_string(),
        "${assets_root}".to_string(),
        "--assetIndex".to_string(),
        "${assets_index_name}".to_string(),
        "--width".to_string(),
        "${resolution_width}".to_string(),
        "--height".to_string(),
        "${resolution_height}".to_string(),
    ];
    let loader_args = vec![
        "--version".to_string(),
        "${version_name}".to_string(),
        "--assetsDir".to_string(),
        "${assets_root}".to_string(),
        "--fml.neoForgeVersion".to_string(),
        "21.11.42".to_string(),
    ];

    assert_eq!(
        merge_minecraft_and_loader_game_arguments(&minecraft_args, loader_args),
        vec![
            "--username".to_string(),
            "${auth_player_name}".to_string(),
            "--gameDir".to_string(),
            "${game_directory}".to_string(),
            "--assetIndex".to_string(),
            "${assets_index_name}".to_string(),
            "--width".to_string(),
            "${resolution_width}".to_string(),
            "--height".to_string(),
            "${resolution_height}".to_string(),
            "--version".to_string(),
            "${version_name}".to_string(),
            "--assetsDir".to_string(),
            "${assets_root}".to_string(),
            "--fml.neoForgeVersion".to_string(),
            "21.11.42".to_string(),
        ]
    );
}

#[test]
fn modded_jvm_args_keep_natives_and_drop_minecraft_classpath_placeholder() {
    let minecraft_args = vec![
        "-Djava.library.path=${natives_directory}".to_string(),
        "-Dminecraft.launcher.brand=${launcher_name}".to_string(),
        "-cp".to_string(),
        "${classpath}".to_string(),
    ];
    let loader_args = vec!["-Dforgewrapper.installer=forge-installer.jar".to_string()];

    assert_eq!(
        merge_minecraft_and_loader_jvm_arguments(&minecraft_args, loader_args),
        vec![
            "-Djava.library.path=${natives_directory}".to_string(),
            "-Dminecraft.launcher.brand=${launcher_name}".to_string(),
            "-Dforgewrapper.installer=forge-installer.jar".to_string(),
        ]
    );
}

#[test]
fn legacy_vanilla_jvm_args_gain_natives_path_when_absent() {
    // Legacy versions (<=1.12.x) use the `minecraftArguments` string, so the
    // downloader yields an EMPTY jvm-args vec (no arguments.jvm block). Without
    // injecting -Djava.library.path the LWJGL2 loader fails with
    // "UnsatisfiedLinkError: no lwjgl(64) in java.library.path".
    let merged = merge_minecraft_and_loader_jvm_arguments(&[], Vec::new());

    assert!(merged.contains(&"-Djava.library.path=${natives_directory}".to_string()));
    // r2-percorsi-non-ascii-e-limiti-windows-2: not `-Dorg.lwjgl.librarypath`. LWJGL 2
    // hands it to `System.load`, which wants an absolute path, and the natives now reach
    // Java relative to the instance; `java.library.path` is what Mojang's launcher passes.
    assert!(!merged
        .iter()
        .any(|argument| argument.starts_with("-Dorg.lwjgl.librarypath=")));
}

/// r2-percorsi-non-ascii-e-limiti-windows-1 e -2: dentro l'istanza i segnaposto dei
/// percorsi sono relativi alla cartella di lavoro, e il nome della mod list, con «:», «;» o
/// un'emoji, non arriva più a Java; gli asset della cache condivisa restano assoluti.
#[test]
fn path_placeholders_inside_the_instance_reach_java_relative() {
    let data_root = PathBuf::from("/data/cubic");
    let instance_root = data_root
        .join("mod-lists")
        .join("🌲 Create: Astral; v2")
        .join("instances")
        .join("1.20.1-forge");
    let assets_root = data_root.join("cache").join("minecraft").join("assets");
    let placeholders = LaunchPlaceholders::new(
        &PlayerIdentity {
            username: "PlayerOne".into(),
            uuid: "uuid-123".into(),
            access_token: "token-abc".into(),
            user_type: "offline".into(),
            version_type: "Cubic".into(),
        },
        "🌲 Create: Astral; v2",
        &ResolutionTarget {
            minecraft_version: "1.20.1".into(),
            mod_loader: ModLoader::Forge,
        },
        &LaunchDirectories {
            instance_root: &instance_root,
            assets_root: &assets_root,
            asset_index_id: "5",
            library_directory: &instance_root.join("libraries"),
            natives_directory: &instance_root.join("natives"),
            is_virtual_assets: false,
        },
    );

    let jvm_and_game_arguments = [
        "-Djava.library.path=${natives_directory}",
        "-DlibraryDirectory=${library_directory}",
        "--gameDir",
        "${game_directory}",
        "--assetsDir",
        "${assets_root}",
    ]
    .map(|argument| substitute_known_placeholders(argument, &placeholders));

    assert_eq!(
        jvm_and_game_arguments,
        [
            "-Djava.library.path=natives".to_string(),
            "-DlibraryDirectory=libraries".to_string(),
            "--gameDir".to_string(),
            ".".to_string(),
            "--assetsDir".to_string(),
            assets_root.display().to_string(),
        ]
    );
}

/// r2-percorsi-non-ascii-e-limiti-windows-2: con Forge si rompeva anche
/// `-Dforgewrapper.installer`; installer e librerie ora sono relativi all'istanza.
#[test]
fn forge_wrapper_properties_point_inside_the_instance_relatively() {
    let instance_root = PathBuf::from("/data/cubic/mod-lists/🌲 Cozy/instances/1.20.1-forge");
    let library_root = instance_root.join("libraries");
    let installer = library_root
        .join("net")
        .join("minecraftforge")
        .join("forge-installer.jar");
    let client_jar = PathBuf::from("/data/cubic/cache/minecraft/versions/1.20.1/1.20.1.jar");

    let arguments = super::runtime::forge_wrapper_jvm_arguments(
        &instance_root,
        &installer,
        &library_root,
        &client_jar,
    );

    assert_eq!(
        arguments,
        [
            format!(
                "-Dforgewrapper.installer={}",
                PathBuf::from("libraries")
                    .join("net")
                    .join("minecraftforge")
                    .join("forge-installer.jar")
                    .display()
            ),
            "-Dforgewrapper.librariesDir=libraries".to_string(),
            format!("-Dforgewrapper.minecraft={}", client_jar.display()),
        ]
    );
}

#[test]
fn modded_classpath_prefers_loader_profile_libraries_over_minecraft_libraries() {
    let minecraft_libraries = vec![
        PathBuf::from(
            "minecraft-libraries/org/apache/logging/log4j/log4j-core/2.8.1/log4j-core-2.8.1.jar",
        ),
        PathBuf::from(
            "minecraft-libraries/org/apache/logging/log4j/log4j-api/2.8.1/log4j-api-2.8.1.jar",
        ),
        PathBuf::from("minecraft-libraries/com/google/guava/guava/21.0/guava-21.0.jar"),
    ];
    let loader_libraries = vec![
        PathBuf::from(
            "instance-libraries/org/apache/logging/log4j/log4j-core/2.15.0/log4j-core-2.15.0.jar",
        ),
        PathBuf::from(
            "instance-libraries/org/apache/logging/log4j/log4j-api/2.15.0/log4j-api-2.15.0.jar",
        ),
    ];
    let client_jar = PathBuf::from("minecraft/client.jar");

    let classpath =
        build_modded_classpath_entries(&minecraft_libraries, loader_libraries, client_jar.clone());

    assert!(!classpath.iter().any(|path| path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "log4j-core-2.8.1.jar" || name == "log4j-api-2.8.1.jar")
        .unwrap_or(false)));
    assert!(classpath.iter().any(|path| path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "log4j-core-2.15.0.jar")
        .unwrap_or(false)));
    assert!(classpath.iter().any(|path| path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "guava-21.0.jar")
        .unwrap_or(false)));
    assert_eq!(classpath.last(), Some(&client_jar));
}

fn sample_version(project_id: &str, version_id: &str) -> ModrinthVersion {
    ModrinthVersion {
        id: version_id.into(),
        project_id: project_id.into(),
        version_number: "1.0.0".into(),
        name: format!("{project_id} {version_id}"),
        game_versions: vec!["1.21.5".into()],
        loaders: vec!["fabric".into()],
        version_type: "release".into(),
        dependencies: Vec::new(),
        files: Vec::new(),
        date_published: "2024-08-15T10:00:00.000Z".into(),
    }
}

#[test]
fn java_predicates_extract_minimum_requirement() {
    assert_eq!(minimum_java_version_for_predicate(">=22"), Some(22));
    assert_eq!(minimum_java_version_for_predicate(">21"), Some(22));
    assert_eq!(minimum_java_version_for_predicate("21"), Some(21));
    assert_eq!(
        embedded_min_java_requirement(&EmbeddedFabricRequirements {
            root_entry: None,
            entries: vec![
                EmbeddedFabricRequirementSet {
                    minecraft_predicates: Vec::new(),
                    java_predicates: vec![">=21".into()],
                },
                EmbeddedFabricRequirementSet {
                    minecraft_predicates: Vec::new(),
                    java_predicates: vec![">=22".into(), ">=23".into()],
                },
            ],
        }),
        Some(22)
    );
}

#[test]
fn tilde_minecraft_predicates_match_target_patch_line() {
    assert!(minecraft_version_predicate_matches("~1.21.6", "1.21.6"));
    assert!(minecraft_version_predicate_matches("~1.21.6", "1.21.9"));
    assert!(!minecraft_version_predicate_matches("~1.21.6", "1.22.0"));
}

#[test]
fn fabric_dependency_predicates_match_semver_ranges() {
    assert!(fabric_dependency_predicates_match(
        &["<7.0.0".into()],
        "6.2.9"
    ));
    assert!(fabric_dependency_predicates_match(
        &[">=17.0.6".into()],
        "17.0.6"
    ));
    assert!(!fabric_dependency_predicates_match(
        &["<3.0.0".into()],
        "3.0.0"
    ));
    assert!(fabric_dependency_predicates_match(
        &["<1.8.0".into()],
        "1.8.0-beta.4+mc1.21.1"
    ));
    assert!(!fabric_dependency_predicates_match(
        &[">=1.8.0".into()],
        "1.8.0-beta.4+mc1.21.1"
    ));
}

// ── Automation entry-point (env → full verification) ────────────────────────

fn verification_result(state: &str, success: bool) -> LaunchVerificationResult {
    LaunchVerificationResult {
        started: state != "launch_failed",
        success,
        state: state.to_string(),
        pid: None,
        launch_log_dir: None,
        duration_ms: 0,
        failure_kind: None,
        failure_summary: None,
        minecraft_log_tail: Vec::new(),
    }
}

#[test]
fn automation_env_parses_full_request_with_defaults() {
    // (a) A complete JSON deserializes into a full LaunchVerificationRequest,
    // preserving every observation field — none dropped (the old bug routed
    // through into_launch_request and discarded timeout/successAfter/terminate*).
    let json = r#"{
        "modlistName": "Pack",
        "minecraftVersion": "1.21.1",
        "modLoader": "fabric",
        "timeoutSeconds": 90,
        "successAfterSeconds": 12,
        "terminateOnSuccess": false,
        "terminateOnTimeout": false
    }"#;
    let request: LaunchVerificationRequest =
        serde_json::from_str(json).expect("full request should parse");
    assert_eq!(request.modlist_name, "Pack");
    assert_eq!(request.minecraft_version, "1.21.1");
    assert_eq!(request.mod_loader, "fabric");
    assert_eq!(request.timeout_seconds, 90);
    assert_eq!(request.success_after_seconds, 12);
    assert!(!request.terminate_on_success);
    assert!(!request.terminate_on_timeout);
}

#[test]
fn automation_env_minimal_request_uses_defaults() {
    // (b) A minimal request (only the three required fields) still works: the
    // optional observation fields fall back to the IPC defaults.
    let json = r#"{"modlistName":"Pack","minecraftVersion":"1.21.1","modLoader":"vanilla"}"#;
    let request: LaunchVerificationRequest =
        serde_json::from_str(json).expect("minimal request should parse");
    assert_eq!(request.timeout_seconds, 45);
    assert_eq!(request.success_after_seconds, 15);
    assert!(request.terminate_on_success);
    assert!(request.terminate_on_timeout);
}

#[test]
fn automation_exit_always_exits_on_conclusion_when_requested() {
    // (c) With _EXIT enabled, the process exits on BOTH success and failure —
    // no leaked process. Success -> 0, any non-success/error -> 1.
    let ok = verification_result("running", true);
    assert_eq!(automation_exit_code(true, Some(&ok)), Some(0));

    for state in ["timed_out", "crashed", "exited", "launch_failed"] {
        let failed = verification_result(state, false);
        assert_eq!(
            automation_exit_code(true, Some(&failed)),
            Some(1),
            "state {state} must exit with code 1"
        );
    }
    // Verification errored entirely (None) -> still exits with 1.
    assert_eq!(automation_exit_code(true, None), Some(1));

    // Without _EXIT, never exits regardless of outcome.
    assert_eq!(automation_exit_code(false, Some(&ok)), None);
    assert_eq!(automation_exit_code(false, None), None);
}

#[test]
fn automation_env_does_not_silently_drop_fields() {
    // (d) Guard against regressing to into_launch_request(): a request whose
    // observation fields differ from the defaults must round-trip those exact
    // values, proving they reach run_launch_verification unmodified.
    let request = LaunchVerificationRequest {
        modlist_name: "Pack".into(),
        minecraft_version: "1.20.1".into(),
        mod_loader: "neoforge".into(),
        resolved_versions: Some(HashMap::from([(
            "sodium".to_string(),
            "abc123".to_string(),
        )])),
        resolved_content: Some(HashMap::from([(
            "resourcepack".to_string(),
            HashMap::from([("fresh-animations".to_string(), "xN57JJts".to_string())]),
        )])),
        timeout_seconds: 30,
        success_after_seconds: 10,
        terminate_on_success: true,
        terminate_on_timeout: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: LaunchVerificationRequest = serde_json::from_str(&json).expect("round-trip");
    assert_eq!(back, request);
    assert_eq!(back.success_after_seconds, 10);
    assert!(!back.terminate_on_timeout);
    // The two maps have to survive `into_launch_request` too, or an automated
    // run could never exercise the pre-resolved path — for the mods or for the
    // packs, whose map is the only way a popup answer reaches the install
    // (D58).
    let launch_request = back.into_launch_request();
    assert_eq!(
        launch_request.resolved_versions,
        Some(HashMap::from([(
            "sodium".to_string(),
            "abc123".to_string()
        )]))
    );
    assert_eq!(
        launch_request.resolved_content,
        Some(HashMap::from([(
            "resourcepack".to_string(),
            HashMap::from([("fresh-animations".to_string(), "xN57JJts".to_string())]),
        )]))
    );
}

#[test]
fn a_launch_request_without_a_version_map_is_the_request_of_today() {
    // Every existing caller — the frontend and `scripts/launch-harness/` —
    // sends these three fields and nothing else. The pipeline branches on
    // `resolved_versions` being absent, so an accidental `#[serde(deny_...)]`
    // or a non-optional field would break all of them at once.
    let request: LaunchRequest = serde_json::from_str(
        r#"{"modlistName":"Pack","minecraftVersion":"1.20.1","modLoader":"forge"}"#,
    )
    .expect("today's request must still deserialize");

    assert_eq!(request.resolved_versions, None);

    let with_map: LaunchRequest = serde_json::from_str(
        r#"{"modlistName":"Pack","minecraftVersion":"1.20.1","modLoader":"forge",
             "resolvedVersions":{"sodium":"abc123"}}"#,
    )
    .expect("a request carrying the map must deserialize");

    assert_eq!(
        with_map.resolved_versions,
        Some(HashMap::from([(
            "sodium".to_string(),
            "abc123".to_string()
        )]))
    );
}

/// D-d (onda 8a): offline e nessun runtime della major richiesta né di una
/// più alta è un errore che nomina la major, la cartella dei runtime e la
/// scelta dell'account — non una richiesta ad Adoptium. Online la stessa
/// situazione resta un download (`Ok(None)`).
///
/// D158: offline, una Java più nuova installata basta: si usa la più vicina
/// superiore, e il chiamante avvisa della sostituzione.
#[test]
fn offline_with_no_runtime_of_the_required_major_or_newer_is_a_clear_error() {
    let runtimes_dir = Path::new("/launcher/java-runtimes");
    let java = |version: u32| JavaInstallation {
        path: PathBuf::from(format!("/launcher/java-runtimes/java-{version}/bin/java")),
        version,
        auto_detected: true,
        architecture: "x64".into(),
        source: JavaInstallationSource::LauncherManaged,
    };
    let installed = vec![java(25), java(21)];

    let error = installed_java_for_launch(&installed, 26, true, runtimes_dir)
        .expect_err("offline, nothing from Java 26 up must stop the launch, not reach Adoptium");
    let message = error.to_string();
    assert!(message.contains("Java 26"), "{message}");
    assert!(message.contains("/launcher/java-runtimes"), "{message}");
    assert!(message.contains("plays offline"), "{message}");

    // Online the exact major is downloaded instead.
    assert_eq!(
        installed_java_for_launch(&installed, 17, false, runtimes_dir).expect("online"),
        None
    );
    // Offline, exact major installed: that one.
    assert_eq!(
        installed_java_for_launch(&installed, 21, true, runtimes_dir)
            .expect("offline, installed")
            .map(|java| java.version),
        Some(21)
    );
    // D158: offline, 17 missing → the nearest higher installed, 21, not 25.
    assert_eq!(
        installed_java_for_launch(&installed, 17, true, runtimes_dir)
            .expect("offline, newer installed")
            .map(|java| java.version),
        Some(21)
    );
}

/// download-4 (audit 101): una Java di un'altra architettura non entra nella
/// scelta, nemmeno quando è la sola della major giusta o viene dal PATH.
#[test]
fn java_of_another_architecture_is_left_out_of_the_choice() {
    let host = crate::java_runtime::host_java_architecture();
    let foreign = if host == "x86" { "x64" } else { "x86" };
    let installation = |path: &str, architecture: &str, source| JavaInstallation {
        path: PathBuf::from(path),
        version: 8,
        auto_detected: true,
        architecture: architecture.into(),
        source,
    };
    let (usable, skipped) = split_installations_by_host_architecture(vec![
        installation("/usr/bin/java", foreign, JavaInstallationSource::SystemPath),
        installation(
            "/launcher/java-runtimes/java-8/bin/java",
            &host,
            JavaInstallationSource::LauncherManaged,
        ),
    ]);

    assert_eq!(
        usable
            .iter()
            .map(|java| java.path.clone())
            .collect::<Vec<_>>(),
        vec![PathBuf::from("/launcher/java-runtimes/java-8/bin/java")]
    );
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].architecture, foreign);
}
