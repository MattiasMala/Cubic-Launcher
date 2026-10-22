// Cache su disco dei metadati del loader (D137, onda 8b; D160).
//
// Il lancio online continua a chiedere alla rete l'ultimo loader stabile, come
// prima, e scrive quello che la rete ha risposto in
// `cache/loader-metadata/<minecraft>/<loader>.json`. Il lancio di un account
// che gioca offline rilegge quella voce e non fa nessuna richiesta (D132):
// usa il loader dell'ultimo lancio online della stessa combinazione. Una
// combinazione mai lanciata online non parte offline, e lo dice con un
// messaggio suo invece di un errore di rete.
//
// La cache sta attorno alla fetch, non dentro i parser di `loader_metadata.rs`,
// e non vince mai online: nemmeno quando la rete dà errore (D137, vincolo 2).
// Il vanilla non ha niente da mettere in cache e non la tocca (vincolo 9).
//
// Il formato è un tipo di questo file, non `LoaderMetadata` con serde: i due si
// convertono con una destrutturazione completa, così un campo aggiunto a
// `LoaderMetadata` non compila finché qualcuno non decide cosa farne qui.
//
// Le librerie del loader però stanno nell'istanza, non nella cache: la voce di
// combinazione dice quale loader l'ultimo lancio online di *una* mod list ha
// usato, non quale ha sul disco *questa* (review-lancio-1). Per questo un lancio
// online che ha preparato tutti i file del loader ne lascia una copia anche
// nell'istanza, in `<istanza>/.cubic/loader-metadata.json`, e offline vince
// quella: è il loader di cui l'istanza ha davvero le librerie. Senza la copia
// dell'istanza resta la voce di combinazione (D137); e offline un file del
// loader che manca nell'istanza ferma il lancio con un messaggio suo, senza
// nessuna richiesta (D132).

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::atomic_write::write_atomically;
use crate::loader_metadata::{LibraryDownloadArtifact, LoaderLibrary, LoaderMetadata};
use crate::path_safety::validate_path_component;
use crate::resolver::{ModLoader, ResolutionTarget};

/// La versione del formato della voce. Una voce di un'altra versione vale come
/// assente (D137, vincolo 7): online si richiede alla rete, offline si dà il
/// messaggio. Chi cambia la forma di `CachedLoaderMetadataFile` la incrementa.
pub(super) const LOADER_METADATA_CACHE_FORMAT: u32 = 1;

/// Quello che la cache dice di una combinazione.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CachedLoaderMetadata {
    Found(LoaderMetadata),
    Missing,
    /// Il file c'è ma non si legge: troncato, vuoto, JSON non valido, di una
    /// versione del formato che non si conosce, o scritto per un'altra
    /// combinazione. Vale come assente, ma il motivo va nel log.
    Unreadable(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedLoaderMetadataFile {
    format_version: u32,
    mod_loader: String,
    minecraft_version: String,
    loader_version: String,
    main_class: String,
    libraries: Vec<CachedLoaderLibrary>,
    maven_files: Vec<CachedLoaderLibrary>,
    jvm_arguments: Vec<String>,
    game_arguments: Vec<String>,
    min_java_version: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedLoaderLibrary {
    name: String,
    url: Option<String>,
    download: Option<CachedLibraryDownload>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedLibraryDownload {
    url: String,
    path: Option<String>,
    sha1: Option<String>,
    size: Option<u64>,
}

/// Il nome del file della voce: stabile, minuscolo, indipendente da come il
/// frontend scrive il loader.
fn loader_cache_key(mod_loader: ModLoader) -> &'static str {
    match mod_loader {
        ModLoader::Fabric => "fabric",
        ModLoader::Quilt => "quilt",
        ModLoader::Forge => "forge",
        ModLoader::NeoForge => "neoforge",
        ModLoader::Vanilla => "vanilla",
    }
}

fn loader_from_cache_key(key: &str) -> Option<ModLoader> {
    match key {
        "fabric" => Some(ModLoader::Fabric),
        "quilt" => Some(ModLoader::Quilt),
        "forge" => Some(ModLoader::Forge),
        "neoforge" => Some(ModLoader::NeoForge),
        "vanilla" => Some(ModLoader::Vanilla),
        _ => None,
    }
}

fn loader_display_name(mod_loader: ModLoader) -> &'static str {
    match mod_loader {
        ModLoader::Fabric => "Fabric",
        ModLoader::Quilt => "Quilt",
        ModLoader::Forge => "Forge",
        ModLoader::NeoForge => "NeoForge",
        ModLoader::Vanilla => "Vanilla",
    }
}

/// `<cache_dir>/<minecraft>/<loader>.json`. La versione di Minecraft arriva
/// dalla richiesta di lancio: deve essere un solo segmento di percorso.
pub(super) fn loader_metadata_cache_path(
    cache_dir: &Path,
    target: &ResolutionTarget,
) -> Result<PathBuf> {
    validate_path_component(&target.minecraft_version).with_context(|| {
        format!(
            "Minecraft version '{}' cannot name a loader metadata cache entry",
            target.minecraft_version
        )
    })?;
    Ok(cache_dir
        .join(&target.minecraft_version)
        .join(format!("{}.json", loader_cache_key(target.mod_loader))))
}

fn to_cached_library(library: &LoaderLibrary) -> CachedLoaderLibrary {
    let LoaderLibrary {
        name,
        url,
        download,
    } = library;
    CachedLoaderLibrary {
        name: name.clone(),
        url: url.clone(),
        download: download.as_ref().map(|download| {
            let LibraryDownloadArtifact {
                url,
                path,
                sha1,
                size,
            } = download;
            CachedLibraryDownload {
                url: url.clone(),
                path: path.clone(),
                sha1: sha1.clone(),
                size: *size,
            }
        }),
    }
}

fn from_cached_library(library: CachedLoaderLibrary) -> LoaderLibrary {
    let CachedLoaderLibrary {
        name,
        url,
        download,
    } = library;
    LoaderLibrary {
        name,
        url,
        download: download.map(|download| {
            let CachedLibraryDownload {
                url,
                path,
                sha1,
                size,
            } = download;
            LibraryDownloadArtifact {
                url,
                path,
                sha1,
                size,
            }
        }),
    }
}

fn to_cache_file(metadata: &LoaderMetadata) -> CachedLoaderMetadataFile {
    let LoaderMetadata {
        mod_loader,
        minecraft_version,
        loader_version,
        main_class,
        libraries,
        maven_files,
        jvm_arguments,
        game_arguments,
        min_java_version,
    } = metadata;
    CachedLoaderMetadataFile {
        format_version: LOADER_METADATA_CACHE_FORMAT,
        mod_loader: loader_cache_key(*mod_loader).to_string(),
        minecraft_version: minecraft_version.clone(),
        loader_version: loader_version.clone(),
        main_class: main_class.clone(),
        libraries: libraries.iter().map(to_cached_library).collect(),
        maven_files: maven_files.iter().map(to_cached_library).collect(),
        jvm_arguments: jvm_arguments.clone(),
        game_arguments: game_arguments.clone(),
        min_java_version: *min_java_version,
    }
}

fn from_cache_file(
    file: CachedLoaderMetadataFile,
    target: &ResolutionTarget,
) -> std::result::Result<LoaderMetadata, String> {
    let CachedLoaderMetadataFile {
        format_version,
        mod_loader,
        minecraft_version,
        loader_version,
        main_class,
        libraries,
        maven_files,
        jvm_arguments,
        game_arguments,
        min_java_version,
    } = file;
    if format_version != LOADER_METADATA_CACHE_FORMAT {
        return Err(format!(
            "format version {format_version}, this launcher reads {LOADER_METADATA_CACHE_FORMAT}"
        ));
    }
    let mod_loader = loader_from_cache_key(&mod_loader)
        .ok_or_else(|| format!("unknown mod loader '{mod_loader}'"))?;
    if mod_loader != target.mod_loader || minecraft_version != target.minecraft_version {
        return Err(format!(
            "written for {} {minecraft_version}",
            loader_display_name(mod_loader)
        ));
    }
    Ok(LoaderMetadata {
        mod_loader,
        minecraft_version,
        loader_version,
        main_class,
        libraries: libraries.into_iter().map(from_cached_library).collect(),
        maven_files: maven_files.into_iter().map(from_cached_library).collect(),
        jvm_arguments,
        game_arguments,
        min_java_version,
    })
}

/// Rilegge la voce della combinazione. `Err` solo se la combinazione non può
/// nominare un file; un file che non si legge è `Unreadable`, non un errore.
pub(super) fn read_cached_loader_metadata(
    cache_dir: &Path,
    target: &ResolutionTarget,
) -> Result<CachedLoaderMetadata> {
    let path = loader_metadata_cache_path(cache_dir, target)?;
    Ok(read_loader_metadata_entry(&path, target))
}

/// La copia che l'istanza tiene del loader di cui ha preparato i file.
pub(super) fn instance_loader_metadata_path(instance_root: &Path) -> PathBuf {
    instance_root.join(".cubic").join("loader-metadata.json")
}

/// Rilegge la copia dell'istanza; stesso formato e stesse regole della voce di
/// combinazione.
pub(super) fn read_instance_loader_metadata(
    instance_root: &Path,
    target: &ResolutionTarget,
) -> CachedLoaderMetadata {
    read_loader_metadata_entry(&instance_loader_metadata_path(instance_root), target)
}

/// Scrive la copia dell'istanza. Va chiamata solo quando tutti i file del
/// loader sono nell'istanza, e con i metadati come la rete li ha dati (prima
/// che ForgeWrapper ci aggiunga i suoi argomenti).
pub(super) fn write_instance_loader_metadata(
    instance_root: &Path,
    metadata: &LoaderMetadata,
) -> Result<()> {
    write_loader_metadata_entry(&instance_loader_metadata_path(instance_root), metadata)
}

fn read_loader_metadata_entry(path: &Path, target: &ResolutionTarget) -> CachedLoaderMetadata {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CachedLoaderMetadata::Missing
        }
        Err(error) => {
            return CachedLoaderMetadata::Unreadable(format!("{}: {error}", path.display()))
        }
    };
    let file = match serde_json::from_str::<CachedLoaderMetadataFile>(&contents) {
        Ok(file) => file,
        Err(error) => {
            return CachedLoaderMetadata::Unreadable(format!("{}: {error}", path.display()))
        }
    };
    match from_cache_file(file, target) {
        Ok(metadata) => CachedLoaderMetadata::Found(metadata),
        Err(reason) => CachedLoaderMetadata::Unreadable(format!("{}: {reason}", path.display())),
    }
}

/// Scrive la voce della combinazione in modo atomico: un lancio interrotto a
/// metà lascia la voce di prima, mai una mezza voce.
pub(super) fn write_cached_loader_metadata(
    cache_dir: &Path,
    target: &ResolutionTarget,
    metadata: &LoaderMetadata,
) -> Result<()> {
    let path = loader_metadata_cache_path(cache_dir, target)?;
    write_loader_metadata_entry(&path, metadata)
}

fn write_loader_metadata_entry(path: &Path, metadata: &LoaderMetadata) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&to_cache_file(metadata))
        .context("failed to serialize the loader metadata")?;
    write_atomically(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Il messaggio di D137, vincolo 3: nomina la combinazione e dice cosa fare.
/// Non parla di rete, perché offline la rete non è mai stata toccata.
pub(super) fn never_launched_online_message(target: &ResolutionTarget) -> String {
    let loader = loader_display_name(target.mod_loader);
    let minecraft = &target.minecraft_version;
    format!(
        "{loader} {minecraft} has never been launched online on this computer, so the {loader} loader it needs is not saved here, and the active account plays offline, so nothing is downloaded. Play {loader} {minecraft} once with the account online (switch the account to online for one launch); after that it plays offline too, with the same loader version."
    )
}

/// Offline, un file del loader che l'istanza non ha (review-lancio-1): la
/// voce di combinazione può venire dal lancio online di un'altra mod list, o da
/// un lancio online morto prima di scaricare le librerie. Come D137 nomina cosa
/// manca e dice cosa fare, senza parlare di rete.
pub(super) fn loader_not_installed_message(metadata: &LoaderMetadata, missing: &Path) -> String {
    let loader = loader_display_name(metadata.mod_loader);
    let minecraft = &metadata.minecraft_version;
    let version = &metadata.loader_version;
    format!(
        "{loader} {version} for Minecraft {minecraft} is not installed in this mod list (missing {}), and the active account plays offline, so nothing is downloaded. Play this mod list once with the account online (switch the account to online for one launch); after that it plays offline too, with the same loader version.",
        missing.display()
    )
}

/// I metadati del loader per un lancio.
///
/// - Vanilla: `fetch` e basta, senza cache (non chiede niente a nessuno).
/// - Online: `fetch`, come prima; quello che la rete ha risposto si scrive in
///   cache **prima** di ogni modifica del chiamante (vincolo 8). Un errore
///   della rete resta l'errore di rete: niente ripiego sulla cache. Una
///   scrittura che fallisce non ferma il lancio, lo dice il log.
/// - Offline: senza chiamare `fetch`, la copia dell'istanza se c'è (il loader
///   di cui l'istanza ha i file), altrimenti la voce di combinazione; assenti o
///   illeggibili tutte e due, il messaggio di D137.
pub(super) async fn loader_metadata_for_launch<F, Fut>(
    cache_dir: &Path,
    instance_root: &Path,
    target: &ResolutionTarget,
    plays_offline: bool,
    fetch: F,
    log: impl Fn(String),
) -> Result<LoaderMetadata>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<LoaderMetadata>>,
{
    if target.mod_loader == ModLoader::Vanilla {
        return fetch().await;
    }
    let loader = loader_display_name(target.mod_loader);
    let minecraft = &target.minecraft_version;

    if !plays_offline {
        let metadata = fetch().await?;
        if let Err(error) = write_cached_loader_metadata(cache_dir, target, &metadata) {
            log(format!(
                "[Loader] Could not save the {loader} {minecraft} loader metadata for offline launches: {error:#}"
            ));
        }
        return Ok(metadata);
    }

    match read_instance_loader_metadata(instance_root, target) {
        CachedLoaderMetadata::Found(metadata) => {
            log(format!(
                "[Loader] The active account plays offline: using {loader} loader {} for Minecraft {minecraft}, the one this mod list was last launched online with. The latest stable loader is not looked up.",
                metadata.loader_version
            ));
            return Ok(metadata);
        }
        CachedLoaderMetadata::Missing => {}
        CachedLoaderMetadata::Unreadable(reason) => log(format!(
            "[Loader] This mod list's saved {loader} {minecraft} loader metadata cannot be read, so the one saved for {loader} {minecraft} is used: {reason}"
        )),
    }

    match read_cached_loader_metadata(cache_dir, target)? {
        CachedLoaderMetadata::Found(metadata) => {
            log(format!(
                "[Loader] The active account plays offline: using {loader} loader {} for Minecraft {minecraft}, saved by the last online launch of {loader} {minecraft}. The latest stable loader is not looked up.",
                metadata.loader_version
            ));
            Ok(metadata)
        }
        CachedLoaderMetadata::Missing => bail!(never_launched_online_message(target)),
        CachedLoaderMetadata::Unreadable(reason) => {
            log(format!(
                "[Loader] The saved {loader} {minecraft} loader metadata cannot be read, so it counts as missing: {reason}"
            ));
            bail!(never_launched_online_message(target))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::anyhow;

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "cubic-loader-cache-{label}-{unique}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn target(minecraft_version: &str, mod_loader: ModLoader) -> ResolutionTarget {
        ResolutionTarget {
            minecraft_version: minecraft_version.to_string(),
            mod_loader,
        }
    }

    /// Non banale: librerie con e senza `download`, `sha1`/`size`/`path`
    /// presenti e assenti, `maven_files`, argomenti non vuoti.
    fn sample_metadata(
        minecraft_version: &str,
        mod_loader: ModLoader,
        min_java_version: Option<u32>,
    ) -> LoaderMetadata {
        LoaderMetadata {
            mod_loader,
            minecraft_version: minecraft_version.to_string(),
            loader_version: "0.16.10".to_string(),
            main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            libraries: vec![
                LoaderLibrary {
                    name: "net.fabricmc:fabric-loader:0.16.10".to_string(),
                    url: Some("https://maven.fabricmc.net/".to_string()),
                    download: None,
                },
                LoaderLibrary {
                    name: "org.ow2.asm:asm:9.7.1".to_string(),
                    url: None,
                    download: Some(LibraryDownloadArtifact {
                        url: "https://maven.fabricmc.net/org/ow2/asm/asm/9.7.1/asm-9.7.1.jar"
                            .to_string(),
                        path: Some("org/ow2/asm/asm/9.7.1/asm-9.7.1.jar".to_string()),
                        sha1: Some("f0ed132a49244b042cd0e15702ab9f2ce3cc8436".to_string()),
                        size: Some(126_093),
                    }),
                },
                LoaderLibrary {
                    name: "net.fabricmc:sponge-mixin:0.15.4".to_string(),
                    url: None,
                    download: Some(LibraryDownloadArtifact {
                        url: "https://maven.fabricmc.net/net/fabricmc/sponge-mixin.jar".to_string(),
                        path: None,
                        sha1: None,
                        size: None,
                    }),
                },
            ],
            maven_files: vec![LoaderLibrary {
                name: "net.neoforged:neoforge:21.1.1:installer".to_string(),
                url: Some("https://maven.neoforged.net/releases/".to_string()),
                download: None,
            }],
            jvm_arguments: vec!["-DFabricMcEmu= net.minecraft.client.main.Main ".to_string()],
            game_arguments: vec!["--tweakClass".to_string(), "é ünïcode".to_string()],
            min_java_version,
        }
    }

    fn assert_unreadable(dir: &Path, target: &ResolutionTarget, contents: &str) {
        let path = loader_metadata_cache_path(dir, target).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        match read_cached_loader_metadata(dir, target).unwrap() {
            CachedLoaderMetadata::Unreadable(_) => {}
            other => panic!("{contents:?} should be unreadable, got {other:?}"),
        }
    }

    #[test]
    fn written_entry_reads_back_equal() {
        let dir = TestDir::new("roundtrip");
        for (mod_loader, min_java) in [
            (ModLoader::Fabric, Some(17)),
            (ModLoader::NeoForge, None),
            (ModLoader::Forge, Some(8)),
            (ModLoader::Quilt, None),
        ] {
            let target = target("1.20.1", mod_loader);
            let metadata = sample_metadata("1.20.1", mod_loader, min_java);
            write_cached_loader_metadata(&dir.0, &target, &metadata).unwrap();
            assert_eq!(
                read_cached_loader_metadata(&dir.0, &target).unwrap(),
                CachedLoaderMetadata::Found(metadata)
            );
        }
    }

    #[test]
    fn entry_lives_where_the_offline_guard_looks() {
        let dir = TestDir::new("path");
        let target = target("1.20.1", ModLoader::Fabric);
        write_cached_loader_metadata(
            &dir.0,
            &target,
            &sample_metadata("1.20.1", ModLoader::Fabric, None),
        )
        .unwrap();
        assert!(dir.0.join("1.20.1").join("fabric.json").is_file());
        // Nessun `.part` lasciato dalla scrittura atomica.
        assert_eq!(std::fs::read_dir(dir.0.join("1.20.1")).unwrap().count(), 1);
    }

    #[test]
    fn missing_entry_is_missing() {
        let dir = TestDir::new("missing");
        assert_eq!(
            read_cached_loader_metadata(&dir.0, &target("1.20.1", ModLoader::Fabric)).unwrap(),
            CachedLoaderMetadata::Missing
        );
    }

    #[test]
    fn truncated_entry_is_unreadable() {
        let dir = TestDir::new("truncated");
        let target = target("1.20.1", ModLoader::Fabric);
        let full = serde_json::to_string(&to_cache_file(&sample_metadata(
            "1.20.1",
            ModLoader::Fabric,
            Some(17),
        )))
        .unwrap();
        assert_unreadable(&dir.0, &target, &full[..full.len() / 2]);
    }

    #[test]
    fn empty_entry_is_unreadable() {
        let dir = TestDir::new("empty");
        assert_unreadable(&dir.0, &target("1.20.1", ModLoader::Fabric), "");
    }

    #[test]
    fn entry_of_an_unknown_format_version_is_unreadable() {
        let dir = TestDir::new("version");
        let target = target("1.20.1", ModLoader::Fabric);
        let mut file = to_cache_file(&sample_metadata("1.20.1", ModLoader::Fabric, Some(17)));
        file.format_version = LOADER_METADATA_CACHE_FORMAT + 1;
        assert_unreadable(&dir.0, &target, &serde_json::to_string(&file).unwrap());
    }

    #[test]
    fn entry_written_for_another_combination_is_unreadable() {
        let dir = TestDir::new("foreign");
        let target = target("1.20.1", ModLoader::Fabric);
        let file = to_cache_file(&sample_metadata("1.21", ModLoader::Fabric, Some(21)));
        assert_unreadable(&dir.0, &target, &serde_json::to_string(&file).unwrap());
    }

    #[test]
    fn the_key_separates_combinations() {
        let dir = TestDir::new("key");
        let fabric = target("1.20.1", ModLoader::Fabric);
        write_cached_loader_metadata(
            &dir.0,
            &fabric,
            &sample_metadata("1.20.1", ModLoader::Fabric, Some(17)),
        )
        .unwrap();
        for other in [
            target("1.20.1", ModLoader::Quilt),
            target("1.20.1", ModLoader::NeoForge),
            target("1.21", ModLoader::Fabric),
        ] {
            assert_eq!(
                read_cached_loader_metadata(&dir.0, &other).unwrap(),
                CachedLoaderMetadata::Missing,
                "{other:?}"
            );
        }
    }

    #[test]
    fn a_version_that_is_not_one_path_segment_names_no_entry() {
        let dir = TestDir::new("traversal");
        let target = target("../escape", ModLoader::Fabric);
        assert!(read_cached_loader_metadata(&dir.0, &target).is_err());
        assert!(write_cached_loader_metadata(
            &dir.0,
            &target,
            &sample_metadata("../escape", ModLoader::Fabric, None),
        )
        .is_err());
    }

    /// Esegue `loader_metadata_for_launch` con una fetch finta: conta le
    /// chiamate e raccoglie il log.
    async fn launch_with(
        dir: &Path,
        target: &ResolutionTarget,
        plays_offline: bool,
        fetched: Result<LoaderMetadata>,
    ) -> (Result<LoaderMetadata>, u32, Vec<String>) {
        let calls = Cell::new(0);
        let lines = RefCell::new(Vec::new());
        let result = loader_metadata_for_launch(
            dir,
            &dir.join("instance"),
            target,
            plays_offline,
            || {
                calls.set(calls.get() + 1);
                async move { fetched }
            },
            |line| lines.borrow_mut().push(line),
        )
        .await;
        (result, calls.get(), lines.into_inner())
    }

    #[tokio::test]
    async fn offline_reads_the_entry_and_never_fetches() {
        let dir = TestDir::new("offline-hit");
        let target = target("1.20.1", ModLoader::Fabric);
        let saved = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        write_cached_loader_metadata(&dir.0, &target, &saved).unwrap();

        let (result, calls, lines) =
            launch_with(&dir.0, &target, true, Err(anyhow!("network touched"))).await;
        assert_eq!(result.unwrap(), saved);
        assert_eq!(calls, 0);
        assert!(lines
            .iter()
            .any(|line| line.contains("Fabric loader 0.16.10") && line.contains("1.20.1")));
    }

    /// review-lancio-1: la voce di combinazione l'ha riscritta il lancio online
    /// di un'altra mod list con un loader più nuovo; offline questa mod list usa
    /// il loader di cui la sua istanza ha i file.
    #[tokio::test]
    async fn offline_prefers_the_loader_this_instance_was_launched_with() {
        let dir = TestDir::new("offline-instance");
        let target = target("1.20.1", ModLoader::Fabric);
        let mut newer = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        newer.loader_version = "0.16.14".to_string();
        write_cached_loader_metadata(&dir.0, &target, &newer).unwrap();
        let own = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        write_instance_loader_metadata(&dir.0.join("instance"), &own).unwrap();

        let (result, calls, lines) =
            launch_with(&dir.0, &target, true, Err(anyhow!("network touched"))).await;
        assert_eq!(result.unwrap(), own);
        assert_eq!(calls, 0);
        assert!(lines
            .iter()
            .any(|line| line.contains("0.16.10") && line.contains("this mod list")));
    }

    #[tokio::test]
    async fn offline_with_a_damaged_instance_copy_uses_the_combination_entry() {
        let dir = TestDir::new("offline-instance-damaged");
        let target = target("1.20.1", ModLoader::Fabric);
        let saved = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        write_cached_loader_metadata(&dir.0, &target, &saved).unwrap();
        let copy = instance_loader_metadata_path(&dir.0.join("instance"));
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, "{").unwrap();

        let (result, calls, lines) =
            launch_with(&dir.0, &target, true, Err(anyhow!("network touched"))).await;
        assert_eq!(result.unwrap(), saved);
        assert_eq!(calls, 0);
        assert!(lines.iter().any(|line| line.contains("cannot be read")));
    }

    /// Un'istanza copiata da un'altra combinazione non presta il suo loader.
    #[test]
    fn an_instance_copy_of_another_combination_is_unreadable() {
        let dir = TestDir::new("instance-other-combo");
        write_instance_loader_metadata(&dir.0, &sample_metadata("1.21.1", ModLoader::Fabric, None))
            .unwrap();
        assert!(matches!(
            read_instance_loader_metadata(&dir.0, &target("1.20.1", ModLoader::Fabric)),
            CachedLoaderMetadata::Unreadable(_)
        ));
    }

    #[test]
    fn the_not_installed_message_names_the_loader_and_what_to_do() {
        let metadata = sample_metadata("1.20.1", ModLoader::Fabric, None);
        let message =
            loader_not_installed_message(&metadata, Path::new("libraries/net/fabricmc/x.jar"));
        assert!(
            message.contains("Fabric 0.16.10 for Minecraft 1.20.1"),
            "{message}"
        );
        assert!(
            message.contains("not installed in this mod list"),
            "{message}"
        );
        assert!(
            message.contains("libraries/net/fabricmc/x.jar"),
            "{message}"
        );
        assert!(
            message.contains("once with the account online"),
            "{message}"
        );
        assert!(!message.to_lowercase().contains("network"), "{message}");
    }

    #[tokio::test]
    async fn offline_without_an_entry_says_so_and_never_fetches() {
        let dir = TestDir::new("offline-miss");
        let target = target("1.20.1", ModLoader::Fabric);
        let (result, calls, _) = launch_with(
            &dir.0,
            &target,
            true,
            Ok(sample_metadata("1.20.1", ModLoader::Fabric, None)),
        )
        .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("has never been launched online"),
            "{message}"
        );
        assert!(message.contains("Fabric 1.20.1"), "{message}");
        assert_eq!(calls, 0);
        // Niente scritto: offline la cache si legge soltanto.
        assert!(!dir.0.join("1.20.1").exists());
    }

    #[tokio::test]
    async fn offline_with_a_damaged_entry_says_so_and_never_fetches() {
        let dir = TestDir::new("offline-damaged");
        let target = target("1.20.1", ModLoader::Forge);
        let path = loader_metadata_cache_path(&dir.0, &target).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"format_version\":1,").unwrap();

        let (result, calls, lines) = launch_with(
            &dir.0,
            &target,
            true,
            Ok(sample_metadata("1.20.1", ModLoader::Forge, None)),
        )
        .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("Forge 1.20.1 has never been launched online"));
        assert_eq!(calls, 0);
        assert!(lines.iter().any(|line| line.contains("cannot be read")));
    }

    #[tokio::test]
    async fn online_fetches_even_with_an_entry_and_overwrites_it() {
        let dir = TestDir::new("online-hit");
        let target = target("1.20.1", ModLoader::Fabric);
        let old = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        write_cached_loader_metadata(&dir.0, &target, &old).unwrap();
        let mut fresh = old.clone();
        fresh.loader_version = "0.17.0".to_string();

        let (result, calls, _) = launch_with(&dir.0, &target, false, Ok(fresh.clone())).await;
        assert_eq!(result.unwrap(), fresh);
        assert_eq!(calls, 1);
        assert_eq!(
            read_cached_loader_metadata(&dir.0, &target).unwrap(),
            CachedLoaderMetadata::Found(fresh)
        );
    }

    #[tokio::test]
    async fn online_writes_the_entry_the_network_answered() {
        let dir = TestDir::new("online-miss");
        let target = target("1.20.1", ModLoader::NeoForge);
        let fetched = sample_metadata("1.20.1", ModLoader::NeoForge, None);
        let (result, calls, _) = launch_with(&dir.0, &target, false, Ok(fetched.clone())).await;
        assert_eq!(result.unwrap(), fetched);
        assert_eq!(calls, 1);
        assert_eq!(
            read_cached_loader_metadata(&dir.0, &target).unwrap(),
            CachedLoaderMetadata::Found(fetched)
        );
    }

    #[tokio::test]
    async fn online_network_error_never_falls_back_to_the_entry() {
        let dir = TestDir::new("online-error");
        let target = target("1.20.1", ModLoader::Fabric);
        let saved = sample_metadata("1.20.1", ModLoader::Fabric, Some(17));
        write_cached_loader_metadata(&dir.0, &target, &saved).unwrap();

        let (result, calls, _) = launch_with(
            &dir.0,
            &target,
            false,
            Err(anyhow!("failed to query Fabric metadata")),
        )
        .await;
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "failed to query Fabric metadata"
        );
        assert_eq!(calls, 1);
        // La voce di prima resta quella che era.
        assert_eq!(
            read_cached_loader_metadata(&dir.0, &target).unwrap(),
            CachedLoaderMetadata::Found(saved)
        );
    }

    #[tokio::test]
    async fn online_launch_survives_a_cache_that_cannot_be_written() {
        let dir = TestDir::new("online-unwritable");
        // Un file al posto della cartella: la scrittura fallisce.
        let cache_dir = dir.0.join("loader-metadata");
        std::fs::write(&cache_dir, "not a directory").unwrap();
        let target = target("1.20.1", ModLoader::Fabric);
        let fetched = sample_metadata("1.20.1", ModLoader::Fabric, None);

        let (result, calls, lines) =
            launch_with(&cache_dir, &target, false, Ok(fetched.clone())).await;
        assert_eq!(result.unwrap(), fetched);
        assert_eq!(calls, 1);
        assert!(lines.iter().any(|line| line.contains("Could not save")));
    }

    #[tokio::test]
    async fn vanilla_neither_reads_nor_writes_the_cache() {
        let dir = TestDir::new("vanilla");
        let target = target("1.20.1", ModLoader::Vanilla);
        let vanilla = LoaderMetadata {
            mod_loader: ModLoader::Vanilla,
            minecraft_version: "1.20.1".to_string(),
            loader_version: "1.20.1".to_string(),
            main_class: String::new(),
            libraries: vec![],
            maven_files: vec![],
            jvm_arguments: vec![],
            game_arguments: vec![],
            min_java_version: None,
        };
        for plays_offline in [true, false] {
            let (result, calls, _) =
                launch_with(&dir.0, &target, plays_offline, Ok(vanilla.clone())).await;
            assert_eq!(result.unwrap(), vanilla);
            assert_eq!(calls, 1);
        }
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
