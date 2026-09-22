//! I tre file che il gioco tiene nella radice di un'istanza e che E2 condivide
//! fra le istanze di una modlist (D75, D76, D77, D78).
//!
//! I file sono `servers.dat` (la lista multigiocatore), `hotbar.nbt` (le hotbar
//! salvate in creativa) e `command_history.txt` (gli ultimi 50 comandi
//! digitati). Non hanno niente in comune nel formato; hanno in comune il fatto
//! che stanno nella radice dell'istanza e che l'utente si aspetta di
//! ritrovarli uguali passando da un'istanza all'altra della stessa modlist.
//!
//! **Copia, non link, per tutti e tre (D78).** Che `servers.dat` non si possa
//! condividere con un link non è un'opinione: il gioco lo salva scrivendo un
//! temporaneo nella radice dell'istanza e poi chiamando `Util.safeReplaceFile`,
//! che è una coppia di `Files.move` — il vecchio finisce su `servers.dat_old`,
//! il nuovo prende il suo posto (D76, misurato nel bytecode di 1.20.1 e 26.3 e
//! nelle tracce su disco, report `052`). Un symlink viene sostituito da un file
//! vero al primo ping, un hardlink viene reciso. `hotbar.nbt` e
//! `command_history.txt` il gioco li scrive invece **in place**, quindi lì un
//! link sopravviverebbe — ma il meccanismo a copia deve esistere comunque per
//! il primo file, e due meccanismi costano il doppio a mantenerli e regalano un
//! vantaggio (la condivisione viva fra istanze aperte insieme) che D78 mette
//! fuori perimetro.
//!
//! **Il limite di D78, scritto una volta qui e ripetuto dove fa danno**
//! (`copy_back_from_instance`): due istanze avviate una dopo l'altra non si
//! fondono. L'ultima che esce riscrive la copia canonica con quello che ha in
//! mano, e le modifiche dell'altra spariscono. Non è una svista: la
//! riconciliazione per voce — leggere l'NBT, riconoscere le voci una per una,
//! fondere — è esattamente il pezzo che D78 rimanda.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;

// ---------------------------------------------------------------------------
// I tre file
// ---------------------------------------------------------------------------

pub const SERVERS_FILENAME: &str = "servers.dat";
pub const HOTBAR_FILENAME: &str = "hotbar.nbt";
pub const COMMAND_HISTORY_FILENAME: &str = "command_history.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SharedFile {
    Servers,
    Hotbar,
    CommandHistory,
}

impl SharedFile {
    pub const ALL: [SharedFile; 3] = [
        SharedFile::Servers,
        SharedFile::Hotbar,
        SharedFile::CommandHistory,
    ];

    pub fn filename(self) -> &'static str {
        match self {
            SharedFile::Servers => SERVERS_FILENAME,
            SharedFile::Hotbar => HOTBAR_FILENAME,
            SharedFile::CommandHistory => COMMAND_HISTORY_FILENAME,
        }
    }

    /// Il nome con cui l'interruttore di questo file sta nel database e nella
    /// GUI. Non è il nome del file: quello può cambiare fra versioni del gioco,
    /// una chiave salvata no.
    pub fn key(self) -> &'static str {
        match self {
            SharedFile::Servers => "servers",
            SharedFile::Hotbar => "hotbar",
            SharedFile::CommandHistory => "commandHistory",
        }
    }
}

/// Vero per i file che **non devono uscire** da qui dentro in un archivio di
/// modlist (D77).
///
/// Oggi ce n'è uno solo, `command_history.txt`, e il motivo è concreto e non
/// teorico: il gioco persiste ogni riga di chat che comincia per `/`, testo
/// esatto e senza nessuna redazione (`CommandHistory.addCommand` →
/// `Files.newBufferedWriter`, misurato nel jar). Sui server con AuthMe e simili
/// si entra con `/login <password>`: quella riga finisce nel file **in chiaro**,
/// e un archivio di modlist è una cosa che si manda a qualcun altro.
///
/// **Non esiste un'opzione per riattivarlo, ed è voluto.** Un interruttore
/// "includi anche la cronologia comandi" sarebbe acceso da chi non sa cosa c'è
/// dentro. Se un domani serve davvero, la strada è chiedere all'utente di
/// esportarlo a mano, non rimettere una spunta qui.
///
/// Dalla fase 2 ce n'è un secondo: il `.bak` di D80. Non contiene password —
/// sono gli stessi dati degli altri tre — ma è una **fotografia di uno stato
/// che il destinatario dell'archivio non ha mai avuto**, e nessuno la
/// guarderebbe mai dentro un pacchetto scaricato da qualcun altro. Spedirlo
/// raddoppierebbe i file d'istanza dell'archivio per niente.
pub fn is_excluded_from_export(file_name: &str) -> bool {
    if file_name == COMMAND_HISTORY_FILENAME {
        return true;
    }

    file_name
        .strip_suffix(BACKUP_SUFFIX)
        .is_some_and(|stem| SharedFile::ALL.iter().any(|file| file.filename() == stem))
}

// ---------------------------------------------------------------------------
// Dove vive la copia canonica
// ---------------------------------------------------------------------------

/// `<root>/mod-lists/<nome>/servers.dat` e compagni, accanto all'`options.txt`
/// di modlist di E1: stessa cartella, stessa logica — file che l'utente può
/// aprire, non roba interna sotto `.cubic/`.
pub fn canonical_path(
    launcher_paths: &LauncherPaths,
    modlist_name: &str,
    file: SharedFile,
) -> Result<PathBuf> {
    validate_path_component(modlist_name)?;
    Ok(launcher_paths
        .modlists_dir()
        .join(modlist_name)
        .join(file.filename()))
}

pub fn instance_path(instance_root: &Path, file: SharedFile) -> PathBuf {
    instance_root.join(file.filename())
}

// ---------------------------------------------------------------------------
// Il collegamento fra modlist (D75)
// ---------------------------------------------------------------------------

/// La riga di `global_settings` che tiene i collegamenti. Stessa scelta della
/// lista dei mondi nascosti (D65, `worlds::HIDDEN_WORLDS_KEY`): la tabella è
/// già chiave/valore con più di uno scrittore, e questa lista non merita uno
/// schema suo né una migrazione.
pub const SHARED_FILE_GROUPS_KEY: &str = "shared_file_groups";

/// Un gruppo di modlist che vedono gli stessi tre file.
///
/// **Il collegamento è simmetrico e transitivo: un gruppo è un insieme, non una
/// catena.** Se Drehmal si collega a test2, test2 vede i server di Drehmal
/// tanto quanto Drehmal vede i suoi: è la stessa lista, non due liste che si
/// copiano. E collegando una terza modlist a una qualsiasi delle due, le tre
/// finiscono nello stesso insieme. La ragione non è eleganza: le catene e i
/// collegamenti a senso unico hanno bisogno di un ordine di risoluzione, e un
/// ordine di risoluzione è la cosa che diventa complicata in silenzio — "A vede
/// B, B vede C, cosa vede A?" non ha una risposta che l'utente possa
/// indovinare guardando la schermata.
///
/// `members[0]` è la modlist la cui cartella **contiene davvero i file**: le
/// altre passano da lì. Tenere un solo posto per i byte è quello che rende
/// l'insieme un insieme; se ogni membro avesse la sua copia servirebbe decidere
/// quale vince, che è di nuovo un ordine di risoluzione.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedGroup {
    pub members: Vec<String>,
}

impl SharedGroup {
    pub fn canonical(&self) -> Option<&str> {
        self.members.first().map(String::as_str)
    }

    pub fn contains(&self, modlist_name: &str) -> bool {
        self.members.iter().any(|name| name == modlist_name)
    }
}

/// Una riga assente, o illeggibile, è una lista vuota: nessuna modlist è
/// collegata a nessun'altra, che è esattamente il comportamento predefinito.
/// Un collegamento perso è fastidioso; un lancio che non parte perché una riga
/// del database non si deserializza è peggio.
pub fn load_groups(connection: &Connection) -> Result<Vec<SharedGroup>> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = ?1",
            [SHARED_FILE_GROUPS_KEY],
            |row| row.get(0),
        )
        .ok();

    let Some(stored) = stored else {
        return Ok(Vec::new());
    };

    Ok(serde_json::from_str::<Vec<SharedGroup>>(&stored)
        .unwrap_or_default()
        .into_iter()
        .filter(|group| group.members.len() > 1)
        .collect())
}

fn write_groups(connection: &Connection, groups: &[SharedGroup]) -> Result<()> {
    let value = serde_json::to_string(groups).context("failed to serialize the shared groups")?;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SHARED_FILE_GROUPS_KEY, value.as_str()],
        )
        .context("failed to write the shared groups")?;

    Ok(())
}

/// La modlist nella cui cartella stanno i file che questa modlist deve vedere.
/// Senza collegamenti è la modlist stessa, che è il caso predefinito di D75:
/// condiviso fra le istanze della stessa modlist e basta.
pub fn canonical_modlist(connection: &Connection, modlist_name: &str) -> Result<String> {
    for group in load_groups(connection)? {
        if group.contains(modlist_name) {
            if let Some(canonical) = group.canonical() {
                return Ok(canonical.to_string());
            }
        }
    }

    Ok(modlist_name.to_string())
}

/// `modlist_name` entra nel gruppo di `other`, e da quel momento le due
/// condividono i tre file.
///
/// **Chi adotta la lista di chi**: il gruppo di arrivo tiene la sua copia
/// canonica, quindi `modlist_name` adotta i file di `other`. Detto dal lato
/// dell'utente: apro le impostazioni di Drehmal, scelgo test2, e Drehmal vede i
/// server di test2. L'alternativa — fondere le due liste — è di nuovo la
/// riconciliazione per voce che D78 rimanda, e sceglierne una a caso sarebbe
/// peggio che dichiarare quale.
pub fn link_modlists(
    connection: &Connection,
    modlist_name: &str,
    other: &str,
) -> Result<Vec<SharedGroup>> {
    validate_path_component(modlist_name)?;
    validate_path_component(other)?;
    if modlist_name == other {
        bail!("a mod list cannot be linked to itself");
    }

    let groups = load_groups(connection)?;

    let mut members_of_target: Vec<String> = groups
        .iter()
        .find(|group| group.contains(other))
        .map(|group| group.members.clone())
        .unwrap_or_else(|| vec![other.to_string()]);

    let joining: Vec<String> = groups
        .iter()
        .find(|group| group.contains(modlist_name))
        .map(|group| group.members.clone())
        .unwrap_or_else(|| vec![modlist_name.to_string()]);

    if members_of_target.iter().any(|name| name == modlist_name) {
        // Già nello stesso insieme: collegarle di nuovo non deve riordinare
        // niente, perché riordinare vuol dire cambiare chi tiene i byte.
        return Ok(groups);
    }

    for name in joining {
        if !members_of_target.contains(&name) {
            members_of_target.push(name);
        }
    }

    let mut updated: Vec<SharedGroup> = groups
        .into_iter()
        .filter(|group| !group.contains(modlist_name) && !group.contains(other))
        .collect();
    updated.push(SharedGroup {
        members: members_of_target,
    });

    write_groups(connection, &updated)?;

    Ok(updated)
}

/// `modlist_name` esce dal suo gruppo e torna a vedere solo le proprie istanze.
///
/// Uscire non deve far sparire una lista a nessuno dei due lati, quindi i byte
/// vengono duplicati prima di separare: chi esce si porta via una copia di
/// quello che stava vedendo, e se era lui a tenerla, il gruppo che resta ne
/// riceve una. È l'unico punto in cui una copia canonica ne genera un'altra.
pub fn unlink_modlist(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
) -> Result<Vec<SharedGroup>> {
    validate_path_component(modlist_name)?;

    let groups = load_groups(connection)?;
    let Some(group) = groups.iter().find(|group| group.contains(modlist_name)) else {
        return Ok(groups);
    };

    let remaining: Vec<String> = group
        .members
        .iter()
        .filter(|name| name.as_str() != modlist_name)
        .cloned()
        .collect();

    // Da chi a chi copiare: se esce il portatore, i byte devono raggiungere il
    // nuovo portatore; altrimenti è chi esce a non avere niente di suo.
    let was_canonical = group.canonical() == Some(modlist_name);
    if let Some(new_canonical) = remaining.first() {
        let (from, to) = if was_canonical {
            (modlist_name, new_canonical.as_str())
        } else {
            (group.canonical().unwrap_or(modlist_name), modlist_name)
        };
        copy_canonical_files(launcher_paths, from, to)?;
    }

    let mut updated: Vec<SharedGroup> = groups
        .into_iter()
        .filter(|group| !group.contains(modlist_name))
        .collect();
    if remaining.len() > 1 {
        updated.push(SharedGroup { members: remaining });
    }

    write_groups(connection, &updated)?;

    Ok(updated)
}

fn copy_canonical_files(launcher_paths: &LauncherPaths, from: &str, to: &str) -> Result<()> {
    for file in SharedFile::ALL {
        let source = canonical_path(launcher_paths, from, file)?;
        if !source.exists() {
            continue;
        }
        let target = canonical_path(launcher_paths, to, file)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::copy(&source, &target).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Quali dei tre file si condividono (per modlist)
// ---------------------------------------------------------------------------

/// La riga di `global_settings` che tiene gli interruttori, uno per file e per
/// modlist.
///
/// **Uno per file e non uno solo per tutti e tre**, perché i tre rischi non
/// sono confrontabili: `servers.dat` è una lista di indirizzi, `hotbar.nbt` può
/// perdere degli item attraversando la soglia 1.20.5 (D79), e
/// `command_history.txt` contiene le righe che l'utente ha digitato, `/login`
/// compresi. Un interruttore unico costringerebbe a prendersi il terzo per
/// avere il primo.
///
/// Gli interruttori sono **per modlist e non per gruppo**: sono una preferenza
/// di chi lancia, e una modlist che esce da un gruppo se li porta dietro.
pub const SHARED_FILE_OPTIONS_KEY: &str = "shared_file_options";

/// Il default è **acceso per tutti e tre**, che è quello che la fase 1 faceva
/// senza chiedere. Spegnerne uno di nascosto adesso sarebbe un cambio di
/// comportamento silenzioso; l'interruttore esiste proprio perché chi non vuole
/// condividere la cronologia possa dirlo.
pub fn shared_file_enabled(
    connection: &Connection,
    modlist_name: &str,
    file: SharedFile,
) -> bool {
    load_file_options(connection)
        .get(modlist_name)
        .and_then(|per_file| per_file.get(file.key()).copied())
        .unwrap_or(true)
}

fn load_file_options(connection: &Connection) -> BTreeMap<String, BTreeMap<String, bool>> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = ?1",
            [SHARED_FILE_OPTIONS_KEY],
            |row| row.get(0),
        )
        .ok();

    stored
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn set_shared_file_enabled(
    connection: &Connection,
    modlist_name: &str,
    file: SharedFile,
    enabled: bool,
) -> Result<()> {
    validate_path_component(modlist_name)?;

    let mut options = load_file_options(connection);
    options
        .entry(modlist_name.to_string())
        .or_default()
        .insert(file.key().to_string(), enabled);

    let value = serde_json::to_string(&options).context("failed to serialize the file options")?;
    connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SHARED_FILE_OPTIONS_KEY, value.as_str()],
        )
        .context("failed to write the file options")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// La soglia delle hotbar (D79)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NbtDataVersion {
    #[serde(rename = "DataVersion")]
    data_version: Option<i32>,
}

/// La `DataVersion` che `HotbarManager.save` scrive in testa al file con
/// `NbtUtils.addCurrentDataVersion`. NBT non compresso, quindi si legge diretto.
fn nbt_data_version(bytes: &[u8]) -> Option<i64> {
    fastnbt::from_bytes::<NbtDataVersion>(bytes)
        .ok()
        .and_then(|header| header.data_version)
        .map(i64::from)
}

/// Perché un confronto di numeri protegge da una hotbar che si svuota, e
/// perché toglierlo la rompe. La catena, misurata nei jar (report `052` e
/// `053`):
///
/// 1. dalla **1.20.5** in poi il gioco scrive gli item con `id`, `count`
///    minuscolo e `components`; prima li scriveva con `id`, **`Count`** con la
///    maiuscola e `tag`;
/// 2. il `DataFixer` non aiuta all'indietro: `HotbarManager.load` chiama
///    `DataFixTypes.HOTBAR.updateToCurrentVersion`, che passa a
///    `DataFixerUpper.update(type, input, version, current)`, e quel metodo
///    comincia con `iload_3; iload 4; if_icmpge → aload_2; areturn` — se il
///    file è più nuovo del gioco **torna l'input identico** (misurato su
///    `datafixerupper:6.0.8` e `10.0.21`, report `044`);
/// 3. quindi un gioco pre-1.20.5 legge quel file con `getByte("Count")`, non
///    trova niente, ottiene **0**, e uno stack con conteggio zero è uno stack
///    vuoto;
/// 4. la hotbar risulta vuota, e **il primo salvataggio riscrive il vuoto**
///    nella copia canonica: gli item non sono nascosti, sono persi per tutte le
///    istanze della modlist.
///
/// Da qui la regola: `hotbar.nbt` non va **mai** da una `DataVersion` più alta a
/// una più bassa. All'insù il `DataFixer` fa il suo mestiere ed è il caso per
/// cui esiste — con un limite che vale la pena dire: gli item **moddati** non
/// attraversano comunque, perché la conversione `tag` → `components` non sa
/// cosa farsene di un `tag` che non è vanilla.
fn hotbar_downgrade_refusal(source: &[u8], target_data_version: Option<i64>) -> Option<String> {
    // Nessuna `DataVersion` nella sorgente vuol dire che quel file non l'ha
    // scritto `HotbarManager`: non c'è niente da proteggere e non è compito di
    // questo guardiano decidere cosa sia.
    let source_version = nbt_data_version(source)?;

    // **Non sapere è un no, non un sì.** Se la destinazione non dichiara una
    // versione — il suo `hotbar.nbt` non esiste ancora e la derivazione della
    // `DataVersion` del jar non è riuscita — il confronto non si può fare, e
    // l'unico esito che non può perdere degli item è rifiutare. Lasciar
    // passare qui sarebbe il difetto peggiore di questa funzione: sembra un
    // caso raro e invece è il caso **normale** di un'istanza al primo avvio.
    let Some(target_version) = target_data_version else {
        return Some(format!(
            "refused: the hotbars were saved by DataVersion {source_version} and there is no \
             way to tell which one the destination reads; copying them could empty them (D79)"
        ));
    };

    if source_version > target_version {
        return Some(format!(
            "refused: the hotbars were saved by DataVersion {source_version} and the \
             destination is {target_version}; an older game reads the newer item format \
             as empty stacks and would save the empty hotbars back (D79)"
        ));
    }

    None
}

// ---------------------------------------------------------------------------
// La copia dentro e la copia fuori
// ---------------------------------------------------------------------------

/// Il suffisso della copia di sicurezza di D80.
pub const BACKUP_SUFFIX: &str = ".bak";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SharedFileAction {
    Copied { bytes: u64 },
    /// Come `Copied`, ma il file che stava lì aveva byte diversi e non era mai
    /// stato condiviso: prima di sovrascriverlo se n'è tenuta una copia (D80).
    CopiedAfterBackup { bytes: u64, backup: String },
    /// La copia canonica non c'è ed è questa istanza a non avere niente da
    /// darle: non c'è niente da fare, e il file dell'istanza resta suo.
    NothingToCopy,
    /// Sorgente e destinazione hanno già gli stessi byte. Vale la pena dirlo
    /// invece di copiare: risparmia una riscrittura e, soprattutto, lascia
    /// l'`mtime` dov'è.
    AlreadyIdentical,
    /// L'utente ha spento la condivisione di questo file per questa modlist.
    Disabled,
    /// Copiare sarebbe stato peggio che non copiare (D79). Non è un errore: il
    /// lancio continua e l'istanza tiene quello che ha.
    Refused { reason: String },
    /// Copiare non è riuscito. Non ferma il lancio: un file condiviso che non
    /// arriva è una lista vuota, un lancio che non parte è un pomeriggio perso.
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedFileStatus {
    pub file: SharedFile,
    pub action: SharedFileAction,
}

/// Una riga sola per il log di lancio, come fa la semina di E1: il meccanismo
/// deve lasciare traccia anche quando non fa niente.
pub fn describe(statuses: &[SharedFileStatus], direction: &str) -> String {
    let parts: Vec<String> = statuses
        .iter()
        .map(|status| {
            let action = match &status.action {
                SharedFileAction::Copied { bytes } => format!("copied {bytes}B"),
                SharedFileAction::CopiedAfterBackup { bytes, backup } => {
                    format!("copied {bytes}B, kept the previous one as {backup}")
                }
                SharedFileAction::NothingToCopy => "nothing to copy".to_string(),
                SharedFileAction::AlreadyIdentical => "already identical".to_string(),
                SharedFileAction::Disabled => "sharing is off".to_string(),
                SharedFileAction::Refused { reason } => reason.clone(),
                SharedFileAction::Failed { reason } => format!("failed: {reason}"),
            };
            format!("{} {}", status.file.filename(), action)
        })
        .collect();

    format!("{direction}: {}", parts.join("; "))
}

/// `keep_backup` distingue i due versi: entrando in un'istanza si può stare per
/// sovrascrivere un file che l'utente ha scritto e che nessuno ha mai
/// condiviso (D80); tornando indietro si sovrascrive una copia canonica, che
/// per definizione è già condivisa.
fn copy_file(source: &Path, target: &Path, keep_backup: bool) -> Result<SharedFileAction> {
    let bytes =
        std::fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;

    let existing = std::fs::read(target).ok();
    if existing.as_deref() == Some(bytes.as_slice()) {
        return Ok(SharedFileAction::AlreadyIdentical);
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // D80. La copia di sicurezza nasce **una volta sola**: al secondo lancio il
    // file che si sta sovrascrivendo è quello che abbiamo scritto noi, e
    // salvarlo di nuovo cancellerebbe l'unica cosa che vale la pena tenere,
    // cioè la lista che l'utente aveva prima che la condivisione esistesse.
    let mut backup_name = None;
    if keep_backup && existing.is_some() {
        let backup = backup_path(target);
        if !backup.exists() {
            std::fs::write(&backup, existing.as_deref().unwrap_or_default())
                .with_context(|| format!("failed to write {}", backup.display()))?;
            backup_name = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
        }
    }

    std::fs::write(target, &bytes)
        .with_context(|| format!("failed to write {}", target.display()))?;

    Ok(match backup_name {
        Some(backup) => SharedFileAction::CopiedAfterBackup {
            bytes: bytes.len() as u64,
            backup,
        },
        None => SharedFileAction::Copied {
            bytes: bytes.len() as u64,
        },
    })
}

pub fn backup_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(BACKUP_SUFFIX);
    target.with_file_name(name)
}

/// Prima dello spawn: la copia canonica entra nell'istanza.
///
/// **Prima dello spawn e non dopo**, e per `servers.dat` il margine è stretto:
/// basta che l'utente apra la schermata Multigiocatore perché la risposta al
/// ping — icona e MOTD — faccia riscrivere il file al gioco (D76). Da quel
/// momento in poi copiarci sopra vorrebbe dire buttare via quello che il gioco
/// ha appena scritto.
///
/// Quando la copia canonica non esiste ancora **la si adotta da questa
/// istanza**, se ne ha una. È il modo in cui la condivisione comincia senza
/// chiedere niente a nessuno: la prima istanza che ha un `servers.dat` lo
/// presta alla modlist, le altre lo ricevono al loro primo lancio.
///
/// `instance_data_version` è la `DataVersion` della versione di Minecraft che
/// sta per partire, e serve **solo** al guardiano delle hotbar (D79): senza,
/// un'istanza che non ha ancora un `hotbar.nbt` non avrebbe niente con cui
/// confrontare la copia canonica.
pub fn copy_into_instance(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
    instance_root: &Path,
    instance_data_version: Option<i64>,
) -> Vec<SharedFileStatus> {
    let canonical_owner = match canonical_modlist(connection, modlist_name) {
        Ok(owner) => owner,
        Err(error) => {
            return failed_for_all(&error.to_string());
        }
    };

    SharedFile::ALL
        .into_iter()
        .map(|file| {
            let action = (|| -> Result<SharedFileAction> {
                if !shared_file_enabled(connection, modlist_name, file) {
                    return Ok(SharedFileAction::Disabled);
                }

                let canonical = canonical_path(launcher_paths, &canonical_owner, file)?;
                let local = instance_path(instance_root, file);

                if !canonical.exists() {
                    if local.exists() {
                        // Adottare non sovrascrive niente: la copia canonica non
                        // c'è, quindi non c'è nessun file dell'utente da salvare.
                        return copy_file(&local, &canonical, false);
                    }
                    return Ok(SharedFileAction::NothingToCopy);
                }

                if file == SharedFile::Hotbar {
                    // La destinazione è l'istanza: la sua `DataVersion` è quella
                    // del suo `hotbar.nbt` se ce l'ha, altrimenti quella della
                    // versione che sta per partire.
                    let target_version = match std::fs::read(&local) {
                        Ok(bytes) => nbt_data_version(&bytes).or(instance_data_version),
                        Err(_) => instance_data_version,
                    };
                    let source = std::fs::read(&canonical)
                        .with_context(|| format!("failed to read {}", canonical.display()))?;
                    if let Some(reason) = hotbar_downgrade_refusal(&source, target_version) {
                        return Ok(SharedFileAction::Refused { reason });
                    }
                }

                copy_file(&canonical, &local, true)
            })()
            .unwrap_or_else(|error| SharedFileAction::Failed {
                reason: format!("{error:#}"),
            });

            SharedFileStatus { file, action }
        })
        .collect()
}

/// Dopo l'uscita del gioco: quello che l'istanza ha in mano torna nella copia
/// canonica.
///
/// **Qui vive il limite di D78, e va letto prima di "semplificare" questa
/// funzione.** Non c'è nessuna fusione: si riscrive. Due istanze della stessa
/// modlist avviate una dopo l'altra tengono in memoria due liste che divergono
/// dal momento in cui la seconda è partita, e **l'ultima che esce vince** — i
/// server aggiunti nell'altra spariscono senza dirlo. È una scelta, non una
/// dimenticanza: riconoscere le voci una per una vuol dire leggere l'NBT,
/// dare a ogni server un'identità stabile che il gioco non gli dà, e tenerla da
/// qualche parte. Quel pezzo è rimandato; finché è rimandato, questo commento è
/// l'unico posto in cui il limite è scritto accanto al codice che lo causa.
pub fn copy_back_from_instance(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
    instance_root: &Path,
) -> Vec<SharedFileStatus> {
    let canonical_owner = match canonical_modlist(connection, modlist_name) {
        Ok(owner) => owner,
        Err(error) => {
            return failed_for_all(&error.to_string());
        }
    };

    SharedFile::ALL
        .into_iter()
        .map(|file| {
            let action = (|| -> Result<SharedFileAction> {
                if !shared_file_enabled(connection, modlist_name, file) {
                    return Ok(SharedFileAction::Disabled);
                }

                let local = instance_path(instance_root, file);
                if !local.exists() {
                    return Ok(SharedFileAction::NothingToCopy);
                }

                let canonical = canonical_path(launcher_paths, &canonical_owner, file)?;

                if file == SharedFile::Hotbar && canonical.exists() {
                    // Un'unica direzione vietata, e la simmetria è ingannevole:
                    // qui la sorgente è l'istanza e la destinazione è la copia
                    // canonica, quindi il caso da fermare è **istanza più
                    // vecchia della canonica**. Rifiutare anche il verso
                    // opposto — un'istanza 1.21 che riscrive una canonica
                    // 1.20.1 — congelerebbe la canonica per sempre: le uniche
                    // scritture ammesse sarebbero quelle a `DataVersion`
                    // identica, e le modifiche dell'istanza più nuova
                    // sparirebbero in silenzio.
                    let source = std::fs::read(&local)
                        .with_context(|| format!("failed to read {}", local.display()))?;
                    let local_version = nbt_data_version(&source);
                    let canonical_version = std::fs::read(&canonical)
                        .ok()
                        .and_then(|bytes| nbt_data_version(&bytes));
                    if let (Some(local_version), Some(canonical_version)) =
                        (local_version, canonical_version)
                    {
                        if local_version < canonical_version {
                            return Ok(SharedFileAction::Refused {
                                reason: format!(
                                    "refused: this instance's hotbars are DataVersion \
                                     {local_version} and the shared copy is \
                                     {canonical_version}; writing back would downgrade what \
                                     the other instances see (D79)"
                                ),
                            });
                        }
                    }
                }

                copy_file(&local, &canonical, false)
            })()
            .unwrap_or_else(|error| SharedFileAction::Failed {
                reason: format!("{error:#}"),
            });

            SharedFileStatus { file, action }
        })
        .collect()
}

fn failed_for_all(reason: &str) -> Vec<SharedFileStatus> {
    SharedFile::ALL
        .into_iter()
        .map(|file| SharedFileStatus {
            file,
            action: SharedFileAction::Failed {
                reason: reason.to_string(),
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Quello che serve alla GUI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedFileToggle {
    pub file: SharedFile,
    /// La chiave stabile (`servers`, `hotbar`, `commandHistory`).
    pub key: String,
    /// Il nome del file nell'istanza, che è quello che l'utente riconosce.
    pub filename: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedFilesView {
    pub modlist: String,
    /// Le modlist che vedono gli stessi file, questa compresa. Vuoto quando non
    /// c'è nessun collegamento.
    pub group: Vec<String>,
    /// La modlist nella cui cartella stanno davvero i byte. Senza collegamenti
    /// è questa stessa.
    pub canonical: String,
    /// Le modlist a cui ci si può collegare: tutte le altre che esistono e che
    /// non sono già in questo gruppo.
    pub linkable: Vec<String>,
    pub files: Vec<SharedFileToggle>,
}

fn list_modlist_names(launcher_paths: &LauncherPaths) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let entries = match std::fs::read_dir(launcher_paths.modlists_dir()) {
        Ok(entries) => entries,
        Err(_) => return Ok(names),
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        // Una cartella è una modlist quando ha il suo `rules.json`: le altre
        // sono residui e offrirle nell'elenco dei collegamenti sarebbe un modo
        // di creare gruppi che puntano al nulla.
        if !entry.path().join("rules.json").is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }

    names.sort();
    Ok(names)
}

pub fn shared_files_view(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
) -> Result<SharedFilesView> {
    validate_path_component(modlist_name)?;

    let group = load_groups(connection)?
        .into_iter()
        .find(|group| group.contains(modlist_name))
        .map(|group| group.members)
        .unwrap_or_default();
    let canonical = canonical_modlist(connection, modlist_name)?;

    let linkable = list_modlist_names(launcher_paths)?
        .into_iter()
        .filter(|name| name != modlist_name && !group.iter().any(|member| member == name))
        .collect();

    let files = SharedFile::ALL
        .into_iter()
        .map(|file| SharedFileToggle {
            file,
            key: file.key().to_string(),
            filename: file.filename().to_string(),
            enabled: shared_file_enabled(connection, modlist_name, file),
        })
        .collect();

    Ok(SharedFilesView {
        modlist: modlist_name.to_string(),
        group,
        canonical,
        linkable,
        files,
    })
}

fn open_connection(launcher_paths: &LauncherPaths) -> Result<Connection> {
    Connection::open(launcher_paths.database_path()).context("failed to open the database")
}

fn file_from_key(key: &str) -> Result<SharedFile> {
    SharedFile::ALL
        .into_iter()
        .find(|file| file.key() == key)
        .ok_or_else(|| anyhow::anyhow!("'{key}' is not one of the shared files"))
}

#[tauri::command]
pub async fn shared_files_view_command(
    launcher_paths: tauri::State<'_, LauncherPaths>,
    modlist: String,
) -> Result<SharedFilesView, String> {
    let launcher_paths = launcher_paths.inner().clone();
    (|| -> Result<SharedFilesView> {
        let connection = open_connection(&launcher_paths)?;
        shared_files_view(&launcher_paths, &connection, &modlist)
    })()
    .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub async fn set_shared_file_enabled_command(
    launcher_paths: tauri::State<'_, LauncherPaths>,
    modlist: String,
    file: String,
    enabled: bool,
) -> Result<SharedFilesView, String> {
    let launcher_paths = launcher_paths.inner().clone();
    (|| -> Result<SharedFilesView> {
        let connection = open_connection(&launcher_paths)?;
        set_shared_file_enabled(&connection, &modlist, file_from_key(&file)?, enabled)?;
        shared_files_view(&launcher_paths, &connection, &modlist)
    })()
    .map_err(|error| format!("{error:#}"))
}

/// `modlist` entra nel gruppo di `other` e **adotta i suoi file**. Il verso non
/// è un dettaglio dell'implementazione: decide quale delle due liste server
/// sopravvive, ed è quello che la GUI deve dire prima di chiedere conferma.
#[tauri::command]
pub async fn link_shared_files_command(
    launcher_paths: tauri::State<'_, LauncherPaths>,
    modlist: String,
    other: String,
) -> Result<SharedFilesView, String> {
    let launcher_paths = launcher_paths.inner().clone();
    (|| -> Result<SharedFilesView> {
        let connection = open_connection(&launcher_paths)?;
        link_modlists(&connection, &modlist, &other)?;
        shared_files_view(&launcher_paths, &connection, &modlist)
    })()
    .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub async fn unlink_shared_files_command(
    launcher_paths: tauri::State<'_, LauncherPaths>,
    modlist: String,
) -> Result<SharedFilesView, String> {
    let launcher_paths = launcher_paths.inner().clone();
    (|| -> Result<SharedFilesView> {
        let connection = open_connection(&launcher_paths)?;
        unlink_modlist(&launcher_paths, &connection, &modlist)?;
        shared_files_view(&launcher_paths, &connection, &modlist)
    })()
    .map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
#[path = "shared_files_tests.rs"]
mod tests;
