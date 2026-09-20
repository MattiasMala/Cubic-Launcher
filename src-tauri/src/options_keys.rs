//! L'insieme delle chiavi vanilla di `options.txt`, ricavato dal `client.jar`
//! della versione (D68).
//!
//! Il problema che risolve: nel file, `key_key.hotbar.1` (vanilla) e
//! `key_key.epicfight.dodge` (di un mod) hanno la stessa forma. Non esiste una
//! regola sintattica che li separi — nei dati veri ci sono anche
//! `key_iris.keybind.reload` e `key_zoomify.key.zoom`, che non cominciano
//! nemmeno per `key.`. E il file di lingua non basta: copre i keybind ma solo
//! 38 chiavi semplici su 86, perché `graphicsMode` nel file non si chiama
//! `options.graphics`.
//!
//! Quello che regge è leggere i nomi dalla classe del gioco che scrive il file.
//! L'ancora è il letterale `options.txt`, che sta nel constant pool di **una
//! sola** classe sia nei jar offuscati (1.20.1 → `enr.class`) sia in quelli con
//! i nomi veri (26.3 → `net/minecraft/client/Options.class`). Non è circolare:
//! non usa nessuna conoscenza dell'elenco delle chiavi, che è quello che
//! stiamo cercando.
//!
//! È analisi statica di bytecode contro un formato interno che Mojang non
//! promette, quindi è fragile per costruzione. Quello che la rende sicura non è
//! l'euristica: è [`VanillaOptionKeys::verify`], che fa fallire la derivazione
//! invece di lasciar passare un insieme monco. Un insieme monco non si vede,
//! e seminerebbe di meno in silenzio.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::launcher_paths::LauncherPaths;
use crate::options_file::VERSION_KEY;
use crate::path_safety::validate_path_component;

/// Cambia quando cambia il modo di derivare: invalida le cache su disco.
pub const DERIVATION_FORMAT_VERSION: u32 = 1;

const CACHE_FILENAME: &str = "options-keys.json";
const CLIENT_JAR_FILENAME: &str = "client.jar";
const ANCHOR_LITERAL: &str = "options.txt";
const LOAD_METHOD_LITERAL: &str = "Failed to load options";
const LANG_ENTRY: &str = "assets/minecraft/lang/en_us.json";
const JAR_VERSION_ENTRY: &str = "version.json";
const SCREEN_CLASS_PREFIX: &str = "net/minecraft/client/gui/screens/options/";
/// Classe base delle schermate: nomina chiavi che appartengono alle figlie.
const SCREEN_BASE_CLASS: &str = "OptionsSubScreen";

/// Quanti letterali deve avere un metodo per essere il lettore/scrittore e non
/// un accessore che per caso prende lo stesso tipo interno.
const MIN_METHOD_LITERALS: usize = 5;

/// Soglie della verifica a valle. La più bassa che abbiamo misurato è 86
/// (1.20.1); 60 lascia spazio a una versione più povera senza accettare un
/// insieme mutilato.
const MIN_PLAIN_KEYS: usize = 60;
const MIN_KEYBINDS: usize = 20;

/// Chiavi che esistono in tutte e cinque le versioni misurate. Se la
/// derivazione non le trova, ha letto la classe sbagliata o il formato è
/// cambiato sotto di noi.
const HISTORIC_CORE: [&str; 4] = ["fov", "renderDistance", "guiScale", "mouseSensitivity"];

// ---------------------------------------------------------------------------
// Il risultato
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VanillaOptionKeys {
    pub format_version: u32,
    /// L'id della versione come lo chiama Mojang (`1.20.1`, `26.3`).
    pub version_id: String,
    /// La `DataVersion` di quella versione, letta da `version.json` dentro il
    /// jar. È il numero che finisce nella riga `version:` di `options.txt`.
    pub data_version: i64,
    /// La classe da cui sono usciti i nomi, per poterlo dire nei report.
    pub options_class: String,
    /// Impostazioni semplici: `fov`, `graphicsMode`, `version`, …
    pub plain: BTreeSet<String>,
    /// Keybind **senza** il prefisso `key_` del file: `key.attack`.
    pub keybinds: BTreeSet<String>,
    /// Categorie audio senza il prefisso `soundCategory_`: `master`.
    pub sound_categories: BTreeSet<String>,
    /// Parti del modello senza il prefisso `modelPart_`: `cape`.
    pub model_parts: BTreeSet<String>,
    /// Chiave semplice → nome della schermata del gioco che la nomina.
    /// Vuota sui jar offuscati, dove i nomi dei campi non sopravvivono.
    pub groups: BTreeMap<String, String>,
}

impl VanillaOptionKeys {
    /// Dice se una riga di `options.txt` è vanilla, prefisso compreso.
    pub fn accepts(&self, file_key: &str) -> bool {
        if let Some(name) = file_key.strip_prefix("key_") {
            return self.keybinds.contains(name);
        }
        if let Some(name) = file_key.strip_prefix("soundCategory_") {
            return self.sound_categories.contains(name);
        }
        if let Some(name) = file_key.strip_prefix("modelPart_") {
            return self.model_parts.contains(name);
        }
        self.plain.contains(file_key)
    }

    pub fn total(&self) -> usize {
        self.plain.len() + self.keybinds.len() + self.sound_categories.len() + self.model_parts.len()
    }

    /// La verifica a valle (D72). Fallisce rumorosamente: è un errore del
    /// launcher, non un caso normale, e chi semina deve poter distinguere
    /// "non c'è niente da seminare" da "non so cosa seminare".
    pub fn verify(&self) -> Result<()> {
        if self.plain.len() < MIN_PLAIN_KEYS {
            bail!(
                "derivazione sospetta per {}: {} impostazioni semplici, soglia {}",
                self.version_id,
                self.plain.len(),
                MIN_PLAIN_KEYS
            );
        }
        if self.keybinds.len() < MIN_KEYBINDS {
            bail!(
                "derivazione sospetta per {}: {} keybind, soglia {}",
                self.version_id,
                self.keybinds.len(),
                MIN_KEYBINDS
            );
        }
        if self.sound_categories.is_empty() || self.model_parts.is_empty() {
            bail!(
                "derivazione sospetta per {}: {} categorie audio e {} parti del modello",
                self.version_id,
                self.sound_categories.len(),
                self.model_parts.len()
            );
        }

        let missing: Vec<&str> = HISTORIC_CORE
            .iter()
            .copied()
            .filter(|key| !self.plain.contains(*key))
            .collect();
        if !missing.is_empty() {
            bail!(
                "derivazione sospetta per {}: manca il nucleo storico ({})",
                self.version_id,
                missing.join(", ")
            );
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Lettura del jar
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JarVersionManifest {
    id: String,
    world_version: i64,
}

/// La `DataVersion` di un `client.jar`, senza derivare niente altro.
pub fn read_jar_data_version(jar_path: &Path) -> Result<i64> {
    let file = File::open(jar_path)
        .with_context(|| format!("failed to open {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read {}", jar_path.display()))?;
    let mut entry = archive
        .by_name(JAR_VERSION_ENTRY)
        .with_context(|| format!("{JAR_VERSION_ENTRY} missing from {}", jar_path.display()))?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    let manifest: JarVersionManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {JAR_VERSION_ENTRY} in {}", jar_path.display()))?;
    Ok(manifest.world_version)
}

pub fn derive_from_client_jar(jar_path: &Path) -> Result<VanillaOptionKeys> {
    let file = File::open(jar_path)
        .with_context(|| format!("failed to open {}", jar_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read {}", jar_path.display()))?;

    let mut anchor_hits: Vec<String> = Vec::new();
    let mut options_class_bytes: Option<Vec<u8>> = None;
    let mut screen_classes: Vec<(String, HashSet<String>)> = Vec::new();
    let mut lang_bytes: Option<Vec<u8>> = None;
    let mut manifest_bytes: Option<Vec<u8>> = None;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();

        if name == LANG_ENTRY || name == JAR_VERSION_ENTRY {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if name == LANG_ENTRY {
                lang_bytes = Some(bytes);
            } else {
                manifest_bytes = Some(bytes);
            }
            continue;
        }

        if !name.ends_with(".class") {
            continue;
        }
        let is_screen = name.starts_with(SCREEN_CLASS_PREFIX) && !name.contains('$');

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let Ok((pool, _)) = parse_constant_pool(&bytes) else {
            continue;
        };

        let mut carries_anchor = false;
        let mut literals: HashSet<String> = HashSet::new();
        for constant in &pool {
            if let CpEntry::Utf8(text) = constant {
                if text == ANCHOR_LITERAL {
                    carries_anchor = true;
                }
                if is_screen {
                    literals.insert(text.clone());
                }
            }
        }

        if carries_anchor {
            anchor_hits.push(name.clone());
            options_class_bytes = Some(bytes);
        }
        if is_screen {
            let simple = name
                .rsplit('/')
                .next()
                .unwrap_or(&name)
                .trim_end_matches(".class")
                .to_string();
            if simple != SCREEN_BASE_CLASS {
                screen_classes.push((simple, literals));
            }
        }
    }

    if anchor_hits.len() != 1 {
        bail!(
            "il letterale `{ANCHOR_LITERAL}` doveva stare in una classe sola di {}, invece sta in {} ({})",
            jar_path.display(),
            anchor_hits.len(),
            if anchor_hits.is_empty() {
                "nessuna".to_string()
            } else {
                anchor_hits.join(", ")
            }
        );
    }
    let options_class = anchor_hits.remove(0);
    let class_bytes = options_class_bytes
        .ok_or_else(|| anyhow!("classe delle opzioni trovata ma non letta"))?;

    let manifest_bytes = manifest_bytes
        .ok_or_else(|| anyhow!("{JAR_VERSION_ENTRY} missing from {}", jar_path.display()))?;
    let manifest: JarVersionManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("failed to parse {JAR_VERSION_ENTRY} in {}", jar_path.display()))?;

    let lang_bytes = lang_bytes
        .ok_or_else(|| anyhow!("{LANG_ENTRY} missing from {}", jar_path.display()))?;

    let (plain, keybinds) = derive_from_options_class(&class_bytes, &options_class)?;
    let (sound_categories, model_parts) = derive_from_lang(&lang_bytes)?;
    let groups = assign_groups(&plain, &screen_classes);

    let derived = VanillaOptionKeys {
        format_version: DERIVATION_FORMAT_VERSION,
        version_id: manifest.id,
        data_version: manifest.world_version,
        options_class,
        plain,
        keybinds,
        sound_categories,
        model_parts,
        groups,
    };

    derived.verify()?;
    Ok(derived)
}

fn derive_from_options_class(
    class_bytes: &[u8],
    class_name: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let (pool, after_pool) = parse_constant_pool(class_bytes)?;
    let methods = parse_methods(class_bytes, &pool, after_pool)?;

    // I metodi che leggono e scrivono le impostazioni semplici prendono un solo
    // argomento, ed è un tipo interno della classe stessa
    // (`Options$FieldAccess`, `Options$OptionAccess`, e i loro equivalenti
    // offuscati). Il nome del metodo non serve: cambia con l'offuscamento, la
    // firma no.
    let inner_argument_prefix = format!("(L{}$", class_name.trim_end_matches(".class"));

    let mut plain: BTreeSet<String> = BTreeSet::new();
    for method in &methods {
        let Some(code) = &method.code else { continue };
        if !method.descriptor.starts_with(&inner_argument_prefix)
            || !method.descriptor.ends_with(";)V")
        {
            continue;
        }
        let literals = ldc_strings(code, &pool)?;
        if literals.iter().collect::<HashSet<_>>().len() <= MIN_METHOD_LITERALS {
            continue;
        }
        for literal in literals {
            if is_option_key_shape(&literal) {
                plain.insert(literal);
            }
        }
    }

    // E anche dal metodo di caricamento. Non è ridondante: `fullscreenResolution`
    // il gioco la **legge** lì e non compare fra i letterali dei metodi sopra,
    // e lo stesso vale per gli alias legacy (`fancyGraphics`) nelle versioni in
    // cui la conversione non è ancora un datafixer. Senza questa riga quelle
    // chiavi risultano "non vanilla" e non vengono seminate: un buco che non si
    // vede, perché sbaglia in silenzio e nella direzione giusta.
    for method in &methods {
        let Some(code) = &method.code else { continue };
        let literals = ldc_strings(code, &pool)?;
        if !literals.iter().any(|literal| literal == LOAD_METHOD_LITERAL) {
            continue;
        }
        for literal in literals {
            if is_option_key_shape(&literal) {
                plain.insert(literal);
            }
        }
    }

    // `version` non passa per quei metodi: la maneggia il datafixer.
    plain.insert(VERSION_KEY.to_string());

    // I keybind sono i `KeyMapping` costruiti nel costruttore della classe. I
    // nomi delle categorie hanno la stessa forma ma non sono chiavi del file:
    // `key.categories.*` è la forma vecchia, `key.category.*` quella nuova.
    let mut keybinds: BTreeSet<String> = BTreeSet::new();
    for method in &methods {
        if method.name != "<init>" {
            continue;
        }
        let Some(code) = &method.code else { continue };
        for literal in ldc_strings(code, &pool)? {
            if !literal.starts_with("key.")
                || literal.starts_with("key.categories.")
                || literal.starts_with("key.category.")
            {
                continue;
            }
            keybinds.insert(literal);
        }
    }

    Ok((plain, keybinds))
}

fn derive_from_lang(lang_bytes: &[u8]) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let lang: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(lang_bytes).context("failed to parse en_us.json")?;

    let mut sound_categories = BTreeSet::new();
    let mut model_parts = BTreeSet::new();
    for key in lang.keys() {
        if let Some(name) = key.strip_prefix("soundCategory.") {
            if !name.contains('.') {
                sound_categories.insert(name.to_string());
            }
        }
        if let Some(name) = key.strip_prefix("options.modelPart.") {
            if !name.contains('.') {
                model_parts.insert(name.to_string());
            }
        }
    }

    Ok((sound_categories, model_parts))
}

/// Ogni chiave va alla schermata **più specifica** che la nomina, cioè quella
/// che ne nomina di meno: `chatOpacity` è nominata sia da Accessibilità sia da
/// Chat, e Chat ne nomina meno. A parità, ordine alfabetico, così il risultato
/// non dipende dall'ordine delle voci nel jar.
fn assign_groups(
    plain: &BTreeSet<String>,
    screen_classes: &[(String, HashSet<String>)],
) -> BTreeMap<String, String> {
    let sized: Vec<(&str, usize)> = screen_classes
        .iter()
        .map(|(name, literals)| (name.as_str(), literals.iter().filter(|l| plain.contains(*l)).count()))
        .collect();

    let mut groups = BTreeMap::new();
    for key in plain {
        let mut best: Option<(&str, usize)> = None;
        for ((name, size), (_, literals)) in sized.iter().zip(screen_classes.iter()) {
            if !literals.contains(key) {
                continue;
            }
            let candidate = (*name, *size);
            best = match best {
                None => Some(candidate),
                Some((best_name, best_size)) => {
                    if candidate.1 < best_size || (candidate.1 == best_size && candidate.0 < best_name)
                    {
                        Some(candidate)
                    } else {
                        Some((best_name, best_size))
                    }
                }
            };
        }
        if let Some((name, _)) = best {
            groups.insert(key.clone(), name.to_string());
        }
    }

    groups
}

/// La forma di una chiave di `options.txt`. Serve a buttare via i letterali che
/// non sono chiavi ma condividono il metodo — i formati di log come
/// `Invalid keyMapping {} = {}, unbinding`. Non può essere più stretta di così:
/// `ao` è lunga due caratteri e `discrete_mouse_scroll` ha gli underscore.
fn is_option_key_shape(literal: &str) -> bool {
    let mut chars = literal.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '.')
}

// ---------------------------------------------------------------------------
// Cache accanto al jar
// ---------------------------------------------------------------------------

/// Deriva una volta e poi rilegge: la derivazione costa un decimo di secondo e
/// il jar di una versione non cambia mai.
pub fn load_or_derive(
    launcher_paths: &LauncherPaths,
    version_id: &str,
) -> Result<VanillaOptionKeys> {
    validate_path_component(version_id)?;
    let version_dir = launcher_paths.mc_version_dir(version_id);
    let cache_path = version_dir.join(CACHE_FILENAME);

    if let Ok(text) = std::fs::read_to_string(&cache_path) {
        if let Ok(cached) = serde_json::from_str::<VanillaOptionKeys>(&text) {
            if cached.format_version == DERIVATION_FORMAT_VERSION && cached.verify().is_ok() {
                return Ok(cached);
            }
        }
    }

    let derived = derive_from_client_jar(&version_dir.join(CLIENT_JAR_FILENAME))?;
    if let Ok(serialized) = serde_json::to_string_pretty(&derived) {
        let _ = std::fs::write(&cache_path, serialized);
    }
    Ok(derived)
}

/// Da `version:3465` alla versione che l'ha scritta.
///
/// Serve perché il filtro guarda la **sorgente** (D71): per sapere quali nomi
/// erano vanilla quando quei valori sono stati scritti bisogna risalire al jar
/// di quella versione. La riga `version:` porta una `DataVersion`, non un id, e
/// la corrispondenza sta dentro i jar stessi (`version.json`, `world_version`).
pub fn find_version_for_data_version(
    launcher_paths: &LauncherPaths,
    data_version: i64,
) -> Result<Option<String>> {
    let cache_dir = launcher_paths.mc_cache_dir();
    let Ok(entries) = std::fs::read_dir(&cache_dir) else {
        return Ok(None);
    };

    for entry in entries.flatten() {
        let jar_path = entry.path().join(CLIENT_JAR_FILENAME);
        if !jar_path.is_file() {
            continue;
        }
        let Ok(found) = read_jar_data_version(&jar_path) else {
            continue;
        };
        if found != data_version {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        return Ok(Some(name));
    }

    Ok(None)
}

/// I gruppi presi in prestito da un'altra versione, quando quella bersaglio
/// non ne ha.
///
/// Sui jar offuscati — 1.20.1 e tutti quelli fino a 1.21.1 — i nomi delle
/// classi delle schermate non sopravvivono, quindi [`derive_from_client_jar`]
/// restituisce `groups` vuota e ogni impostazione semplice finisce in un
/// secchio solo. Le versioni condividono però quasi tutti i nomi (81 chiavi su
/// 86 fra 1.20.1 e 26.3, misurate), quindi la mappa di un jar con i nomi veri
/// copre quasi tutta la lista anche di una versione vecchia.
///
/// **Il confine, che non va attraversato**: questa mappa decide solo *in che
/// gruppo* finisce una chiave, **mai se quella chiave è vanilla**.
/// L'appartenenza al vanilla resta quella derivata dal jar della versione
/// bersaglio (D68), e per renderlo vero e non solo dichiarato la mappa viene
/// **ristretta alle chiavi semplici del bersaglio** prima di uscire da qui:
/// una chiave che esiste solo nella versione prestatrice non può passare di
/// qua e finire in un `options.txt` che non la conosce.
///
/// Fra più candidate si prende quella con la `DataVersion` più alta: è quella
/// che conosce più chiavi, e i nomi nuovi non fanno danno perché il filtro
/// sopra li toglie. Se non c'è nessuna candidata resta il secchio unico.
pub fn borrowed_groups(
    launcher_paths: &LauncherPaths,
    target: &VanillaOptionKeys,
) -> Option<(String, BTreeMap<String, String>)> {
    let entries = std::fs::read_dir(launcher_paths.mc_cache_dir()).ok()?;

    let mut best: Option<VanillaOptionKeys> = None;
    for entry in entries.flatten() {
        if !entry.path().join(CLIENT_JAR_FILENAME).is_file() {
            continue;
        }
        let Some(version_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if version_id == target.version_id {
            continue;
        }
        let Ok(candidate) = load_or_derive(launcher_paths, &version_id) else {
            continue;
        };
        if candidate.groups.is_empty() {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|current| candidate.data_version > current.data_version)
        {
            best = Some(candidate);
        }
    }

    let lender = best?;
    let groups: BTreeMap<String, String> = lender
        .groups
        .into_iter()
        .filter(|(key, _)| target.plain.contains(key))
        .collect();
    if groups.is_empty() {
        return None;
    }

    Some((lender.version_id, groups))
}

// ---------------------------------------------------------------------------
// Class file, il minimo indispensabile
// ---------------------------------------------------------------------------

enum CpEntry {
    Utf8(String),
    StringRef(u16),
    Other,
}

struct ClassMethod {
    name: String,
    descriptor: String,
    code: Option<Vec<u8>>,
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow!("class file troncato a {offset}"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("class file troncato a {offset}"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(u32_at(bytes, offset)? as i32)
}

fn parse_constant_pool(bytes: &[u8]) -> Result<(Vec<CpEntry>, usize)> {
    if bytes.len() < 10 || bytes[0..4] != [0xca, 0xfe, 0xba, 0xbe] {
        bail!("non è un class file");
    }

    let count = u16_at(bytes, 8)? as usize;
    let mut pool: Vec<CpEntry> = Vec::with_capacity(count.max(1));
    pool.push(CpEntry::Other); // gli indici del constant pool partono da 1

    let mut offset = 10usize;
    let mut index = 1usize;
    while index < count {
        let tag = *bytes
            .get(offset)
            .ok_or_else(|| anyhow!("constant pool troncato"))?;
        offset += 1;

        match tag {
            1 => {
                let length = u16_at(bytes, offset)? as usize;
                offset += 2;
                let raw = bytes
                    .get(offset..offset + length)
                    .ok_or_else(|| anyhow!("utf8 troncata nel constant pool"))?;
                pool.push(CpEntry::Utf8(String::from_utf8_lossy(raw).into_owned()));
                offset += length;
            }
            8 => {
                pool.push(CpEntry::StringRef(u16_at(bytes, offset)?));
                offset += 2;
            }
            7 | 16 | 19 | 20 => {
                pool.push(CpEntry::Other);
                offset += 2;
            }
            15 => {
                pool.push(CpEntry::Other);
                offset += 3;
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                pool.push(CpEntry::Other);
                offset += 4;
            }
            5 | 6 => {
                // long e double occupano due posizioni
                pool.push(CpEntry::Other);
                pool.push(CpEntry::Other);
                offset += 8;
                index += 1;
            }
            other => bail!("tag {other} sconosciuto nel constant pool"),
        }

        index += 1;
    }

    Ok((pool, offset))
}

fn utf8_at(pool: &[CpEntry], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(CpEntry::Utf8(text)) => Some(text.as_str()),
        _ => None,
    }
}

fn string_literal_at(pool: &[CpEntry], index: u16) -> Option<&str> {
    match pool.get(index as usize) {
        Some(CpEntry::StringRef(target)) => utf8_at(pool, *target),
        _ => None,
    }
}

fn skip_attributes(bytes: &[u8], mut offset: usize) -> Result<usize> {
    let count = u16_at(bytes, offset)? as usize;
    offset += 2;
    for _ in 0..count {
        let length = u32_at(bytes, offset + 2)? as usize;
        offset += 6 + length;
    }
    if offset > bytes.len() {
        bail!("attributi oltre la fine del class file");
    }
    Ok(offset)
}

fn parse_methods(bytes: &[u8], pool: &[CpEntry], mut offset: usize) -> Result<Vec<ClassMethod>> {
    offset += 6; // access_flags, this_class, super_class
    let interfaces = u16_at(bytes, offset)? as usize;
    offset += 2 + 2 * interfaces;

    // campi
    let field_count = u16_at(bytes, offset)? as usize;
    offset += 2;
    for _ in 0..field_count {
        offset += 6;
        offset = skip_attributes(bytes, offset)?;
    }

    let method_count = u16_at(bytes, offset)? as usize;
    offset += 2;

    let mut methods = Vec::with_capacity(method_count);
    for _ in 0..method_count {
        let name = utf8_at(pool, u16_at(bytes, offset + 2)?)
            .unwrap_or_default()
            .to_string();
        let descriptor = utf8_at(pool, u16_at(bytes, offset + 4)?)
            .unwrap_or_default()
            .to_string();
        offset += 6;

        let attribute_count = u16_at(bytes, offset)? as usize;
        offset += 2;
        let mut code: Option<Vec<u8>> = None;
        for _ in 0..attribute_count {
            let attribute_name = utf8_at(pool, u16_at(bytes, offset)?).unwrap_or_default();
            let length = u32_at(bytes, offset + 2)? as usize;
            let body_start = offset + 6;
            let body = bytes
                .get(body_start..body_start + length)
                .ok_or_else(|| anyhow!("attributo troncato"))?;

            if attribute_name == "Code" && code.is_none() {
                let code_length = u32_at(body, 4)? as usize;
                let bytecode = body
                    .get(8..8 + code_length)
                    .ok_or_else(|| anyhow!("bytecode troncato"))?;
                code = Some(bytecode.to_vec());
            }

            offset = body_start + length;
        }

        methods.push(ClassMethod {
            name,
            descriptor,
            code,
        });
    }

    Ok(methods)
}

fn ldc_strings(code: &[u8], pool: &[CpEntry]) -> Result<Vec<String>> {
    let mut literals = Vec::new();
    let mut pc = 0usize;

    while pc < code.len() {
        match code[pc] {
            0x12 => {
                let index = *code
                    .get(pc + 1)
                    .ok_or_else(|| anyhow!("ldc troncata"))? as u16;
                if let Some(text) = string_literal_at(pool, index) {
                    literals.push(text.to_string());
                }
            }
            0x13 => {
                if let Some(text) = string_literal_at(pool, u16_at(code, pc + 1)?) {
                    literals.push(text.to_string());
                }
            }
            _ => {}
        }
        pc += instruction_length(code, pc)?;
    }

    Ok(literals)
}

fn instruction_length(code: &[u8], pc: usize) -> Result<usize> {
    let opcode = *code
        .get(pc)
        .ok_or_else(|| anyhow!("bytecode oltre la fine"))?;

    let length = match opcode {
        // tableswitch: padding a multiplo di 4, poi default/low/high e i salti
        0xaa => {
            let padded = (pc + 4) & !3usize;
            let low = i32_at(code, padded + 4)?;
            let high = i32_at(code, padded + 8)?;
            if high < low {
                bail!("tableswitch con estremi invertiti");
            }
            let entries = (high as i64 - low as i64 + 1) as usize;
            padded + 12 + 4 * entries - pc
        }
        // lookupswitch: padding, default, numero di coppie, poi le coppie
        0xab => {
            let padded = (pc + 4) & !3usize;
            let pairs = i32_at(code, padded + 4)?;
            if pairs < 0 {
                bail!("lookupswitch con numero di coppie negativo");
            }
            padded + 8 + 8 * (pairs as usize) - pc
        }
        // wide: 6 byte se modifica iinc, 4 altrimenti
        0xc4 => {
            let widened = *code
                .get(pc + 1)
                .ok_or_else(|| anyhow!("wide troncata"))?;
            if widened == 0x84 {
                6
            } else {
                4
            }
        }
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1
        | 0xc6 | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        _ => 1,
    };

    if length == 0 {
        bail!("istruzione di lunghezza zero a {pc}");
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_keeps_the_awkward_real_keys() {
        assert!(is_option_key_shape("ao"));
        assert!(is_option_key_shape("discrete_mouse_scroll"));
        assert!(is_option_key_shape("graphicsMode"));
        assert!(is_option_key_shape("key.hotbar.1"));
    }

    #[test]
    fn key_shape_rejects_log_formats() {
        assert!(!is_option_key_shape("Invalid keyMapping {} = {}, unbinding"));
        assert!(!is_option_key_shape("Failed to load options"));
        assert!(!is_option_key_shape(""));
        assert!(!is_option_key_shape("2"));
    }

    /// Un class file valido quanto basta al nostro parser: constant pool con
    /// un solo letterale, nessun metodo. Serve a fabbricare un jar mutilato —
    /// l'ancora c'è, le chiavi no — che è il caso che la verifica a valle deve
    /// fermare.
    fn stub_class(literal: &str) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x34];
        bytes.extend_from_slice(&2u16.to_be_bytes()); // constant_pool_count
        bytes.push(1); // CONSTANT_Utf8
        bytes.extend_from_slice(&(literal.len() as u16).to_be_bytes());
        bytes.extend_from_slice(literal.as_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // access_flags
        bytes.extend_from_slice(&0u16.to_be_bytes()); // this_class
        bytes.extend_from_slice(&0u16.to_be_bytes()); // super_class
        bytes.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        bytes.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        bytes.extend_from_slice(&0u16.to_be_bytes()); // methods_count
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        bytes
    }

    fn write_stub_jar(path: &std::path::Path, class_literal: Option<&str>) {
        let file = std::fs::File::create(path).expect("stub jar");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        archive.start_file("version.json", options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            br#"{"id":"stub","name":"stub","world_version":1}"#,
        )
        .unwrap();

        archive.start_file(LANG_ENTRY, options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            br#"{"soundCategory.master":"Master","options.modelPart.cape":"Cape"}"#,
        )
        .unwrap();

        if let Some(literal) = class_literal {
            archive.start_file("stub/Options.class", options).unwrap();
            std::io::Write::write_all(&mut archive, &stub_class(literal)).unwrap();
        }

        archive.finish().unwrap();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("cubic-options-{tag}-{stamp}"));
        std::fs::create_dir_all(&directory).expect("temp dir");
        directory
    }

    #[test]
    fn derivation_fails_loudly_on_a_mutilated_jar() {
        let directory = temp_dir("mutilated");
        let jar = directory.join("client.jar");
        write_stub_jar(&jar, Some(ANCHOR_LITERAL));

        let error = derive_from_client_jar(&jar).expect_err("a keyless jar must not derive");

        assert!(
            error.to_string().contains("derivazione sospetta"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Un constant pool costruito a mano. Serve per il percorso **positivo**:
    /// i jar veri non ci sono su tutte le macchine, e l'ancora più il
    /// camminatore di `ldc` sono la parte fragile — se restassero coperti solo
    /// dai jar in cache, altrove non li proverebbe niente.
    struct Pool {
        bytes: Vec<u8>,
        next: u16,
    }

    impl Pool {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                next: 1,
            }
        }

        fn utf8(&mut self, text: &str) -> u16 {
            self.bytes.push(1);
            self.bytes
                .extend_from_slice(&(text.len() as u16).to_be_bytes());
            self.bytes.extend_from_slice(text.as_bytes());
            let index = self.next;
            self.next += 1;
            index
        }

        fn string(&mut self, text: &str) -> u16 {
            let utf8 = self.utf8(text);
            self.bytes.push(8);
            self.bytes.extend_from_slice(&utf8.to_be_bytes());
            let index = self.next;
            self.next += 1;
            index
        }
    }

    /// `ldc_w` + `pop` per ogni letterale, poi `return`. Non deve essere
    /// bytecode eseguibile: deve essere bytecode **percorribile**.
    fn code_body(indices: &[u16]) -> Vec<u8> {
        let mut code = Vec::new();
        for index in indices {
            code.push(0x13);
            code.extend_from_slice(&index.to_be_bytes());
            code.push(0x57);
        }
        code.push(0xb1);

        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes()); // max_stack
        body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
        body.extend_from_slice(&(code.len() as u32).to_be_bytes());
        body.extend_from_slice(&code);
        body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
        body.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        body
    }

    fn well_formed_options_class(plain: &[String], keybinds: &[String]) -> Vec<u8> {
        let mut pool = Pool::new();
        pool.utf8(ANCHOR_LITERAL);
        let code_name = pool.utf8("Code");
        let process_name = pool.utf8("processOptions");
        let process_descriptor = pool.utf8("(Lstub/Options$FieldAccess;)V");
        let load_name = pool.utf8("load");
        let init_name = pool.utf8("<init>");
        let void_descriptor = pool.utf8("()V");

        let mut process_literals: Vec<u16> = plain.iter().map(|key| pool.string(key)).collect();
        // un formato di log fra i letterali: la forma deve scartarlo
        process_literals.push(pool.string("Invalid keyMapping {} = {}, unbinding"));

        let load_literals = vec![
            pool.string("fullscreenResolution"),
            pool.string(LOAD_METHOD_LITERAL),
        ];

        let mut init_literals: Vec<u16> = keybinds.iter().map(|key| pool.string(key)).collect();
        // una categoria: ha la stessa forma ma non è una chiave del file
        init_literals.push(pool.string("key.categories.misc"));

        let methods = [
            (process_name, process_descriptor, process_literals),
            (load_name, void_descriptor, load_literals),
            (init_name, void_descriptor, init_literals),
        ];

        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x34];
        bytes.extend_from_slice(&pool.next.to_be_bytes());
        bytes.extend_from_slice(&pool.bytes);
        bytes.extend_from_slice(&0u16.to_be_bytes()); // access_flags
        bytes.extend_from_slice(&0u16.to_be_bytes()); // this_class
        bytes.extend_from_slice(&0u16.to_be_bytes()); // super_class
        bytes.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        bytes.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        bytes.extend_from_slice(&(methods.len() as u16).to_be_bytes());
        for (name, descriptor, literals) in &methods {
            let body = code_body(literals);
            bytes.extend_from_slice(&0u16.to_be_bytes()); // access_flags
            bytes.extend_from_slice(&name.to_be_bytes());
            bytes.extend_from_slice(&descriptor.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
            bytes.extend_from_slice(&code_name.to_be_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&body);
        }
        bytes.extend_from_slice(&0u16.to_be_bytes()); // class attributes_count
        bytes
    }

    #[test]
    fn derives_plain_keybinds_and_lang_from_a_synthetic_jar() {
        let mut plain: Vec<String> = HISTORIC_CORE.iter().map(|key| key.to_string()).collect();
        plain.extend((0..MIN_PLAIN_KEYS).map(|index| format!("setting{index}")));
        let keybinds: Vec<String> = (0..MIN_KEYBINDS + 1)
            .map(|index| format!("key.binding{index}"))
            .collect();

        let directory = temp_dir("synthetic");
        let jar = directory.join("client.jar");
        let file = std::fs::File::create(&jar).expect("stub jar");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        archive.start_file("version.json", options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            br#"{"id":"stub","name":"stub","world_version":4242}"#,
        )
        .unwrap();
        archive.start_file(LANG_ENTRY, options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            br#"{"soundCategory.master":"Master","soundCategory.music":"Music","options.modelPart.cape":"Cape"}"#,
        )
        .unwrap();
        archive.start_file("stub/Options.class", options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            &well_formed_options_class(&plain, &keybinds),
        )
        .unwrap();
        archive.finish().unwrap();

        let derived = derive_from_client_jar(&jar).expect("a well-formed jar must derive");

        assert_eq!(derived.version_id, "stub");
        assert_eq!(derived.data_version, 4242);
        assert_eq!(derived.options_class, "stub/Options.class");
        // le chiavi del metodo scrittore, più `fullscreenResolution` da load()
        // e `version`, che non passa da nessuno dei due
        assert_eq!(derived.plain.len(), plain.len() + 2);
        assert!(derived.plain.contains("fullscreenResolution"));
        assert!(derived.plain.contains(VERSION_KEY));
        assert!(!derived
            .plain
            .iter()
            .any(|key| key.contains(' ')), "il formato di log non è una chiave");
        assert_eq!(derived.keybinds.len(), keybinds.len());
        assert!(!derived.keybinds.contains("key.categories.misc"));
        assert_eq!(derived.sound_categories.len(), 2);
        assert_eq!(derived.model_parts.len(), 1);

        assert!(derived.accepts("key_key.binding0"));
        assert!(derived.accepts("soundCategory_music"));
        assert!(derived.accepts("modelPart_cape"));
        assert!(!derived.accepts("key_key.epicfight.dodge"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn derivation_fails_when_the_anchor_is_missing() {
        let directory = temp_dir("anchorless");
        let jar = directory.join("client.jar");
        write_stub_jar(&jar, None);

        let error = derive_from_client_jar(&jar).expect_err("no anchor, no derivation");

        assert!(
            error.to_string().contains(ANCHOR_LITERAL),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn derivation_refuses_an_ambiguous_anchor() {
        let directory = temp_dir("ambiguous");
        let jar = directory.join("client.jar");
        let file = std::fs::File::create(&jar).expect("stub jar");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        archive.start_file("version.json", options).unwrap();
        std::io::Write::write_all(
            &mut archive,
            br#"{"id":"stub","name":"stub","world_version":1}"#,
        )
        .unwrap();
        for name in ["a/One.class", "a/Two.class"] {
            archive.start_file(name, options).unwrap();
            std::io::Write::write_all(&mut archive, &stub_class(ANCHOR_LITERAL)).unwrap();
        }
        archive.finish().unwrap();

        let error = derive_from_client_jar(&jar).expect_err("two anchors is ambiguous");

        assert!(
            error.to_string().contains("invece sta in 2"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// I jar veri, quando ci sono. Sono i due che la ricognizione ha misurato a
    /// mano: gli insiemi devono coincidere esattamente, non approssimarsi.
    fn cached_client_jar(version: &str) -> Option<std::path::PathBuf> {
        let jar = dirs_next_home()?
            .join(".local/share/com.cubic.launcher/cache/minecraft")
            .join(version)
            .join(CLIENT_JAR_FILENAME);
        jar.is_file().then_some(jar)
    }

    fn dirs_next_home() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }

    /// Gli insiemi misurati a mano nella ricognizione erano quelli **scritti
    /// nei file** dal gioco: 86 chiavi semplici su 1.20.1 e 114 su 26.3. Il
    /// derivato è più grande di poco perché include anche quello che il gioco
    /// legge e non scrive: `fullscreenResolution` su entrambe, e l'alias
    /// legacy `fancyGraphics` su 1.20.1, dove la conversione non è ancora un
    /// datafixer. Da qui 88 e 115.
    #[test]
    fn derives_the_measured_sets_from_the_real_jars() {
        let expectations = [
            ("1.20.1", 3465i64, 88usize, 34usize, 10usize, 7usize),
            ("26.3", 5023, 115, 62, 11, 7),
        ];

        for (version, data_version, plain, keybinds, sounds, model_parts) in expectations {
            let Some(jar) = cached_client_jar(version) else {
                eprintln!("skipping {version}: client.jar not in the local cache");
                continue;
            };

            let derived = derive_from_client_jar(&jar).expect("real jar must derive");

            assert_eq!(derived.version_id, version);
            assert_eq!(derived.data_version, data_version);
            assert_eq!(derived.plain.len(), plain, "{version} plain keys");
            assert_eq!(derived.keybinds.len(), keybinds, "{version} keybinds");
            assert_eq!(derived.sound_categories.len(), sounds, "{version} sounds");
            assert_eq!(derived.model_parts.len(), model_parts, "{version} model parts");
            assert!(derived.plain.contains("fullscreenResolution"));

            assert!(derived.accepts("fov"));
            assert!(derived.accepts("key_key.hotbar.1"));
            assert!(!derived.accepts("key_key.epicfight.dodge"));
            assert!(!derived.accepts("key_iris.keybind.reload"));
        }
    }

    fn sample_keys() -> VanillaOptionKeys {
        VanillaOptionKeys {
            format_version: DERIVATION_FORMAT_VERSION,
            version_id: "1.20.1".into(),
            data_version: 3465,
            options_class: "enr.class".into(),
            plain: ["fov", "renderDistance", "guiScale", "mouseSensitivity", "version"]
                .iter()
                .map(|key| key.to_string())
                .collect(),
            keybinds: ["key.attack", "key.hotbar.1"]
                .iter()
                .map(|key| key.to_string())
                .collect(),
            sound_categories: ["master"].iter().map(|key| key.to_string()).collect(),
            model_parts: ["cape"].iter().map(|key| key.to_string()).collect(),
            groups: BTreeMap::new(),
        }
    }

    #[test]
    fn accepts_strips_the_file_prefixes() {
        let keys = sample_keys();

        assert!(keys.accepts("fov"));
        assert!(keys.accepts("key_key.attack"));
        assert!(keys.accepts("soundCategory_master"));
        assert!(keys.accepts("modelPart_cape"));

        assert!(!keys.accepts("key_key.epicfight.dodge"));
        assert!(!keys.accepts("key_iris.keybind.reload"));
        assert!(!keys.accepts("soundCategory_ui"));
        assert!(!keys.accepts("shaderPackName"));
    }

    #[test]
    fn verify_rejects_a_set_that_is_too_small() {
        let error = sample_keys().verify().expect_err("5 plain keys must not pass");

        assert!(
            error.to_string().contains("impostazioni semplici"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn verify_rejects_a_set_without_the_historic_core() {
        let mut keys = sample_keys();
        keys.plain = (0..MIN_PLAIN_KEYS + 1)
            .map(|index| format!("filler{index}"))
            .collect();
        keys.keybinds = (0..MIN_KEYBINDS + 1)
            .map(|index| format!("key.filler{index}"))
            .collect();

        let error = keys.verify().expect_err("missing core must not pass");

        assert!(
            error.to_string().contains("nucleo storico"),
            "unexpected error: {error}"
        );
    }
}
