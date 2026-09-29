use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::path_safety::validate_path_component;

const ADOPTIUM_API_BASE_URL: &str = "https://api.adoptium.net/v3";

#[derive(Debug, Clone)]
pub struct AdoptiumClient {
    http_client: reqwest::Client,
    base_url: String,
}

impl AdoptiumClient {
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::new(),
            base_url: ADOPTIUM_API_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    pub async fn fetch_latest_jre_package(
        &self,
        java_version: u32,
        os: &str,
        architecture: &str,
    ) -> Result<Option<AdoptiumPackage>> {
        let url = build_latest_assets_url(&self.base_url, java_version, os, architecture)?;
        let response = self
            .http_client
            .get(url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Adoptium for Java {} {} {}",
                    java_version, os, architecture
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Adoptium returned an error for Java {} {} {}",
                    java_version, os, architecture
                )
            })?;

        let assets = response
            .json::<Vec<AdoptiumRelease>>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Adoptium response for Java {} {} {}",
                    java_version, os, architecture
                )
            })?;

        Ok(select_latest_package(&assets))
    }

    pub async fn download_package(
        &self,
        package: &AdoptiumPackage,
        destination_path: &Path,
    ) -> Result<()> {
        if let Some(parent) = destination_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create parent directories for {}",
                    destination_path.display()
                )
            })?;
        }

        let bytes = self
            .http_client
            .get(&package.link)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to start Java runtime download from {}",
                    package.link
                )
            })?
            .error_for_status()
            .with_context(|| format!("Java runtime download failed for {}", package.link))?
            .bytes()
            .await
            .with_context(|| {
                format!("failed to read Java runtime archive from {}", package.link)
            })?;

        if bytes.len() as u64 != package.size {
            bail!(
                "Java runtime archive from {} has {} bytes; expected {}",
                package.link,
                bytes.len(),
                package.size
            );
        }
        let actual_checksum = format!("{:x}", Sha256::digest(&bytes));
        if !actual_checksum.eq_ignore_ascii_case(&package.checksum) {
            bail!(
                "Java runtime archive checksum mismatch from {}: expected {}, got {}",
                package.link,
                package.checksum,
                actual_checksum
            );
        }

        crate::atomic_write::write_atomically(destination_path, bytes).with_context(|| {
            format!(
                "failed to publish Java runtime archive to {}",
                destination_path.display()
            )
        })?;

        Ok(())
    }
}

impl Default for AdoptiumClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaRuntimeDownloadPlan {
    pub java_version: u32,
    pub os: String,
    pub architecture: String,
    pub install_dir: PathBuf,
    pub archive_path: PathBuf,
    pub package: AdoptiumPackage,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdoptiumRelease {
    pub binary: AdoptiumBinary,
    pub release_name: Option<String>,
    pub version: Option<AdoptiumVersionData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdoptiumBinary {
    pub architecture: String,
    pub image_type: String,
    pub os: String,
    pub package: AdoptiumPackage,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdoptiumPackage {
    pub name: String,
    pub link: String,
    pub checksum: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdoptiumVersionData {
    pub semver: String,
}

pub fn build_latest_assets_url(
    base_url: &str,
    java_version: u32,
    os: &str,
    architecture: &str,
) -> Result<Url> {
    if java_version == 0 {
        bail!("java_version must be greater than zero");
    }

    let mut url = Url::parse(&format!(
        "{}/assets/latest/{}/hotspot",
        base_url.trim_end_matches('/'),
        java_version
    ))
    .with_context(|| format!("invalid Adoptium base URL '{base_url}'"))?;

    url.query_pairs_mut()
        .append_pair("architecture", architecture)
        .append_pair("image_type", "jre")
        .append_pair("os", os);

    Ok(url)
}

pub fn select_latest_package(releases: &[AdoptiumRelease]) -> Option<AdoptiumPackage> {
    releases
        .first()
        .map(|release| release.binary.package.clone())
}

pub fn plan_runtime_download(
    launcher_java_runtimes_dir: &Path,
    java_version: u32,
    package: AdoptiumPackage,
    os: &str,
    architecture: &str,
) -> Result<JavaRuntimeDownloadPlan> {
    validate_path_component(&package.name)?;

    let install_dir = launcher_java_runtimes_dir.join(format!("java-{}", java_version));
    let archive_path = launcher_java_runtimes_dir.join(&package.name);

    Ok(JavaRuntimeDownloadPlan {
        java_version,
        os: os.to_string(),
        architecture: architecture.to_string(),
        install_dir,
        archive_path,
        package,
    })
}

pub fn host_adoptium_os() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "windows"
    }

    #[cfg(target_os = "linux")]
    {
        "linux"
    }

    #[cfg(target_os = "macos")]
    {
        "mac"
    }
}

pub fn normalize_adoptium_architecture(architecture: &str) -> &'static str {
    match architecture.to_ascii_lowercase().as_str() {
        "aarch64" | "arm64" => "aarch64",
        _ => "x64",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        build_latest_assets_url, normalize_adoptium_architecture, plan_runtime_download,
        select_latest_package, AdoptiumClient, AdoptiumPackage, AdoptiumRelease,
    };
    use sha2::{Digest, Sha256};

    fn scratch_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("download-tests")
            .join(format!("adoptium-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch root should be created");
        root
    }

    fn fixed_server(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("server address").port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request should arrive");
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("header");
            stream.write_all(body).expect("body");
            stream.flush().expect("flush");
        });
        format!("http://127.0.0.1:{port}/runtime.tar.gz")
    }

    fn package(link: String, body: &[u8]) -> AdoptiumPackage {
        AdoptiumPackage {
            name: "runtime.tar.gz".into(),
            link,
            checksum: format!("{:x}", Sha256::digest(body)),
            size: body.len() as u64,
        }
    }

    fn partial_path(path: &Path) -> PathBuf {
        let mut name = path.file_name().expect("target file name").to_os_string();
        name.push(".part");
        path.with_file_name(name)
    }

    fn sample_releases_json() -> &'static str {
        r#"[
          {
            "binary": {
              "architecture": "x64",
              "image_type": "jre",
              "os": "windows",
              "package": {
                "name": "OpenJDK21U-jre_x64_windows_hotspot_21.0.9_10.zip",
                "link": "https://example.invalid/jre-21.zip",
                "checksum": "abc123",
                "size": 123456
              }
            },
            "release_name": "jdk-21.0.9+10",
            "version": {
              "semver": "21.0.9+10"
            }
          },
          {
            "binary": {
              "architecture": "x64",
              "image_type": "jre",
              "os": "windows",
              "package": {
                "name": "OpenJDK21U-jre_x64_windows_hotspot_21.0.8_9.zip",
                "link": "https://example.invalid/jre-21-old.zip",
                "checksum": "def456",
                "size": 120000
              }
            },
            "release_name": "jdk-21.0.8+9",
            "version": {
              "semver": "21.0.8+9"
            }
          }
        ]"#
    }

    #[test]
    fn builds_expected_adoptium_latest_assets_url() {
        let url = build_latest_assets_url("https://api.adoptium.net/v3", 21, "windows", "x64")
            .expect("url should build");

        assert_eq!(
            url.as_str(),
            "https://api.adoptium.net/v3/assets/latest/21/hotspot?architecture=x64&image_type=jre&os=windows"
        );
    }

    #[test]
    fn deserializes_adoptium_release_payload() {
        let releases: Vec<AdoptiumRelease> =
            serde_json::from_str(sample_releases_json()).expect("json should deserialize");

        assert_eq!(releases.len(), 2);
        assert_eq!(
            releases[0].binary.package.name,
            "OpenJDK21U-jre_x64_windows_hotspot_21.0.9_10.zip"
        );
        assert_eq!(releases[0].binary.package.checksum, "abc123");
    }

    #[test]
    fn selects_first_package_from_latest_assets_response() {
        let releases: Vec<AdoptiumRelease> =
            serde_json::from_str(sample_releases_json()).expect("json should deserialize");

        let package = select_latest_package(&releases).expect("package should exist");

        assert_eq!(
            package.name,
            "OpenJDK21U-jre_x64_windows_hotspot_21.0.9_10.zip"
        );
        assert_eq!(package.link, "https://example.invalid/jre-21.zip");
    }

    #[test]
    fn plans_runtime_install_paths_under_launcher_java_runtimes() {
        let package = AdoptiumPackage {
            name: "OpenJDK17U-jre_x64_windows_hotspot_17.0.12_7.zip".into(),
            link: "https://example.invalid/jre-17.zip".into(),
            checksum: "checksum".into(),
            size: 42,
        };

        let plan =
            plan_runtime_download(Path::new("java-runtimes"), 17, package, "windows", "x64")
                .expect("valid package name should produce a download plan");

        assert_eq!(plan.install_dir, Path::new("java-runtimes").join("java-17"));
        assert_eq!(
            plan.archive_path,
            Path::new("java-runtimes").join("OpenJDK17U-jre_x64_windows_hotspot_17.0.12_7.zip")
        );
    }

    #[test]
    fn plan_runtime_download_rejects_traversal_package_name() {
        let package = AdoptiumPackage {
            name: "../../evil.zip".into(),
            link: "https://example.invalid/evil.zip".into(),
            checksum: "checksum".into(),
            size: 42,
        };

        let result =
            plan_runtime_download(Path::new("java-runtimes"), 17, package, "windows", "x64");

        assert!(result.is_err());
    }

    #[test]
    fn normalizes_supported_adoptium_architectures() {
        assert_eq!(normalize_adoptium_architecture("arm64"), "aarch64");
        assert_eq!(normalize_adoptium_architecture("aarch64"), "aarch64");
        assert_eq!(normalize_adoptium_architecture("x86_64"), "x64");
    }
    #[tokio::test]
    async fn runtime_archive_lands_whole_and_replaces_a_stale_partial() {
        let root = scratch_root("complete");
        let destination = root.join("runtime.tar.gz");
        fs::write(partial_path(&destination), b"stale partial").expect("stale partial");
        let body = b"complete runtime archive";
        let package = package(fixed_server(body), body);

        AdoptiumClient::new()
            .download_package(&package, &destination)
            .await
            .expect("runtime download should complete");

        assert_eq!(fs::read(&destination).expect("runtime archive"), body);
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }

    #[tokio::test]
    async fn runtime_archive_with_wrong_checksum_never_replaces_the_previous_file() {
        let root = scratch_root("bad-checksum");
        let destination = root.join("runtime.tar.gz");
        fs::write(&destination, b"previous complete archive").expect("previous archive");
        let body = b"wrong downloaded bytes";
        let mut package = package(fixed_server(body), body);
        package.checksum = "0".repeat(64);

        let result = AdoptiumClient::new()
            .download_package(&package, &destination)
            .await;

        assert!(result.is_err(), "checksum mismatch must be reported");
        assert_eq!(
            fs::read(&destination).expect("previous archive remains"),
            b"previous complete archive"
        );
        assert!(!partial_path(&destination).exists());
        fs::remove_dir_all(root).expect("scratch cleanup");
    }
}
