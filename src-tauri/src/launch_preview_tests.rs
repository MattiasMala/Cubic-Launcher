use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::app_shell::{ShellGlobalSettings, ShellModListOverrides};
use crate::dependencies::DependencyLink;
use crate::modrinth::{DependencyType, ModrinthDependency, ModrinthFile, ModrinthVersion};
use crate::resolver::{ModLoader, ResolutionTarget};

use super::{
    build_instance_root, build_modded_classpath_entries, build_top_level_owner_map,
    contained_loader_library_path, embedded_min_java_requirement,
    fabric_dependency_predicates_match, filter_minecraft_launch_game_arguments,
    forge_wrapper_installer_artifact, load_modlist, local_mod_jar_path,
    maven_artifact_relative_path, merge_minecraft_and_loader_game_arguments,
    merge_minecraft_and_loader_jvm_arguments, minecraft_version_predicate_matches,
    minimum_java_version_for_predicate, parse_mod_loader, relative_loader_library_path,
    substitute_known_placeholders, validate_content_filename, validate_final_fabric_runtime,
    automation_exit_code, detect_modrinth_declared_notices,
    fabric_issue_to_notice, DependencyNoticeKind, EffectiveLaunchSettings,
    EmbeddedFabricModMetadata, EmbeddedFabricRequirementSet, EmbeddedFabricRequirements,
    FabricValidationIssue, LaunchPlaceholders, LaunchVerificationRequest, LaunchVerificationResult,
    OwnedEmbeddedFabricModMetadata, PlayerIdentity,
};
use crate::loader_metadata::{LibraryDownloadArtifact, LoaderLibrary, LoaderMetadata};

fn global_settings() -> ShellGlobalSettings {
    ShellGlobalSettings {
        min_ram_mb: 2048,
        max_ram_mb: 4096,
        custom_jvm_args: "-Dglobal=true".into(),
        profiler_enabled: false,
        cache_only_mode: true,
        wrapper_command: "gamemoderun".into(),
        java_path_override: "/custom/java".into(),
    }
}

fn modlist_overrides() -> ShellModListOverrides {
    ShellModListOverrides {
        modlist_name: Some("Pack".into()),
        min_ram_mb: Some(8192),
        max_ram_mb: None,
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

    assert_eq!(settings.min_ram_mb, 8192);
    assert_eq!(settings.max_ram_mb, 4096);
    assert_eq!(settings.custom_jvm_args, "-Dmodlist=true");
    assert!(settings.cache_only_mode);
    assert_eq!(settings.wrapper_command, Some("mangohud".into()));
    assert_eq!(
        settings.java_path_override,
        Some(PathBuf::from("/custom/java"))
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
        PathBuf::from("game-dir").as_path(),
        PathBuf::from("assets-root").as_path(),
        "1.21",
        PathBuf::from("libraries-root").as_path(),
        PathBuf::from("natives-root").as_path(),
    );

    let substituted = substitute_known_placeholders(
        "${auth_player_name}:${auth_uuid}:${game_directory}:${version_name}:${resolution_width}x${resolution_height}",
        &placeholders,
    );

    assert_eq!(
        substituted,
        "PlayerOne:uuid-123:game-dir:Pack-1.21.1-fabric:854x480"
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
            "-Dorg.lwjgl.librarypath=${natives_directory}".to_string(),
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
        dependencies: Vec::new(),
        files: Vec::new(),
        date_published: "2024-08-15T10:00:00.000Z".into(),
    }
}

fn metadata_entry(
    owner_project_id: &str,
    mod_id: &str,
    version: &str,
) -> OwnedEmbeddedFabricModMetadata {
    OwnedEmbeddedFabricModMetadata {
        owner_project_id: owner_project_id.into(),
        metadata: EmbeddedFabricModMetadata {
            mod_id: mod_id.into(),
            version: version.into(),
            provides: Vec::new(),
            depends: HashMap::new(),
            breaks: HashMap::new(),
        },
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

#[test]
fn owner_map_propagates_transitive_dependency_owners() {
    let parent_versions = vec![sample_version("top-level", "top-level-1")];
    let owner_map = build_top_level_owner_map(
        &parent_versions,
        &[
            DependencyLink {
                parent_mod_id: "top-level".into(),
                dependency_id: "mid".into(),
                specific_version: None,
                jar_filename: "mid.jar".into(),
            },
            DependencyLink {
                parent_mod_id: "mid".into(),
                dependency_id: "leaf".into(),
                specific_version: None,
                jar_filename: "leaf.jar".into(),
            },
        ],
    );

    assert_eq!(
        owner_map.get("leaf"),
        Some(&HashSet::from(["top-level".to_string()]))
    );
}

#[test]
fn final_fabric_validation_excludes_top_level_on_breaks_conflict() {
    let owner_map = HashMap::from([(
        "puzzle-project".to_string(),
        HashSet::from(["puzzle-project".to_string()]),
    )]);
    let mut puzzle = metadata_entry("puzzle-project", "puzzle", "2.3.0");
    puzzle
        .metadata
        .breaks
        .insert("entity_model_features".into(), vec!["<3.0.0".into()]);
    let emf = metadata_entry("emf-project", "entity_model_features", "2.4.1");

    let issues = validate_final_fabric_runtime(&[puzzle, emf], &owner_map);

    assert_eq!(
        issues.get("puzzle-project").map(|issue| issue.reason_code),
        Some("breaks_conflict")
    );
}

#[test]
fn final_fabric_validation_excludes_top_level_on_prerelease_breaks_conflict() {
    let owner_map = HashMap::from([
        (
            "sodium-project".to_string(),
            HashSet::from(["sodium-project".to_string()]),
        ),
        (
            "reeses-project".to_string(),
            HashSet::from(["sodiumoptionsapi-project".to_string()]),
        ),
    ]);
    let mut sodium = metadata_entry("sodium-project", "sodium", "0.6.13+mc1.21.1");
    sodium
        .metadata
        .breaks
        .insert("reeses-sodium-options".into(), vec!["<1.8.0".into()]);
    let reeses = metadata_entry(
        "reeses-project",
        "reeses-sodium-options",
        "1.8.0-beta.4+mc1.21.1",
    );

    let issues = validate_final_fabric_runtime(&[sodium, reeses], &owner_map);

    assert_eq!(
        issues.get("sodium-project").map(|issue| issue.reason_code),
        Some("breaks_conflict")
    );
}

#[test]
fn final_fabric_validation_excludes_top_level_on_missing_dependency() {
    let owner_map = HashMap::from([
        (
            "sodiumoptionsapi-project".to_string(),
            HashSet::from(["sodiumoptionsapi-project".to_string()]),
        ),
        (
            "embedded-helper-project".to_string(),
            HashSet::from(["sodiumoptionsapi-project".to_string()]),
        ),
    ]);
    let mut sodium_options_api =
        metadata_entry("sodiumoptionsapi-project", "sodiumoptionsapi", "1.0.10");
    sodium_options_api
        .metadata
        .depends
        .insert("reeses-sodium-options".into(), vec!["*".into()]);

    let issues = validate_final_fabric_runtime(&[sodium_options_api], &owner_map);

    assert_eq!(
        issues
            .get("sodiumoptionsapi-project")
            .and_then(|issue| issue.dependency_id.as_deref()),
        Some("reeses-sodium-options")
    );
    assert_eq!(
        issues
            .get("sodiumoptionsapi-project")
            .map(|issue| issue.reason_code),
        Some("missing_dependency")
    );
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
}

// ── Informational dependency detection (no auto-management) ──────────────────

fn version_with_required_dep(
    project_id: &str,
    version_id: &str,
    dep_project: &str,
    dep_version_id: Option<&str>,
) -> ModrinthVersion {
    let mut v = sample_version(project_id, version_id);
    v.dependencies.push(ModrinthDependency {
        version_id: dep_version_id.map(|s| s.to_string()),
        project_id: Some(dep_project.to_string()),
        dependency_type: DependencyType::Required,
        file_name: None,
    });
    v
}

#[test]
fn iris_pinned_dep_produces_notice_without_excluding_or_downloading() {
    // Iris (YL57xq9U) requires sodium (AANobbMI) pinned to version_id vf7UgZpC
    // (sodium 0.9.1, not tagged for 26.1). The mod-list has sodium 0.8.9
    // (version id 'sodium-0.8.9') top-level. Under the new design: Iris is NOT
    // excluded, the pin is NOT downloaded — a single informational notice is
    // produced reporting the version mismatch.
    let iris = version_with_required_dep("YL57xq9U", "iris-1.11.2", "AANobbMI", Some("vf7UgZpC"));
    let sodium = sample_version("AANobbMI", "sodium-0.8.9");
    let parent_versions = vec![iris.clone(), sodium.clone()];
    let selected: std::collections::HashMap<String, ModrinthVersion> = parent_versions
        .iter()
        .map(|v| (v.project_id.clone(), v.clone()))
        .collect();
    let mut pin_labels = std::collections::HashMap::new();
    pin_labels.insert("vf7UgZpC".to_string(), "0.9.1".to_string());

    let notices = detect_modrinth_declared_notices(&parent_versions, &selected, &pin_labels);

    assert_eq!(notices.len(), 1, "exactly one notice for the pin mismatch");
    let notice = &notices[0];
    assert_eq!(notice.requiring_project_id, "YL57xq9U");
    assert_eq!(notice.dependency_id, "AANobbMI");
    assert_eq!(notice.kind, DependencyNoticeKind::VersionUnsatisfied);
    assert!(notice.detail.contains("0.9.1"), "reports the declared version");
    // Both mods remain selectable — detection never mutates the selection.
    assert!(selected.contains_key("YL57xq9U") && selected.contains_key("AANobbMI"));
}

#[test]
fn missing_declared_dependency_produces_missing_notice() {
    // A parent requires a project that is NOT in the mod-list at all.
    let parent = version_with_required_dep("parent", "parent-1", "absent-dep", None);
    let parent_versions = vec![parent.clone()];
    let selected: std::collections::HashMap<String, ModrinthVersion> =
        parent_versions.iter().map(|v| (v.project_id.clone(), v.clone())).collect();

    let notices = detect_modrinth_declared_notices(
        &parent_versions,
        &selected,
        &std::collections::HashMap::new(),
    );

    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, DependencyNoticeKind::Missing);
    assert_eq!(notices[0].dependency_id, "absent-dep");
}

#[test]
fn satisfied_declared_dependency_produces_no_notice() {
    // Parent requires sodium project with NO pin; sodium is present. No notice.
    let parent = version_with_required_dep("parent", "parent-1", "AANobbMI", None);
    let sodium = sample_version("AANobbMI", "sodium-0.8.9");
    let parent_versions = vec![parent, sodium];
    let selected: std::collections::HashMap<String, ModrinthVersion> =
        parent_versions.iter().map(|v| (v.project_id.clone(), v.clone())).collect();

    let notices = detect_modrinth_declared_notices(
        &parent_versions,
        &selected,
        &std::collections::HashMap::new(),
    );
    assert!(notices.is_empty(), "present unpinned dependency yields no notice");
}

#[test]
fn rso_embedded_incompatible_version_maps_to_version_unsatisfied_notice() {
    // RSO (Bh37bMuy) embedded fabric.mod.json requires sodium >=0.9.1 while
    // sodium 0.8.9 is present -> the predicate-aware validator yields an
    // incompatible_dependency_version issue, which becomes a
    // "present but older" VersionUnsatisfied notice.
    let issue = FabricValidationIssue {
        reason_code: "incompatible_dependency_version",
        owner_project_id: "Bh37bMuy".into(),
        mod_id: "reeses_sodium_options".into(),
        dependency_id: Some("sodium".into()),
        detail: "embedded metadata requires 'sodium' with a compatible version, but only incompatible versions are present".into(),
    };
    let notice = fabric_issue_to_notice("Bh37bMuy", &issue);
    assert_eq!(notice.kind, DependencyNoticeKind::VersionUnsatisfied);
    assert_eq!(notice.requiring_project_id, "Bh37bMuy");
    assert_eq!(notice.dependency_id, "sodium");

    let missing = FabricValidationIssue {
        reason_code: "missing_dependency",
        owner_project_id: "Bh37bMuy".into(),
        mod_id: "reeses_sodium_options".into(),
        dependency_id: Some("sodium".into()),
        detail: "embedded metadata requires 'sodium', which is missing".into(),
    };
    assert_eq!(fabric_issue_to_notice("Bh37bMuy", &missing).kind, DependencyNoticeKind::Missing);
}
