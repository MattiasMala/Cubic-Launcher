use anyhow::{Context, Result};
use reqwest::Url;
use serde::Deserialize;

use crate::resolver::ModLoader;

const FABRIC_META_BASE_URL: &str = "https://meta.fabricmc.net/v2";
const QUILT_META_BASE_URL: &str = "https://meta.quiltmc.org/v3";
const PRISM_META_BASE_URL: &str = "https://meta.prismlauncher.org/v1";

#[derive(Debug, Clone)]
pub struct LoaderMetadataClient {
    http_client: reqwest::Client,
    fabric_base_url: String,
    quilt_base_url: String,
    prism_base_url: String,
}

impl LoaderMetadataClient {
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::new(),
            fabric_base_url: FABRIC_META_BASE_URL.to_string(),
            quilt_base_url: QUILT_META_BASE_URL.to_string(),
            prism_base_url: PRISM_META_BASE_URL.to_string(),
        }
    }

    pub fn with_base_urls(
        fabric_base_url: impl Into<String>,
        quilt_base_url: impl Into<String>,
        prism_base_url: impl Into<String>,
    ) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            fabric_base_url: fabric_base_url.into(),
            quilt_base_url: quilt_base_url.into(),
            prism_base_url: prism_base_url.into(),
        }
    }

    pub async fn fetch_loader_metadata(
        &self,
        minecraft_version: &str,
        mod_loader: ModLoader,
    ) -> Result<LoaderMetadata> {
        match mod_loader {
            ModLoader::Fabric => self.fetch_fabric_metadata(minecraft_version).await,
            ModLoader::Quilt => self.fetch_quilt_metadata(minecraft_version).await,
            ModLoader::Forge => {
                self.fetch_prism_metadata(minecraft_version, PrismPackageUid::Forge)
                    .await
            }
            ModLoader::NeoForge => {
                self.fetch_prism_metadata(minecraft_version, PrismPackageUid::NeoForge)
                    .await
            }
            ModLoader::Vanilla => Ok(LoaderMetadata {
                mod_loader: ModLoader::Vanilla,
                minecraft_version: minecraft_version.to_string(),
                loader_version: minecraft_version.to_string(),
                // main_class, game_arguments, and jvm_arguments are filled in from
                // MinecraftVersionData inside launch_preview after MC download.
                main_class: String::new(),
                libraries: vec![],
                maven_files: vec![],
                jvm_arguments: vec![],
                game_arguments: vec![],
                min_java_version: None,
            }),
        }
    }

    async fn fetch_fabric_metadata(&self, minecraft_version: &str) -> Result<LoaderMetadata> {
        let versions_url =
            build_fabric_loader_versions_url(&self.fabric_base_url, minecraft_version)?;
        let versions = self
            .http_client
            .get(versions_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Fabric metadata for Minecraft {}",
                    minecraft_version
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Fabric metadata returned an error for Minecraft {}",
                    minecraft_version
                )
            })?
            .json::<Vec<FabricLoaderVersionEntry>>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Fabric loader versions for Minecraft {}",
                    minecraft_version
                )
            })?;

        let selected = select_fabric_loader_version(&versions).with_context(|| {
            format!(
                "no Fabric loader version found for Minecraft {}",
                minecraft_version
            )
        })?;
        let profile_url = build_fabric_loader_profile_url(
            &self.fabric_base_url,
            minecraft_version,
            &selected.loader.version,
        )?;
        let profile = self
            .http_client
            .get(profile_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Fabric profile for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Fabric profile returned an error for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?
            .json::<LauncherProfile>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Fabric profile for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?;

        Ok(loader_metadata_from_profile(
            ModLoader::Fabric,
            minecraft_version,
            selected.loader.version.clone(),
            selected
                .launcher_meta
                .as_ref()
                .and_then(|launcher_meta| launcher_meta.min_java_version),
            profile,
        ))
    }

    async fn fetch_quilt_metadata(&self, minecraft_version: &str) -> Result<LoaderMetadata> {
        let versions_url =
            build_quilt_loader_versions_url(&self.quilt_base_url, minecraft_version)?;
        let versions = self
            .http_client
            .get(versions_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Quilt metadata for Minecraft {}",
                    minecraft_version
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Quilt metadata returned an error for Minecraft {}",
                    minecraft_version
                )
            })?
            .json::<Vec<QuiltLoaderVersionEntry>>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Quilt loader versions for Minecraft {}",
                    minecraft_version
                )
            })?;

        let selected = select_quilt_loader_version(&versions).with_context(|| {
            format!(
                "no Quilt loader version found for Minecraft {}",
                minecraft_version
            )
        })?;
        let profile_url = build_quilt_loader_profile_url(
            &self.quilt_base_url,
            minecraft_version,
            &selected.loader.version,
        )?;
        let profile = self
            .http_client
            .get(profile_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Quilt profile for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Quilt profile returned an error for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?
            .json::<LauncherProfile>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Quilt profile for Minecraft {} and loader {}",
                    minecraft_version, selected.loader.version
                )
            })?;

        Ok(loader_metadata_from_profile(
            ModLoader::Quilt,
            minecraft_version,
            selected.loader.version.clone(),
            None,
            profile,
        ))
    }

    async fn fetch_prism_metadata(
        &self,
        minecraft_version: &str,
        package_uid: PrismPackageUid,
    ) -> Result<LoaderMetadata> {
        let index_url = build_prism_package_index_url(&self.prism_base_url, package_uid.uid())?;
        let index = self
            .http_client
            .get(index_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Prism metadata package index {}",
                    package_uid.uid()
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Prism metadata returned an error for package index {}",
                    package_uid.uid()
                )
            })?
            .json::<PrismPackageIndex>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Prism package index {}",
                    package_uid.uid()
                )
            })?;

        let selected = select_prism_package_version(&index.versions, minecraft_version)
            .with_context(|| {
                format!(
                    "no {} metadata version found for Minecraft {}",
                    package_uid.uid(),
                    minecraft_version
                )
            })?;
        let version_url = build_prism_package_version_url(
            &self.prism_base_url,
            package_uid.uid(),
            &selected.version,
        )?;
        let version = self
            .http_client
            .get(version_url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to query Prism metadata {} version {}",
                    package_uid.uid(),
                    selected.version
                )
            })?
            .error_for_status()
            .with_context(|| {
                format!(
                    "Prism metadata returned an error for {} version {}",
                    package_uid.uid(),
                    selected.version
                )
            })?
            .json::<PrismPackageVersionDetail>()
            .await
            .with_context(|| {
                format!(
                    "failed to deserialize Prism metadata {} version {}",
                    package_uid.uid(),
                    selected.version
                )
            })?;

        Ok(loader_metadata_from_prism(
            package_uid.mod_loader(),
            minecraft_version,
            selected.version.clone(),
            version,
        ))
    }
}

impl Default for LoaderMetadataClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderMetadata {
    pub mod_loader: ModLoader,
    pub minecraft_version: String,
    pub loader_version: String,
    pub main_class: String,
    pub libraries: Vec<LoaderLibrary>,
    pub maven_files: Vec<LoaderLibrary>,
    pub jvm_arguments: Vec<String>,
    pub game_arguments: Vec<String>,
    pub min_java_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderLibrary {
    pub name: String,
    pub url: Option<String>,
    pub download: Option<LibraryDownloadArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryDownloadArtifact {
    pub url: String,
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PrismPackageUid {
    Forge,
    NeoForge,
}

impl PrismPackageUid {
    fn uid(self) -> &'static str {
        match self {
            PrismPackageUid::Forge => "net.minecraftforge",
            PrismPackageUid::NeoForge => "net.neoforged",
        }
    }

    fn mod_loader(self) -> ModLoader {
        match self {
            PrismPackageUid::Forge => ModLoader::Forge,
            PrismPackageUid::NeoForge => ModLoader::NeoForge,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FabricLoaderVersionEntry {
    pub loader: FabricLoaderDescriptor,
    #[serde(rename = "launcherMeta")]
    pub launcher_meta: Option<FabricLauncherMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FabricLoaderDescriptor {
    pub version: String,
    pub stable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct QuiltLoaderVersionEntry {
    pub loader: QuiltLoaderDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct QuiltLoaderDescriptor {
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FabricLauncherMeta {
    pub version: Option<u32>,
    pub min_java_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LauncherProfile {
    #[serde(rename = "mainClass")]
    pub main_class: String,
    pub libraries: Vec<SimpleProfileLibrary>,
    #[serde(default)]
    pub arguments: Option<ProfileArguments>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SimpleProfileLibrary {
    pub name: String,
    pub url: Option<String>,
    #[serde(default)]
    pub downloads: Option<ProfileDownloads>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileDownloads {
    pub artifact: Option<ProfileArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileArtifact {
    pub url: String,
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct ProfileArguments {
    #[serde(default)]
    pub game: Vec<String>,
    #[serde(default)]
    pub jvm: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismPackageIndex {
    pub versions: Vec<PrismPackageVersionRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismPackageVersionRef {
    pub version: String,
    pub recommended: bool,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
    #[serde(default)]
    pub requires: Vec<PrismRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismRequirement {
    pub uid: String,
    pub equals: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismPackageVersionDetail {
    #[serde(rename = "mainClass")]
    pub main_class: String,
    #[serde(default)]
    pub libraries: Vec<PrismLibrary>,
    #[serde(rename = "mavenFiles", default)]
    pub maven_files: Vec<PrismLibrary>,
    #[serde(rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismLibrary {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<PrismDownloads>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismDownloads {
    pub artifact: Option<PrismArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PrismArtifact {
    pub url: String,
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

pub fn build_fabric_loader_versions_url(base_url: &str, minecraft_version: &str) -> Result<Url> {
    Url::parse(&format!(
        "{}/versions/loader/{}",
        base_url.trim_end_matches('/'),
        minecraft_version
    ))
    .with_context(|| format!("invalid Fabric base URL '{base_url}'"))
}

pub fn build_fabric_loader_profile_url(
    base_url: &str,
    minecraft_version: &str,
    loader_version: &str,
) -> Result<Url> {
    Url::parse(&format!(
        "{}/versions/loader/{}/{}/profile/json",
        base_url.trim_end_matches('/'),
        minecraft_version,
        loader_version
    ))
    .with_context(|| format!("invalid Fabric base URL '{base_url}'"))
}

pub fn build_quilt_loader_versions_url(base_url: &str, minecraft_version: &str) -> Result<Url> {
    Url::parse(&format!(
        "{}/versions/loader/{}",
        base_url.trim_end_matches('/'),
        minecraft_version
    ))
    .with_context(|| format!("invalid Quilt base URL '{base_url}'"))
}

pub fn build_quilt_loader_profile_url(
    base_url: &str,
    minecraft_version: &str,
    loader_version: &str,
) -> Result<Url> {
    Url::parse(&format!(
        "{}/versions/loader/{}/{}/profile/json",
        base_url.trim_end_matches('/'),
        minecraft_version,
        loader_version
    ))
    .with_context(|| format!("invalid Quilt base URL '{base_url}'"))
}

pub fn build_prism_package_index_url(base_url: &str, package_uid: &str) -> Result<Url> {
    Url::parse(&format!(
        "{}/{}/index.json",
        base_url.trim_end_matches('/'),
        package_uid
    ))
    .with_context(|| format!("invalid Prism base URL '{base_url}'"))
}

pub fn build_prism_package_version_url(
    base_url: &str,
    package_uid: &str,
    version: &str,
) -> Result<Url> {
    Url::parse(&format!(
        "{}/{}/{}.json",
        base_url.trim_end_matches('/'),
        package_uid,
        version
    ))
    .with_context(|| format!("invalid Prism base URL '{base_url}'"))
}

pub fn select_fabric_loader_version(
    versions: &[FabricLoaderVersionEntry],
) -> Option<&FabricLoaderVersionEntry> {
    versions
        .iter()
        .find(|entry| entry.loader.stable)
        .or_else(|| versions.first())
}

pub fn select_quilt_loader_version(
    versions: &[QuiltLoaderVersionEntry],
) -> Option<&QuiltLoaderVersionEntry> {
    versions.first()
}

pub fn select_prism_package_version<'a>(
    versions: &'a [PrismPackageVersionRef],
    minecraft_version: &str,
) -> Option<&'a PrismPackageVersionRef> {
    versions
        .iter()
        .filter(|version| prism_version_matches_minecraft(version, minecraft_version))
        .find(|version| version.recommended)
        .or_else(|| {
            versions
                .iter()
                .filter(|version| prism_version_matches_minecraft(version, minecraft_version))
                .max_by(|left, right| left.release_time.cmp(&right.release_time))
        })
}

fn prism_version_matches_minecraft(
    version: &PrismPackageVersionRef,
    minecraft_version: &str,
) -> bool {
    version.requires.iter().any(|requirement| {
        requirement.uid == "net.minecraft"
            && requirement.equals.as_deref() == Some(minecraft_version)
    })
}

fn loader_metadata_from_profile(
    mod_loader: ModLoader,
    minecraft_version: &str,
    loader_version: String,
    min_java_version: Option<u32>,
    profile: LauncherProfile,
) -> LoaderMetadata {
    let arguments = profile.arguments.unwrap_or_default();

    LoaderMetadata {
        mod_loader,
        minecraft_version: minecraft_version.to_string(),
        loader_version,
        main_class: profile.main_class,
        libraries: profile
            .libraries
            .into_iter()
            .map(|library| LoaderLibrary {
                name: library.name,
                url: library.url,
                download: library.downloads.and_then(|downloads| {
                    downloads.artifact.map(|artifact| LibraryDownloadArtifact {
                        url: artifact.url,
                        path: artifact.path,
                        sha1: artifact.sha1,
                        size: artifact.size,
                    })
                }),
            })
            .collect(),
        maven_files: Vec::new(),
        jvm_arguments: arguments.jvm,
        game_arguments: arguments.game,
        min_java_version,
    }
}

fn loader_metadata_from_prism(
    mod_loader: ModLoader,
    minecraft_version: &str,
    loader_version: String,
    detail: PrismPackageVersionDetail,
) -> LoaderMetadata {
    LoaderMetadata {
        mod_loader,
        minecraft_version: minecraft_version.to_string(),
        loader_version,
        main_class: detail.main_class,
        libraries: detail
            .libraries
            .into_iter()
            .map(|library| LoaderLibrary {
                name: library.name,
                url: library.url,
                download: library.downloads.and_then(|downloads| {
                    downloads.artifact.map(|artifact| LibraryDownloadArtifact {
                        url: artifact.url,
                        path: artifact.path,
                        sha1: artifact.sha1,
                        size: artifact.size,
                    })
                }),
            })
            .collect(),
        maven_files: detail
            .maven_files
            .into_iter()
            .map(|library| LoaderLibrary {
                name: library.name,
                url: library.url,
                download: library.downloads.and_then(|downloads| {
                    downloads.artifact.map(|artifact| LibraryDownloadArtifact {
                        url: artifact.url,
                        path: artifact.path,
                        sha1: artifact.sha1,
                        size: artifact.size,
                    })
                }),
            })
            .collect(),
        jvm_arguments: Vec::new(),
        game_arguments: detail
            .minecraft_arguments
            .unwrap_or_default()
            .split_whitespace()
            .map(ToString::to_string)
            .collect(),
        min_java_version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_fabric_loader_profile_url, build_fabric_loader_versions_url,
        build_prism_package_index_url, build_prism_package_version_url,
        build_quilt_loader_profile_url, build_quilt_loader_versions_url,
        loader_metadata_from_prism, loader_metadata_from_profile, select_fabric_loader_version,
        select_prism_package_version, FabricLoaderVersionEntry, LauncherProfile,
        PrismPackageVersionDetail, PrismPackageVersionRef, QuiltLoaderVersionEntry,
    };
    use crate::resolver::ModLoader;

    fn fabric_versions_json() -> &'static str {
        r#"[
          { "loader": { "version": "0.16.14", "stable": false } },
          { "loader": { "version": "0.16.13", "stable": true } }
        ]"#
    }

    fn quilt_versions_json() -> &'static str {
        r#"[
          { "loader": { "version": "0.29.2" } },
          { "loader": { "version": "0.29.1" } }
        ]"#
    }

    fn prism_index_json() -> &'static str {
        r#"[
          {
            "recommended": false,
            "releaseTime": "2025-01-01T00:00:00+00:00",
            "requires": [{ "uid": "net.minecraft", "equals": "1.21.1" }],
            "version": "52.1.9"
          },
          {
            "recommended": true,
            "releaseTime": "2025-01-02T00:00:00+00:00",
            "requires": [{ "uid": "net.minecraft", "equals": "1.21.1" }],
            "version": "52.1.10"
          },
          {
            "recommended": true,
            "releaseTime": "2025-01-03T00:00:00+00:00",
            "requires": [{ "uid": "net.minecraft", "equals": "1.21.3" }],
            "version": "53.1.0"
          }
        ]"#
    }

    fn fabric_profile_json() -> &'static str {
        r#"{
          "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
          "arguments": {
            "game": [],
            "jvm": ["-DFabricMcEmu= net.minecraft.client.main.Main "]
          },
          "libraries": [
            { "name": "net.fabricmc:intermediary:1.21.1", "url": "https://maven.fabricmc.net/" },
            { "name": "net.fabricmc:fabric-loader:0.16.14", "url": "https://maven.fabricmc.net/" }
          ]
        }"#
    }

    fn prism_detail_json() -> &'static str {
        r#"{
          "mainClass": "io.github.zekerzhayard.forgewrapper.installer.Main",
          "minecraftArguments": "--launchTarget forge_client --fml.mcVersion 1.21.1",
          "libraries": [
            {
              "name": "net.minecraftforge:forge:1.21.1-52.1.10:universal",
              "downloads": {
                "artifact": {
                  "url": "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.1.10/forge-1.21.1-52.1.10-universal.jar",
                  "path": "net/minecraftforge/forge/1.21.1-52.1.10/forge-1.21.1-52.1.10-universal.jar",
                  "sha1": "37e26f1dcd6c75537d9529145ce47c096ac08ed8",
                  "size": 2961514
                }
              }
            }
          ],
          "mavenFiles": [
            {
              "name": "net.neoforged.installertools:installertools:4.0.6:fatjar",
              "downloads": {
                "artifact": {
                  "url": "https://maven.neoforged.net/releases/net/neoforged/installertools/installertools/4.0.6/installertools-4.0.6-fatjar.jar",
                  "path": "net/neoforged/installertools/installertools/4.0.6/installertools-4.0.6-fatjar.jar",
                  "sha1": "17b145cf3a1816153d067316eeee9dc89bfd9bb2",
                  "size": 770782
                }
              }
            }
          ]
        }"#
    }

    #[test]
    fn builds_expected_loader_metadata_urls() {
        assert_eq!(
            build_fabric_loader_versions_url("https://meta.fabricmc.net/v2", "1.21.1")
                .unwrap()
                .as_str(),
            "https://meta.fabricmc.net/v2/versions/loader/1.21.1"
        );
        assert_eq!(
            build_fabric_loader_profile_url("https://meta.fabricmc.net/v2", "1.21.1", "0.16.14")
                .unwrap()
                .as_str(),
            "https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.16.14/profile/json"
        );
        assert_eq!(
            build_quilt_loader_versions_url("https://meta.quiltmc.org/v3", "1.21.1")
                .unwrap()
                .as_str(),
            "https://meta.quiltmc.org/v3/versions/loader/1.21.1"
        );
        assert_eq!(
            build_quilt_loader_profile_url("https://meta.quiltmc.org/v3", "1.21.1", "0.29.2")
                .unwrap()
                .as_str(),
            "https://meta.quiltmc.org/v3/versions/loader/1.21.1/0.29.2/profile/json"
        );
        assert_eq!(
            build_prism_package_index_url(
                "https://meta.prismlauncher.org/v1",
                "net.minecraftforge"
            )
            .unwrap()
            .as_str(),
            "https://meta.prismlauncher.org/v1/net.minecraftforge/index.json"
        );
        assert_eq!(
            build_prism_package_version_url(
                "https://meta.prismlauncher.org/v1",
                "net.minecraftforge",
                "52.1.10"
            )
            .unwrap()
            .as_str(),
            "https://meta.prismlauncher.org/v1/net.minecraftforge/52.1.10.json"
        );
    }

    #[test]
    fn selects_stable_fabric_loader_and_first_quilt_loader() {
        let fabric_versions: Vec<FabricLoaderVersionEntry> =
            serde_json::from_str(fabric_versions_json()).unwrap();
        let quilt_versions: Vec<QuiltLoaderVersionEntry> =
            serde_json::from_str(quilt_versions_json()).unwrap();

        assert_eq!(
            select_fabric_loader_version(&fabric_versions)
                .unwrap()
                .loader
                .version,
            "0.16.13"
        );
        assert_eq!(quilt_versions.first().unwrap().loader.version, "0.29.2");
    }

    #[test]
    fn selects_recommended_prism_version_for_target_minecraft() {
        let versions: Vec<PrismPackageVersionRef> =
            serde_json::from_str(prism_index_json()).unwrap();

        let selected = select_prism_package_version(&versions, "1.21.1").unwrap();

        assert_eq!(selected.version, "52.1.10");
    }

    #[test]
    fn converts_fabric_profile_into_loader_metadata() {
        let profile: LauncherProfile = serde_json::from_str(fabric_profile_json()).unwrap();
        let metadata = loader_metadata_from_profile(
            ModLoader::Fabric,
            "1.21.1",
            "0.16.14".into(),
            None,
            profile,
        );

        assert_eq!(
            metadata.main_class,
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
        assert_eq!(metadata.loader_version, "0.16.14");
        assert_eq!(metadata.libraries.len(), 2);
        assert_eq!(metadata.jvm_arguments.len(), 1);
    }

    #[test]
    fn fabric_min_java_version_is_propagated() {
        let entry: FabricLoaderVersionEntry = serde_json::from_str(
            r#"{
              "loader": { "version": "0.16.14", "stable": true },
              "launcherMeta": { "min_java_version": 21 }
            }"#,
        )
        .unwrap();
        let min_java_version = entry
            .launcher_meta
            .and_then(|launcher_meta| launcher_meta.min_java_version);
        let profile: LauncherProfile = serde_json::from_str(fabric_profile_json()).unwrap();
        let metadata = loader_metadata_from_profile(
            ModLoader::Fabric,
            "1.21.1",
            "0.16.14".into(),
            min_java_version,
            profile,
        );

        assert_eq!(min_java_version, Some(21));
        assert_eq!(metadata.min_java_version, Some(21));
    }

    #[test]
    fn converts_prism_detail_into_loader_metadata() {
        let detail: PrismPackageVersionDetail = serde_json::from_str(prism_detail_json()).unwrap();
        let metadata =
            loader_metadata_from_prism(ModLoader::Forge, "1.21.1", "52.1.10".into(), detail);

        assert_eq!(
            metadata.main_class,
            "io.github.zekerzhayard.forgewrapper.installer.Main"
        );
        assert_eq!(metadata.loader_version, "52.1.10");
        assert_eq!(metadata.libraries.len(), 1);
        assert_eq!(metadata.maven_files.len(), 1);
        assert_eq!(
            metadata.maven_files[0].name,
            "net.neoforged.installertools:installertools:4.0.6:fatjar"
        );
        assert!(metadata
            .game_arguments
            .contains(&"--launchTarget".to_string()));
    }

    fn prism_neoforge_detail_json() -> &'static str {
        // Shape mirrors live Prism meta for net.neoforged/21.1.242.json: modern
        // NeoForge still ships game args via the (Prism-synthesized) legacy
        // `minecraftArguments` string, NOT an `arguments.game` array.
        r#"{
          "mainClass": "io.github.zekerzhayard.forgewrapper.installer.Main",
          "minecraftArguments": "--username ${auth_player_name} --version ${version_name} --gameDir ${game_directory} --launchTarget forgeclient --fml.neoForgeVersion 21.1.242 --fml.fmlVersion 4.0.43 --fml.mcVersion 1.21.1",
          "libraries": [
            {
              "name": "net.neoforged.fancymodloader:loader:4.0.43",
              "downloads": {
                "artifact": {
                  "url": "https://maven.neoforged.net/releases/net/neoforged/fancymodloader/loader/4.0.43/loader-4.0.43.jar",
                  "path": "net/neoforged/fancymodloader/loader/4.0.43/loader-4.0.43.jar",
                  "sha1": "fb10b7bf2f568a9676ad8b426b19c23badbbd98a",
                  "size": 505633
                }
              }
            }
          ]
        }"#
    }

    #[test]
    fn converts_prism_neoforge_detail_populates_game_arguments() {
        let detail: PrismPackageVersionDetail =
            serde_json::from_str(prism_neoforge_detail_json()).unwrap();
        let metadata =
            loader_metadata_from_prism(ModLoader::NeoForge, "1.21.1", "21.1.242".into(), detail);

        // ForgeWrapper requires these game args; empty args would crash it with
        // IndexOutOfBounds on --fml.mcVersion.
        assert!(!metadata.game_arguments.is_empty());
        assert!(metadata
            .game_arguments
            .contains(&"--launchTarget".to_string()));
        assert!(metadata
            .game_arguments
            .contains(&"--fml.mcVersion".to_string()));
        assert!(metadata
            .game_arguments
            .contains(&"1.21.1".to_string()));
    }
}
