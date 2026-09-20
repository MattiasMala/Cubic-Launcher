//! Condivisione delle impostazioni di gioco fra i livelli (D66, D67, D70, D71, D72).
//!
//! Tre livelli, ognuno semina quello sotto **una volta sola** e poi i due
//! divergono: globale → modlist alla creazione della modlist, modlist →
//! istanza al primo avvio di quell'istanza. Niente symlink: quello che
//! l'utente cambia in gioco resta suo.
//!
//! Due regole che sembrano dettagli e non lo sono:
//!
//! - **il filtro guarda la sorgente** (D71). Per sapere quali nomi erano
//!   vanilla quando quei valori sono stati scritti si risale dal numero nella
//!   riga `version:` al jar che l'ha prodotto. Filtrare sui nomi del bersaglio
//!   butterebbe via proprio le chiavi che il DataFixer del gioco saprebbe
//!   convertire;
//! - **le rinomine non le facciamo noi** (D69). `graphicsMode` non è diventato
//!   `graphicsPreset`: è stato spezzato in quattro campi con valori diversi per
//!   preset, e quella tabella il gioco ce l'ha già. La nostra sarebbe una copia
//!   sempre indietro.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::options_file::{OptionsFile, VERSION_KEY};
use crate::options_keys::{
    find_version_for_data_version, load_or_derive, VanillaOptionKeys,
};
use crate::path_safety::validate_path_component;

pub const OPTIONS_FILENAME: &str = "options.txt";

/// Chiavi vanilla che descrivono lo **stato** di un'istanza, non una
/// preferenza. Il filtro le lascerebbe passare — sono vanilla — ma seminarle
/// racconta bugie all'istanza nuova (D72). Il motivo sta accanto a ognuna
/// perché la prossima persona possa giudicare se aggiungerne o toglierne.
///
/// `version` non è qui: non è escluso, è **riscritto** dalla semina, ed è
/// l'unica riga che deve esserci sempre.
pub const INTERNAL_STATE_KEYS: &[(&str, &str)] = &[
    (
        "joinedFirstServer",
        "dice che l'utente è già entrato in un server: a un'istanza nuova mentirebbe",
    ),
    (
        "lastServer",
        "l'ultimo indirizzo usato: comparirebbe in un'istanza che non ci è mai entrata",
    ),
    (
        "tutorialStep",
        "a che punto è il tutorial di quella installazione",
    ),
    (
        "startedCleanly",
        "se l'ultima uscita del gioco è stata pulita: stato del processo, non preferenza",
    ),
    (
        "onboardAccessibility",
        "se la schermata di accessibilità del primo avvio è già stata mostrata",
    ),
    (
        "skipMultiplayerWarning",
        "un avviso già accettato in quella installazione",
    ),
    (
        "skipRealms32bitWarning",
        "un avviso già accettato in quella installazione",
    ),
    (
        "telemetryOptInExtra",
        "una scelta sulla telemetria: non la propaghiamo al posto dell'utente",
    ),
    (
        "hideBundleTutorial",
        "un suggerimento già chiuso in quella installazione",
    ),
];

pub fn is_internal_state(key: &str) -> bool {
    INTERNAL_STATE_KEYS
        .iter()
        .any(|(candidate, _)| *candidate == key)
}

// ---------------------------------------------------------------------------
// Dove vivono i file
// ---------------------------------------------------------------------------

/// `<root>/options.txt`. Accanto al database, nella cartella che l'utente apre
/// già per guardare i log: un file di testo lì è la cosa più facile da trovare
/// di tutto l'albero.
pub fn global_options_path(launcher_paths: &LauncherPaths) -> PathBuf {
    launcher_paths.root_dir().join(OPTIONS_FILENAME)
}

/// `<root>/mod-lists/<nome>/options.txt`, accanto a `rules.json` e
/// `resourcepacks.json`, che sono già file che l'utente può aprire. **Non** in
/// `.cubic/`, che è roba interna e derivata ed è nascosta.
pub fn modlist_options_path(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
) -> anyhow::Result<PathBuf> {
    validate_path_component(modlist_name)?;
    Ok(launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(OPTIONS_FILENAME))
}

pub fn instance_options_path(instance_root: &Path) -> PathBuf {
    instance_root.join(OPTIONS_FILENAME)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "camelCase")]
pub enum OptionsScope {
    Global,
    Modlist {
        modlist: String,
    },
    Instance {
        modlist: String,
        instance: String,
    },
}

impl OptionsScope {
    pub fn path(&self, launcher_paths: &LauncherPaths) -> anyhow::Result<PathBuf> {
        match self {
            OptionsScope::Global => Ok(global_options_path(launcher_paths)),
            OptionsScope::Modlist { modlist } => modlist_options_path(launcher_paths, modlist),
            OptionsScope::Instance { modlist, instance } => {
                validate_path_component(modlist)?;
                validate_path_component(instance)?;
                Ok(launcher_paths
                    .modlists_dir()
                    .join(modlist)
                    .join("instances")
                    .join(instance)
                    .join(OPTIONS_FILENAME))
            }
        }
    }

    /// Le istanze non si scrivono dal launcher: quel file è del gioco. Il
    /// launcher lo crea al primo avvio e da lì in poi non lo tocca più (D66).
    pub fn is_writable(&self) -> bool {
        !matches!(self, OptionsScope::Instance { .. })
    }
}

// ---------------------------------------------------------------------------
// L'esito di una semina
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockReason {
    /// Non è una chiave vanilla della versione che ha scritto i valori: quasi
    /// sempre un keybind di un mod.
    NotVanilla,
    /// È vanilla ma descrive lo stato di quella installazione.
    InternalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedKey {
    pub key: String,
    pub reason: BlockReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedReport {
    pub source: String,
    pub target: String,
    pub data_version: i64,
    pub source_version_id: String,
    pub seeded: usize,
    pub blocked: Vec<BlockedKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SeedStatus {
    Seeded { report: SeedReport },
    SkippedTargetExists { target: String },
    SkippedNoSource { source: String },
    /// La semina non si poteva fare in sicurezza. Non seminare è sempre la
    /// direzione giusta: l'istanza tiene i suoi valori e non si rompe niente.
    Refused { reason: String },
}

impl SeedStatus {
    /// Una riga per il log di lancio: la semina passa da lì e deve lasciare
    /// traccia anche quando non fa niente.
    pub fn describe(&self) -> String {
        match self {
            SeedStatus::Seeded { report } => format!(
                "seminate {} impostazioni da {} (version:{}, {}) verso {}; bloccate {}",
                report.seeded,
                report.source,
                report.data_version,
                report.source_version_id,
                report.target,
                report.blocked.len()
            ),
            SeedStatus::SkippedTargetExists { target } => {
                format!("niente da seminare: {target} esiste già")
            }
            SeedStatus::SkippedNoSource { source } => {
                format!("niente da seminare: {source} non esiste")
            }
            SeedStatus::Refused { reason } => format!("semina rifiutata: {reason}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Il filtro
// ---------------------------------------------------------------------------

pub fn filter_for_seed(
    source: &OptionsFile,
    source_keys: &VanillaOptionKeys,
) -> (OptionsFile, Vec<BlockedKey>) {
    let mut seeded = OptionsFile::new();
    let mut blocked: Vec<BlockedKey> = Vec::new();

    for (key, value) in source.iter() {
        if key == VERSION_KEY {
            continue;
        }
        if is_internal_state(key) {
            blocked.push(BlockedKey {
                key: key.to_string(),
                reason: BlockReason::InternalState,
            });
            continue;
        }
        if !source_keys.accepts(key) {
            blocked.push(BlockedKey {
                key: key.to_string(),
                reason: BlockReason::NotVanilla,
            });
            continue;
        }
        seeded.set(key, value);
    }

    // La riga `version:` va sempre scritta, e porta la DataVersion di chi ha
    // scritto i valori (D70). Un file che non ce l'ha vale **DataVersion 0**
    // per il gioco, e a DataVersion 0 si applicano tutti i datafixer delle
    // opzioni, compreso `OptionsKeyLwjgl3Fix` (registrato a 1344), che rimappa
    // i codici dei tasti da LWJGL2 a LWJGL3. È il danno peggiore che questa
    // feature possa fare, e costa una riga evitarlo.
    seeded.set_first(VERSION_KEY, &source_keys.data_version.to_string());

    (seeded, blocked)
}

// ---------------------------------------------------------------------------
// Seminare
// ---------------------------------------------------------------------------

fn seed_into(
    launcher_paths: &LauncherPaths,
    source_path: &Path,
    target_path: &Path,
    overwrite: bool,
) -> SeedStatus {
    if !overwrite && target_path.exists() {
        return SeedStatus::SkippedTargetExists {
            target: target_path.display().to_string(),
        };
    }
    if !source_path.is_file() {
        return SeedStatus::SkippedNoSource {
            source: source_path.display().to_string(),
        };
    }

    let source = match OptionsFile::read(source_path) {
        Ok(source) => source,
        Err(error) => {
            return SeedStatus::Refused {
                reason: error.to_string(),
            }
        }
    };

    let Some(data_version) = source.data_version() else {
        return SeedStatus::Refused {
            reason: format!(
                "{} non ha una riga `{VERSION_KEY}:` leggibile, e senza quella non si sa \
                 quali nomi fossero vanilla quando i valori sono stati scritti",
                source_path.display()
            ),
        };
    };

    let version_id = match find_version_for_data_version(launcher_paths, data_version) {
        Ok(Some(version_id)) => version_id,
        Ok(None) => {
            return SeedStatus::Refused {
                reason: format!(
                    "nessun client.jar in cache ha DataVersion {data_version}: \
                     non si può derivare l'insieme vanilla della sorgente"
                ),
            }
        }
        Err(error) => {
            return SeedStatus::Refused {
                reason: error.to_string(),
            }
        }
    };

    let source_keys = match load_or_derive(launcher_paths, &version_id) {
        Ok(keys) => keys,
        Err(error) => {
            return SeedStatus::Refused {
                reason: error.to_string(),
            }
        }
    };

    let (seeded, blocked) = filter_for_seed(&source, &source_keys);
    if let Err(error) = seeded.write(target_path) {
        return SeedStatus::Refused {
            reason: error.to_string(),
        };
    }

    SeedStatus::Seeded {
        report: SeedReport {
            source: source_path.display().to_string(),
            target: target_path.display().to_string(),
            data_version,
            source_version_id: version_id,
            // `version:` non è un'impostazione seminata: è il cancello.
            seeded: seeded.len().saturating_sub(1),
            blocked,
        },
    }
}

/// Alla creazione di una modlist: il globale è il modello per una modlist nuova.
pub fn seed_modlist_from_global(launcher_paths: &LauncherPaths, modlist_name: &str) -> SeedStatus {
    let target = match modlist_options_path(launcher_paths, modlist_name) {
        Ok(target) => target,
        Err(error) => {
            return SeedStatus::Refused {
                reason: error.to_string(),
            }
        }
    };
    seed_into(
        launcher_paths,
        &global_options_path(launcher_paths),
        &target,
        false,
    )
}

/// Al primo avvio di un'istanza, e mai più: `seed_into` non sovrascrive, e dal
/// secondo avvio il file c'è perché l'abbiamo scritto noi o perché il gioco lo
/// ha riscritto.
pub fn seed_instance_from_modlist(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    instance_root: &Path,
) -> SeedStatus {
    let source = match modlist_options_path(launcher_paths, modlist_name) {
        Ok(source) => source,
        Err(error) => {
            return SeedStatus::Refused {
                reason: error.to_string(),
            }
        }
    };
    seed_into(
        launcher_paths,
        &source,
        &instance_options_path(instance_root),
        false,
    )
}

// ---------------------------------------------------------------------------
// Quello che serve alla fase 2
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OptionKind {
    Plain,
    Keybind,
    SoundCategory,
    ModelPart,
}

fn kind_of(key: &str) -> OptionKind {
    if key.starts_with("key_") {
        OptionKind::Keybind
    } else if key.starts_with("soundCategory_") {
        OptionKind::SoundCategory
    } else if key.starts_with("modelPart_") {
        OptionKind::ModelPart
    } else {
        OptionKind::Plain
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionEntryView {
    pub key: String,
    pub value: String,
    pub kind: OptionKind,
    pub vanilla: bool,
    pub internal_state: bool,
    /// Nome della schermata del gioco che nomina la chiave, quando il jar
    /// permette di ricavarlo. Sui jar offuscati è sempre assente.
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedOptionsView {
    pub path: String,
    pub exists: bool,
    pub writable: bool,
    pub data_version: Option<i64>,
    pub version_id: Option<String>,
    /// Perché non sappiamo dire cosa è vanilla, quando non lo sappiamo.
    pub derivation_error: Option<String>,
    pub entries: Vec<OptionEntryView>,
}

pub fn load_shared_options(
    launcher_paths: &LauncherPaths,
    scope: &OptionsScope,
) -> anyhow::Result<SharedOptionsView> {
    let path = scope.path(launcher_paths)?;
    let exists = path.is_file();

    let file = if exists {
        OptionsFile::read(&path)?
    } else {
        OptionsFile::new()
    };

    let data_version = file.data_version();
    let mut version_id = None;
    let mut derivation_error = None;
    let mut keys: Option<VanillaOptionKeys> = None;

    if let Some(data_version) = data_version {
        match find_version_for_data_version(launcher_paths, data_version) {
            Ok(Some(found)) => match load_or_derive(launcher_paths, &found) {
                Ok(derived) => {
                    version_id = Some(found);
                    keys = Some(derived);
                }
                Err(error) => derivation_error = Some(error.to_string()),
            },
            Ok(None) => {
                derivation_error = Some(format!(
                    "nessun client.jar in cache ha DataVersion {data_version}"
                ))
            }
            Err(error) => derivation_error = Some(error.to_string()),
        }
    } else if exists {
        derivation_error = Some(format!("{} non ha una riga `{VERSION_KEY}:`", path.display()));
    }

    let entries = file
        .iter()
        .map(|(key, value)| OptionEntryView {
            key: key.to_string(),
            value: value.to_string(),
            kind: kind_of(key),
            vanilla: keys.as_ref().is_some_and(|keys| keys.accepts(key)),
            internal_state: is_internal_state(key),
            group: keys
                .as_ref()
                .and_then(|keys| keys.groups.get(key))
                .cloned(),
        })
        .collect();

    Ok(SharedOptionsView {
        path: path.display().to_string(),
        exists,
        writable: scope.is_writable(),
        data_version,
        version_id,
        derivation_error,
        entries,
    })
}

// ---------------------------------------------------------------------------
// Comandi
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn load_shared_options_command(
    launcher_paths: State<'_, LauncherPaths>,
    scope: OptionsScope,
) -> Result<SharedOptionsView, String> {
    load_shared_options(&launcher_paths, &scope).map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedOptionsEntryInput {
    pub key: String,
    pub value: String,
}

#[tauri::command]
pub fn save_shared_options_command(
    launcher_paths: State<'_, LauncherPaths>,
    scope: OptionsScope,
    entries: Vec<SharedOptionsEntryInput>,
) -> Result<(), String> {
    if !scope.is_writable() {
        return Err(
            "l'options.txt di un'istanza lo scrive il gioco: il launcher lo crea al primo \
             avvio e poi non lo tocca più"
                .to_string(),
        );
    }

    let path = scope.path(&launcher_paths).map_err(|e| e.to_string())?;
    let mut file = OptionsFile::new();
    for entry in entries {
        file.set(&entry.key, &entry.value);
    }
    file.write(&path).map_err(|error| error.to_string())
}

/// Promozione all'insù (istanza → modlist, modlist → globale) e copia fra
/// modlist: sono lo stesso gesto della semina, in un'altra direzione, e
/// sovrascrivono perché è l'utente a chiederlo.
#[tauri::command]
pub fn promote_options_command(
    launcher_paths: State<'_, LauncherPaths>,
    from: OptionsScope,
    to: OptionsScope,
) -> Result<SeedStatus, String> {
    if !to.is_writable() {
        return Err("non si promuove dentro un'istanza".to_string());
    }

    let source = from.path(&launcher_paths).map_err(|e| e.to_string())?;
    let target = to.path(&launcher_paths).map_err(|e| e.to_string())?;

    Ok(seed_into(&launcher_paths, &source, &target, true))
}

#[tauri::command]
pub fn derive_option_keys_command(
    launcher_paths: State<'_, LauncherPaths>,
    version_id: String,
) -> Result<VanillaOptionKeys, String> {
    load_or_derive(&launcher_paths, &version_id).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn keys_for(data_version: i64, plain: &[&str], keybinds: &[&str]) -> VanillaOptionKeys {
        VanillaOptionKeys {
            format_version: crate::options_keys::DERIVATION_FORMAT_VERSION,
            version_id: "1.20.1".into(),
            data_version,
            options_class: "enr.class".into(),
            plain: plain.iter().map(|key| key.to_string()).collect(),
            keybinds: keybinds.iter().map(|key| key.to_string()).collect(),
            sound_categories: ["master"].iter().map(|key| key.to_string()).collect(),
            model_parts: BTreeSet::new(),
            groups: BTreeMap::new(),
        }
    }

    #[test]
    fn mod_keys_never_pass_the_filter() {
        let source = OptionsFile::parse(concat!(
            "version:3465\n",
            "fov:0.5\n",
            "key_key.attack:key.mouse.left\n",
            "key_key.epicfight.dodge:key.keyboard.left.alt\n",
            "key_iris.keybind.reload:key.keyboard.r\n",
            "soundCategory_master:0.5\n",
        ));
        let keys = keys_for(3465, &["fov"], &["key.attack"]);

        let (seeded, blocked) = filter_for_seed(&source, &keys);

        assert_eq!(seeded.get("fov"), Some("0.5"));
        assert_eq!(seeded.get("key_key.attack"), Some("key.mouse.left"));
        assert_eq!(seeded.get("soundCategory_master"), Some("0.5"));
        assert_eq!(seeded.get("key_key.epicfight.dodge"), None);
        assert_eq!(seeded.get("key_iris.keybind.reload"), None);

        let blocked_keys: Vec<&str> = blocked.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(
            blocked_keys,
            vec!["key_key.epicfight.dodge", "key_iris.keybind.reload"]
        );
        assert!(blocked
            .iter()
            .all(|entry| entry.reason == BlockReason::NotVanilla));
    }

    #[test]
    fn internal_state_is_blocked_even_though_it_is_vanilla() {
        let source = OptionsFile::parse(concat!(
            "version:3465\n",
            "fov:0.5\n",
            "lastServer:mc.example.invalid\n",
            "tutorialStep:none\n",
        ));
        let keys = keys_for(3465, &["fov", "lastServer", "tutorialStep"], &[]);

        let (seeded, blocked) = filter_for_seed(&source, &keys);

        assert_eq!(seeded.get("lastServer"), None);
        assert_eq!(seeded.get("tutorialStep"), None);
        assert!(blocked
            .iter()
            .all(|entry| entry.reason == BlockReason::InternalState));
    }

    #[test]
    fn the_version_line_is_always_written_first_and_comes_from_the_source() {
        let source = OptionsFile::parse("version:3465\nfov:0.5\n");
        let keys = keys_for(3465, &["fov"], &[]);

        let (seeded, _) = filter_for_seed(&source, &keys);

        assert_eq!(seeded.render(), "version:3465\nfov:0.5\n");
    }

    #[test]
    fn a_source_without_a_version_line_is_refused() {
        let root = temp_root("noversion");
        let launcher_paths = LauncherPaths::new(root.clone());
        let source = root.join("source.txt");
        std::fs::write(&source, "fov:0.5\n").unwrap();

        let status = seed_into(&launcher_paths, &source, &root.join("target.txt"), false);

        match status {
            SeedStatus::Refused { reason } => assert!(
                reason.contains("version"),
                "unexpected refusal: {reason}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!root.join("target.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_data_version_is_refused() {
        let root = temp_root("unknown-dv");
        let launcher_paths = LauncherPaths::new(root.clone());
        let source = root.join("source.txt");
        std::fs::write(&source, "version:999999\nfov:0.5\n").unwrap();

        let status = seed_into(&launcher_paths, &source, &root.join("target.txt"), false);

        match status {
            SeedStatus::Refused { reason } => {
                assert!(reason.contains("999999"), "unexpected refusal: {reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seeding_never_touches_an_existing_target() {
        let root = temp_root("existing");
        let launcher_paths = LauncherPaths::new(root.clone());
        let source = root.join("source.txt");
        let target = root.join("target.txt");
        std::fs::write(&source, "version:3465\nfov:0.5\n").unwrap();
        std::fs::write(&target, "fov:0.9\n").unwrap();

        let status = seed_into(&launcher_paths, &source, &target, false);

        assert!(matches!(status, SeedStatus::SkippedTargetExists { .. }));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fov:0.9\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn instance_scope_is_not_writable_from_the_launcher() {
        assert!(OptionsScope::Global.is_writable());
        assert!(OptionsScope::Modlist {
            modlist: "Drehmal".into()
        }
        .is_writable());
        assert!(!OptionsScope::Instance {
            modlist: "Drehmal".into(),
            instance: "1.20.1-forge".into()
        }
        .is_writable());
    }

    fn temp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cubic-options-share-{tag}-{stamp}"));
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    /// Il `client.jar` vero, **copiato**: la radice di prova sta in `/tmp`, che
    /// qui è tmpfs, e un hard link fra filesystem diversi fallisce con EXDEV —
    /// silenziosamente, saltando il test. Copiare costa 22 MiB e qualche
    /// decina di millisecondi, e ha il vantaggio di non condividere l'inode con
    /// la cache vera.
    fn copy_real_jar(root: &Path, version: &str) -> bool {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return false;
        };
        let source = home
            .join(".local/share/com.cubic.launcher/cache/minecraft")
            .join(version)
            .join("client.jar");
        if !source.is_file() {
            return false;
        }
        let version_dir = root.join("cache/minecraft").join(version);
        std::fs::create_dir_all(&version_dir).expect("version dir");
        std::fs::copy(&source, version_dir.join("client.jar")).is_ok()
    }

    #[test]
    fn the_first_launch_seeds_and_the_second_leaves_the_file_alone() {
        let root = temp_root("first-launch");
        if !copy_real_jar(&root, "1.20.1") {
            eprintln!("skipping: 1.20.1/client.jar not in the local cache");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let launcher_paths = LauncherPaths::new(root.clone());

        let modlist_options = modlist_options_path(&launcher_paths, "Drehmal").unwrap();
        OptionsFile::parse(concat!(
            "version:3465\n",
            "fov:0.75\n",
            "renderDistance:16\n",
            "lastServer:mc.example.invalid\n",
            "key_key.attack:key.mouse.left\n",
            "key_key.epicfight.dodge:key.keyboard.left.alt\n",
        ))
        .write(&modlist_options)
        .unwrap();

        let instance_root = root.join("mod-lists/Drehmal/instances/1.20.1-neoforge");
        std::fs::create_dir_all(&instance_root).unwrap();

        let first = seed_instance_from_modlist(&launcher_paths, "Drehmal", &instance_root);
        let report = match &first {
            SeedStatus::Seeded { report } => report,
            other => panic!("expected a seed, got {other:?}"),
        };
        assert_eq!(report.source_version_id, "1.20.1");
        assert_eq!(report.data_version, 3465);
        assert_eq!(report.seeded, 3);

        let seeded_text =
            std::fs::read_to_string(instance_options_path(&instance_root)).unwrap();
        assert_eq!(
            seeded_text,
            "version:3465\nfov:0.75\nrenderDistance:16\nkey_key.attack:key.mouse.left\n"
        );

        let second = seed_instance_from_modlist(&launcher_paths, "Drehmal", &instance_root);
        assert!(matches!(second, SeedStatus::SkippedTargetExists { .. }));
        assert_eq!(
            std::fs::read_to_string(instance_options_path(&instance_root)).unwrap(),
            seeded_text
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_modlist_inherits_the_global_file() {
        let root = temp_root("modlist-seed");
        if !copy_real_jar(&root, "1.20.1") {
            eprintln!("skipping: 1.20.1/client.jar not in the local cache");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let launcher_paths = LauncherPaths::new(root.clone());

        OptionsFile::parse("version:3465\nguiScale:2\nkey_zoomify.key.zoom:key.keyboard.c\n")
            .write(&global_options_path(&launcher_paths))
            .unwrap();

        let status = seed_modlist_from_global(&launcher_paths, "Nuova");
        let report = match &status {
            SeedStatus::Seeded { report } => report,
            other => panic!("expected a seed, got {other:?}"),
        };

        assert_eq!(report.seeded, 1);
        assert_eq!(report.blocked.len(), 1);
        assert_eq!(report.blocked[0].key, "key_zoomify.key.zoom");
        assert_eq!(
            std::fs::read_to_string(modlist_options_path(&launcher_paths, "Nuova").unwrap())
                .unwrap(),
            "version:3465\nguiScale:2\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
