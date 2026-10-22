use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaInstallation {
    pub path: PathBuf,
    pub version: u32,
    pub auto_detected: bool,
    pub architecture: String,
    pub source: JavaInstallationSource,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum JavaInstallationSource {
    SystemPath,
    PlatformDirectory,
    LauncherManaged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaBinaryCandidate {
    pub path: PathBuf,
    pub source: JavaInstallationSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaProbe {
    pub version: u32,
    pub architecture: String,
}

pub trait JavaBinaryInspector {
    fn inspect(&self, path: &Path) -> Result<Option<JavaProbe>>;
}

pub struct CommandJavaBinaryInspector;

impl JavaBinaryInspector for CommandJavaBinaryInspector {
    fn inspect(&self, path: &Path) -> Result<Option<JavaProbe>> {
        if !path.exists() {
            return Ok(None);
        }

        // `-XshowSettings:properties` aggiunge `os.arch = …`, l'unico dato
        // esatto sull'architettura: `-version` da solo dice «64-Bit» anche su
        // aarch64 e niente su una JVM a 32 bit (download-4). Una JVM che non
        // conosce l'opzione non stampa la versione, e allora si riprova con
        // `-version` soltanto.
        for args in [
            &["-XshowSettings:properties", "-version"][..],
            &["-version"][..],
        ] {
            let Some(combined_output) = run_java_for_output(path, args) else {
                return Ok(None); // binary not executable or missing
            };
            if let Some(version) = parse_java_major_version(&combined_output) {
                return Ok(Some(JavaProbe {
                    version,
                    architecture: parse_java_architecture(&combined_output),
                }));
            }
        }

        Ok(None)
    }
}

/// stdout e stderr di `java <args>`, insieme; `None` se il binario non parte.
fn run_java_for_output(path: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.args(args);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd.output().ok()?;
    Some(format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

pub fn discover_java_installations(
    launcher_java_runtimes_dir: &Path,
) -> Result<Vec<JavaInstallation>> {
    let candidates = discover_java_binary_candidates(launcher_java_runtimes_dir)?;
    inspect_java_binary_candidates(&candidates, &CommandJavaBinaryInspector)
}

pub fn discover_java_binary_candidates(
    launcher_java_runtimes_dir: &Path,
) -> Result<Vec<JavaBinaryCandidate>> {
    let mut candidates = Vec::new();

    if let Some(path_value) = env::var_os("PATH") {
        let path_entries = env::split_paths(&path_value).collect::<Vec<_>>();
        candidates.extend(candidates_from_path_entries(&path_entries));
    }

    candidates.extend(candidates_from_java_home_root(
        &platform_java_root_directory(),
        JavaInstallationSource::PlatformDirectory,
    )?);
    candidates.extend(candidates_from_java_home_root(
        launcher_java_runtimes_dir,
        JavaInstallationSource::LauncherManaged,
    )?);

    deduplicate_candidates(candidates)
}

pub fn inspect_java_binary_candidates(
    candidates: &[JavaBinaryCandidate],
    inspector: &impl JavaBinaryInspector,
) -> Result<Vec<JavaInstallation>> {
    let mut installations = Vec::new();

    for candidate in candidates {
        if let Some(probe) = inspector.inspect(&candidate.path)? {
            installations.push(JavaInstallation {
                path: candidate.path.clone(),
                version: probe.version,
                auto_detected: true,
                architecture: probe.architecture,
                source: candidate.source,
            });
        }
    }

    installations.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then(left.version.cmp(&right.version))
            .then(left.path.cmp(&right.path))
    });

    Ok(installations)
}

pub fn persist_java_installations(
    connection: &Connection,
    installations: &[JavaInstallation],
) -> Result<()> {
    for installation in installations {
        connection.execute(
            r#"
            INSERT INTO java_installations (
                path,
                version,
                auto_detected,
                architecture
            ) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(path) DO UPDATE SET
                version = excluded.version,
                auto_detected = excluded.auto_detected,
                architecture = excluded.architecture
            "#,
            params![
                installation.path.to_string_lossy().to_string(),
                installation.version,
                installation.auto_detected,
                installation.architecture,
            ],
        )?;
    }

    Ok(())
}

pub fn select_java_for_minecraft(
    installations: &[JavaInstallation],
    minecraft_version: &str,
) -> Option<JavaInstallation> {
    let required_version = required_java_for_minecraft_version(None, minecraft_version);

    select_java_for_requirement(installations, required_version)
}

pub fn select_java_for_requirement(
    installations: &[JavaInstallation],
    required_version: u32,
) -> Option<JavaInstallation> {
    // Nearest-higher policy: prefer an exact major, else fall back to the
    // lowest installed major that is still >= the requirement. Adoptium no
    // longer ships EOL majors (e.g. Java 16), so a launch requiring one must be
    // able to run on the next available major. Picking the *lowest* qualifying
    // major keeps the runtime as close as possible to what the game expects.
    installations
        .iter()
        .filter(|installation| installation.version >= required_version)
        .cloned()
        .min_by(|left, right| {
            left.version
                .cmp(&right.version)
                .then(left.source.cmp(&right.source))
                .then(left.path.cmp(&right.path))
        })
}

/// Select only an *exact* major match, never a higher one. The launch asks
/// this before deciding to download a Java runtime from Adoptium, so a legacy
/// launch that genuinely needs an EOL-adjacent major (e.g. Java 8) still gets
/// the exact download instead of silently reusing a newer installed runtime
/// (offline, where nothing is downloaded, D158 falls back to a newer one).
pub fn select_exact_java_for_requirement(
    installations: &[JavaInstallation],
    required_version: u32,
) -> Option<JavaInstallation> {
    installations
        .iter()
        .filter(|installation| installation.version == required_version)
        .cloned()
        .min_by(|left, right| {
            left.source
                .cmp(&right.source)
                .then(left.path.cmp(&right.path))
        })
}

/// La major di Java che una versione di Minecraft chiede: quella che il suo
/// version.json dichiara (`javaVersion.majorVersion`), poi la tabella sul nome
/// della versione, poi 8.
///
/// Il version.json prima di tutto (download-3): la tabella capisce solo i
/// nomi numerici, e snapshot, pre-release e release candidate («25w14a»,
/// «1.21-pre1») facevano fallire il lancio con `invalid Minecraft version`.
/// Mai un errore: un nome che la tabella non capisce vale 8, come un
/// version.json senza `javaVersion` (le versioni vecchie).
pub fn required_java_for_minecraft_version(
    declared_major: Option<u32>,
    minecraft_version: &str,
) -> u32 {
    declared_major
        .filter(|major| *major > 0)
        .or_else(|| required_java_version_for_minecraft(minecraft_version).ok())
        .unwrap_or(8)
}

pub fn required_java_version_for_minecraft(minecraft_version: &str) -> Result<u32> {
    let parts = minecraft_version
        .split('.')
        .map(str::trim)
        .map(|segment| segment.parse::<u32>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("invalid Minecraft version '{minecraft_version}'"))?;

    if parts.len() < 2 {
        bail!("Minecraft version '{minecraft_version}' must contain at least major.minor");
    }

    let major = parts[0];
    let minor = parts[1];
    let patch = parts.get(2).copied().unwrap_or(0);

    // New format from 2026: year.drop (e.g. 26.1 = first drop of 2026).
    // 26.1+ requires Java 25.
    if major >= 26 {
        return Ok(25);
    }

    if major != 1 {
        bail!("unsupported Minecraft major version '{minecraft_version}'");
    }

    let required = match minor {
        0..=16 => 8,
        17 => 16,
        18 | 19 => 17,
        20 if patch <= 4 => 17,
        21..=255 => 21,
        _ => 21,
    };

    Ok(required)
}

pub fn parse_java_major_version(output: &str) -> Option<u32> {
    // La riga di `-version` (`openjdk version "17.0.12"`) prima di tutto: con
    // `-XshowSettings` l'output contiene anche le proprietà, e un loro valore
    // tra virgolette non deve passare per la versione.
    let version_token = output
        .lines()
        .filter(|line| line.contains(" version \""))
        .chain(output.lines())
        .flat_map(|line| line.split(|character: char| character.is_whitespace()))
        .find(|token| token.len() > 2 && token.starts_with('"') && token.ends_with('"'))?
        .trim_matches('"');

    let mut segments = version_token.split('.');
    let first = leading_number(segments.next()?)?;

    if first == 1 {
        leading_number(segments.next()?)
    } else {
        Some(first)
    }
}

/// Le cifre iniziali di un segmento di versione: le build «early access» e
/// interne scrivono `25-ea` o `17-internal` senza minor, e il suffisso non
/// deve rendere la Java invisibile (r2-percorsi-non-ascii-e-limiti-windows-6).
fn leading_number(segment: &str) -> Option<u32> {
    let end = segment
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(segment.len());
    segment[..end].parse().ok()
}

/// L'architettura di una JVM: `x64`, `arm64`, `x86`, oppure il nome di
/// `os.arch` per le altre.
///
/// Prima `os.arch` di `-XshowSettings:properties`, che è esatto; senza, i
/// segni di `-version`: aarch64/arm64, poi «64-Bit», e altrimenti una JVM a
/// 32 bit, che è proprio quella che non lo scrive (download-4). Prima tutto
/// quello che non era arm diventava `x64`, e una Java 8 a 32 bit nel PATH
/// vinceva su quella a 64 bit del launcher, per poi non partire con `-Xmx4096M`.
pub fn parse_java_architecture(output: &str) -> String {
    let os_arch = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("os.arch")?
            .trim_start()
            .strip_prefix('=')
            .map(str::trim)
            .filter(|value| !value.is_empty())
    });
    if let Some(os_arch) = os_arch {
        return normalize_java_architecture(os_arch);
    }

    let normalized = output.to_ascii_lowercase();

    if normalized.contains("aarch64") || normalized.contains("arm64") {
        "arm64".to_string()
    } else if normalized.contains("64-bit") {
        "x64".to_string()
    } else {
        "x86".to_string()
    }
}

/// `os.arch` di Java e `std::env::consts::ARCH` di Rust con gli stessi nomi.
fn normalize_java_architecture(architecture: &str) -> String {
    match architecture.to_ascii_lowercase().as_str() {
        "amd64" | "x86_64" | "x64" => "x64".to_string(),
        "aarch64" | "arm64" => "arm64".to_string(),
        "x86" | "i386" | "i486" | "i586" | "i686" => "x86".to_string(),
        other => other.to_string(),
    }
}

/// L'architettura di questo computer, nei nomi di [`parse_java_architecture`].
pub fn host_java_architecture() -> String {
    normalize_java_architecture(std::env::consts::ARCH)
}

/// Se una Java di questa architettura è adatta al lancio su questo computer.
/// Una JVM a 32 bit su un sistema a 64 bit parte, ma non con la memoria che
/// il lancio le chiede: si scarta, come ogni architettura diversa.
pub fn java_architecture_matches_host(architecture: &str) -> bool {
    architecture == host_java_architecture()
}

pub fn candidates_from_path_entries(path_entries: &[PathBuf]) -> Vec<JavaBinaryCandidate> {
    path_entries
        .iter()
        .map(|directory| JavaBinaryCandidate {
            path: directory.join(java_binary_name()),
            source: JavaInstallationSource::SystemPath,
        })
        .collect()
}

pub fn candidates_from_java_home_root(
    root: &Path,
    source: JavaInstallationSource,
) -> Result<Vec<JavaBinaryCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut candidates = java_binary_candidates_for_home(root, source);

    // Scan two levels deep to find Adoptium-style layouts:
    // java-runtimes/java-25/jdk-25.0.1+9-jre/bin/java.exe
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed to scan Java root directory {}", root.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect Java root entry inside {}",
                root.display()
            )
        })?;
        let child = entry.path();
        candidates.extend(java_binary_candidates_for_home(&child, source));

        // Second level: scan subdirectories of each child.
        if child.is_dir() {
            if let Ok(grandchildren) = std::fs::read_dir(&child) {
                for gc in grandchildren.flatten() {
                    if gc.path().is_dir() {
                        candidates.extend(java_binary_candidates_for_home(&gc.path(), source));
                    }
                }
            }
        }
    }

    deduplicate_candidates(candidates)
}

fn java_binary_candidates_for_home(
    java_home: &Path,
    source: JavaInstallationSource,
) -> Vec<JavaBinaryCandidate> {
    [
        java_home.join("bin").join(java_binary_name()),
        java_home
            .join("Contents")
            .join("Home")
            .join("bin")
            .join(java_binary_name()),
    ]
    .into_iter()
    .filter(|candidate| candidate.exists())
    .map(|path| JavaBinaryCandidate { path, source })
    .collect()
}

fn deduplicate_candidates(
    candidates: Vec<JavaBinaryCandidate>,
) -> Result<Vec<JavaBinaryCandidate>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();

    for candidate in candidates {
        let key = normalize_candidate_path(&candidate.path)?;
        if seen.insert(key) {
            unique.push(candidate);
        }
    }

    Ok(unique)
}

fn normalize_candidate_path(path: &Path) -> Result<String> {
    if path.exists() {
        Ok(path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?
            .to_string_lossy()
            .to_ascii_lowercase())
    } else {
        Ok(path.to_string_lossy().to_ascii_lowercase())
    }
}

fn platform_java_root_directory() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\Program Files\Java")
    }

    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/usr/lib/jvm")
    }

    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Java/JavaVirtualMachines")
    }
}

fn java_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "java.exe"
    }

    #[cfg(not(target_os = "windows"))]
    {
        "java"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use rusqlite::Connection;

    use crate::database::initialize_database;

    use super::{
        candidates_from_java_home_root, candidates_from_path_entries, host_java_architecture,
        inspect_java_binary_candidates, java_architecture_matches_host, parse_java_architecture,
        parse_java_major_version, persist_java_installations, required_java_for_minecraft_version,
        required_java_version_for_minecraft, select_exact_java_for_requirement,
        select_java_for_minecraft, select_java_for_requirement, JavaBinaryCandidate,
        JavaBinaryInspector, JavaInstallation, JavaInstallationSource, JavaProbe,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-java-runtime-test-{timestamp}"))
    }

    #[derive(Default)]
    struct FakeInspector {
        probes: HashMap<PathBuf, JavaProbe>,
    }

    impl JavaBinaryInspector for FakeInspector {
        fn inspect(&self, path: &Path) -> Result<Option<JavaProbe>> {
            Ok(self.probes.get(path).cloned())
        }
    }

    #[test]
    fn required_java_version_matches_launcher_rules() {
        assert_eq!(required_java_version_for_minecraft("1.16.5").unwrap(), 8);
        assert_eq!(required_java_version_for_minecraft("1.17.1").unwrap(), 16);
        assert_eq!(required_java_version_for_minecraft("1.20.4").unwrap(), 17);
        assert_eq!(required_java_version_for_minecraft("1.20.5").unwrap(), 21);
        assert_eq!(required_java_version_for_minecraft("1.21.1").unwrap(), 21);
    }

    /// download-3: il version.json decide, la tabella è il ripiego, e un nome
    /// che non è numerico non è più un errore.
    #[test]
    fn required_java_comes_from_the_version_json_first() {
        // Snapshot e release candidate: dal version.json.
        assert_eq!(required_java_for_minecraft_version(Some(21), "25w14a"), 21);
        assert_eq!(
            required_java_for_minecraft_version(Some(17), "1.18-rc3"),
            17
        );
        // Il version.json vince anche sulla tabella.
        assert_eq!(required_java_for_minecraft_version(Some(25), "1.21.1"), 25);
        // Senza `javaVersion`: la tabella, e per un nome che non capisce, 8.
        assert_eq!(required_java_for_minecraft_version(None, "1.20.4"), 17);
        assert_eq!(required_java_for_minecraft_version(None, "25w14a"), 8);
        assert_eq!(required_java_for_minecraft_version(None, "1.21-pre1"), 8);
        assert_eq!(required_java_for_minecraft_version(Some(0), "1.16.5"), 8);
    }

    #[test]
    fn parses_legacy_and_modern_java_versions() {
        assert_eq!(
            parse_java_major_version("java version \"1.8.0_402\""),
            Some(8)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"17.0.12\" 2024-07-16"),
            Some(17)
        );
        assert_eq!(
            parse_java_major_version("java version \"21.0.9\" 2025-10-21 LTS"),
            Some(21)
        );
    }

    #[test]
    fn parses_java_architecture_markers() {
        assert_eq!(
            parse_java_architecture("OpenJDK Runtime Environment aarch64"),
            "arm64"
        );
        assert_eq!(
            parse_java_architecture("Java HotSpot(TM) 64-Bit Server VM"),
            "x64"
        );
        // download-4: la Java 8 Oracle a 32 bit non scrive «64-Bit», e non è x64.
        assert_eq!(
            parse_java_architecture(
                "java version \"1.8.0_381\"\nJava(TM) SE Runtime Environment (build 1.8.0_381-b09)\nJava HotSpot(TM) Client VM (build 25.381-b09, mixed mode, sharing)"
            ),
            "x86"
        );
    }

    /// `os.arch` di `-XshowSettings:properties` vince sui segni di `-version`:
    /// una JVM aarch64 scrive «64-Bit» come una x64.
    #[test]
    fn os_arch_property_decides_the_architecture() {
        let output = |os_arch: &str| {
            format!(
                "Property settings:\n    file.encoding = UTF-8\n    os.arch = {os_arch}\n    os.name = Linux\n\nopenjdk version \"17.0.12\" 2024-07-16\nOpenJDK 64-Bit Server VM (build 17.0.12+7, mixed mode)\n"
            )
        };
        assert_eq!(parse_java_architecture(&output("amd64")), "x64");
        assert_eq!(parse_java_architecture(&output("x86_64")), "x64");
        assert_eq!(parse_java_architecture(&output("aarch64")), "arm64");
        assert_eq!(parse_java_architecture(&output("x86")), "x86");
        assert_eq!(parse_java_architecture(&output("riscv64")), "riscv64");
        assert_eq!(parse_java_major_version(&output("amd64")), Some(17));
    }

    #[test]
    fn host_architecture_accepts_only_its_own_runtimes() {
        let host = host_java_architecture();
        assert!(java_architecture_matches_host(&host));
        if cfg!(target_arch = "x86_64") {
            assert_eq!(host, "x64");
            assert!(!java_architecture_matches_host("x86"));
            assert!(!java_architecture_matches_host("arm64"));
        }
        if cfg!(target_arch = "aarch64") {
            assert_eq!(host, "arm64");
            assert!(!java_architecture_matches_host("x64"));
        }
    }

    /// r2-percorsi-non-ascii-e-limiti-windows-6: le build «early access» e
    /// interne non hanno il minor, e prima non venivano mai rilevate.
    #[test]
    fn parses_early_access_and_internal_java_versions() {
        assert_eq!(
            parse_java_major_version("openjdk version \"25-ea\" 2025-09-16"),
            Some(25)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"17-internal\" 2021-09-14"),
            Some(17)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"1.8.0-internal\""),
            Some(8)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"21.0.2-ea\" 2024-01-16"),
            Some(21)
        );
        assert_eq!(parse_java_major_version("openjdk version \"ea\""), None);
    }

    #[test]
    fn candidates_from_path_entries_append_java_binary_name() {
        let candidates = candidates_from_path_entries(&[
            PathBuf::from("C:/Java/bin"),
            PathBuf::from("D:/Tools/java/bin"),
        ]);

        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .all(|candidate| candidate.source == JavaInstallationSource::SystemPath));
        assert!(candidates[0].path.to_string_lossy().contains("java"));
    }

    #[test]
    fn candidates_from_java_home_root_detects_bin_layouts() {
        let root_dir = unique_test_root();
        let launcher_managed_root = root_dir.join("java-runtimes");
        let standard_home = launcher_managed_root.join("java-17");
        let mac_home = launcher_managed_root.join("zulu-21.jdk");

        fs::create_dir_all(standard_home.join("bin")).unwrap();
        fs::create_dir_all(mac_home.join("Contents").join("Home").join("bin")).unwrap();
        fs::write(
            standard_home.join("bin").join(super::java_binary_name()),
            b"java",
        )
        .unwrap();
        fs::write(
            mac_home
                .join("Contents")
                .join("Home")
                .join("bin")
                .join(super::java_binary_name()),
            b"java",
        )
        .unwrap();

        let candidates = candidates_from_java_home_root(
            &launcher_managed_root,
            JavaInstallationSource::LauncherManaged,
        )
        .unwrap();

        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| {
            candidate.source == JavaInstallationSource::LauncherManaged && candidate.path.exists()
        }));

        fs::remove_dir_all(&root_dir).unwrap();
    }

    #[test]
    fn inspect_candidates_and_select_matching_runtime() {
        let system_path = PathBuf::from("C:/Java/system/bin/java.exe");
        let managed_path = PathBuf::from("C:/Launcher/java-runtimes/java-17/bin/java.exe");
        let candidates = vec![
            JavaBinaryCandidate {
                path: managed_path.clone(),
                source: JavaInstallationSource::LauncherManaged,
            },
            JavaBinaryCandidate {
                path: system_path.clone(),
                source: JavaInstallationSource::SystemPath,
            },
        ];
        let inspector = FakeInspector {
            probes: HashMap::from([
                (
                    managed_path,
                    JavaProbe {
                        version: 17,
                        architecture: "x64".into(),
                    },
                ),
                (
                    system_path.clone(),
                    JavaProbe {
                        version: 17,
                        architecture: "x64".into(),
                    },
                ),
            ]),
        };

        let installations = inspect_java_binary_candidates(&candidates, &inspector).unwrap();
        let selected = select_java_for_minecraft(&installations, "1.20.4")
            .expect("matching installation should exist");

        assert_eq!(selected.path, system_path);
        assert_eq!(selected.version, 17);
    }

    #[test]
    fn explicit_java_requirement_prefers_matching_runtime() {
        let installations = vec![
            JavaInstallation {
                path: PathBuf::from("C:/Java/jdk-21/bin/java.exe"),
                version: 21,
                auto_detected: true,
                architecture: "x64".into(),
                source: JavaInstallationSource::SystemPath,
            },
            JavaInstallation {
                path: PathBuf::from("C:/Launcher/java-runtimes/java-22/bin/java.exe"),
                version: 22,
                auto_detected: true,
                architecture: "x64".into(),
                source: JavaInstallationSource::LauncherManaged,
            },
        ];

        let selected =
            select_java_for_requirement(&installations, 22).expect("Java 22 should be selected");

        assert_eq!(selected.version, 22);
    }

    #[test]
    fn requirement_falls_back_to_nearest_higher_when_exact_missing() {
        // Goal decision: exact-major is no longer mandatory. Adoptium drops EOL
        // majors (e.g. 16), so a requirement for a missing major must resolve to
        // the lowest installed major that is still >= the requirement.
        let installations = vec![JavaInstallation {
            path: PathBuf::from("C:/Java/jdk-21/bin/java.exe"),
            version: 21,
            auto_detected: true,
            architecture: "x64".into(),
            source: JavaInstallationSource::SystemPath,
        }];

        let selected = select_java_for_requirement(&installations, 17)
            .expect("nearest-higher runtime should be selected");
        assert_eq!(selected.version, 21);

        // The exact gate, by contrast, must still reject a newer-only set so the
        // download path can fetch the precise major when it is available.
        assert!(select_exact_java_for_requirement(&installations, 17).is_none());
    }

    #[test]
    fn requirement_prefers_exact_over_higher_and_picks_lowest_qualifying() {
        let installations = vec![
            JavaInstallation {
                path: PathBuf::from("C:/Java/jdk-17/bin/java.exe"),
                version: 17,
                auto_detected: true,
                architecture: "x64".into(),
                source: JavaInstallationSource::LauncherManaged,
            },
            JavaInstallation {
                path: PathBuf::from("C:/Java/jdk-21/bin/java.exe"),
                version: 21,
                auto_detected: true,
                architecture: "x64".into(),
                source: JavaInstallationSource::SystemPath,
            },
        ];

        // Exact major present -> use it, not the higher one.
        assert_eq!(
            select_java_for_requirement(&installations, 17)
                .expect("exact runtime should be selected")
                .version,
            17
        );
        // Requirement below both -> lowest qualifying major (nearest-higher).
        assert_eq!(
            select_java_for_requirement(&installations, 16)
                .expect("nearest-higher runtime should be selected")
                .version,
            17
        );
    }

    #[test]
    fn persist_java_installations_upserts_rows() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");

        initialize_database(&database_path).unwrap();
        let connection = Connection::open(&database_path).unwrap();

        persist_java_installations(
            &connection,
            &[JavaInstallation {
                path: PathBuf::from("C:/Java/jdk-21/bin/java.exe"),
                version: 21,
                auto_detected: true,
                architecture: "x64".into(),
                source: JavaInstallationSource::PlatformDirectory,
            }],
        )
        .unwrap();

        let row = connection
            .query_row(
                "SELECT version, auto_detected, architecture FROM java_installations WHERE path = ?1",
                ["C:/Java/jdk-21/bin/java.exe"],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0, 21);
        assert!(row.1);
        assert_eq!(row.2, "x64");

        drop(connection);
        fs::remove_dir_all(&root_dir).unwrap();
    }
}
