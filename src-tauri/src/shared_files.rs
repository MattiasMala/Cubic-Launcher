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

/// Prima di cancellare `modlist_name`: se è lei a tenere i byte del suo gruppo,
/// li passa a chi diventa il nuovo portatore. È la metà di [`unlink_modlist`]
/// che deve girare **prima** che la cartella sparisca; l'altra metà, togliere
/// il nome dai gruppi, è [`forget_modlist`], **dopo**. Il verso opposto di
/// `unlink_modlist` (dal portatore verso chi esce) qui non serve: chi esce va
/// nel cestino, e copiarci dentro ricreerebbe la cartella appena tolta
/// (`dati-5`).
pub fn hand_over_before_delete(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
) -> Result<()> {
    validate_path_component(modlist_name)?;

    let groups = load_groups(connection)?;
    let Some(group) = groups.iter().find(|group| group.contains(modlist_name)) else {
        return Ok(());
    };
    if group.canonical() != Some(modlist_name) {
        return Ok(());
    }
    if let Some(new_canonical) = group.members.iter().find(|name| *name != modlist_name) {
        copy_canonical_files(launcher_paths, modlist_name, new_canonical)?;
    }

    Ok(())
}

/// Dopo la cancellazione di `modlist_name`, o prima di far nascere una mod
/// list con lo stesso nome: il nome esce da ogni gruppo e perde i suoi
/// interruttori, senza copiare niente (`r2-ciclo-vita-nome-1`). Un gruppo che
/// resta con un solo membro sparisce, come in [`unlink_modlist`].
pub fn forget_modlist(connection: &Connection, modlist_name: &str) -> Result<()> {
    let groups = load_groups(connection)?;
    if groups.iter().any(|group| group.contains(modlist_name)) {
        let updated: Vec<SharedGroup> = groups
            .into_iter()
            .filter_map(|mut group| {
                group.members.retain(|name| name != modlist_name);
                (group.members.len() > 1).then_some(group)
            })
            .collect();
        write_groups(connection, &updated)?;
    }

    // Una riga degli interruttori illeggibile non si riscrive: perderebbe le
    // scelte delle altre mod list, e finché resta così non si condivide niente
    // comunque. La creazione e la cancellazione non si fermano per lei.
    let mut options = match load_file_options(connection) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("[shared] the switches of '{modlist_name}' were left in place: {error:#}");
            return Ok(());
        }
    };
    if options.remove(modlist_name).is_some() {
        let value =
            serde_json::to_string(&options).context("failed to serialize the file options")?;
        connection
            .execute(
                "INSERT INTO global_settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SHARED_FILE_OPTIONS_KEY, value.as_str()],
            )
            .context("failed to write the file options")?;
    }

    Ok(())
}

/// `dati-5`: la copia canonica si scrive solo nella cartella di una mod list
/// che esiste davvero. Senza `rules.json` — la cartella tolta a mano, o un
/// gruppo rimasto da una build che non puliva alla cancellazione — scriverci
/// dentro farebbe rinascere la mod list come un fantasma nella sidebar.
fn missing_canonical_owner(launcher_paths: &LauncherPaths, owner: &str) -> Option<String> {
    let rules = launcher_paths
        .modlists_dir()
        .join(owner)
        .join(crate::rules::RULES_FILENAME);
    (!rules.is_file()).then(|| {
        format!(
            "the mod list '{owner}' that holds the shared copy no longer exists ({} is missing)",
            rules.display()
        )
    })
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
///
/// **Una riga illeggibile non vuol dire "tutto acceso".** Prima diventava una
/// mappa vuota, cioè il default: riaccendeva in silenzio `command_history.txt`
/// a chi l'aveva spento (D77), e il primo interruttore toccato riscriveva la
/// riga cancellando le scelte di tutte le altre modlist. Adesso l'errore
/// risale: chi copia salta il file dicendo perché, e chi salva rifiuta.
pub fn shared_file_enabled(
    connection: &Connection,
    modlist_name: &str,
    file: SharedFile,
) -> Result<bool> {
    Ok(enabled_in(
        &load_file_options(connection)?,
        modlist_name,
        file,
    ))
}

type FileOptions = BTreeMap<String, BTreeMap<String, bool>>;

fn enabled_in(options: &FileOptions, modlist_name: &str, file: SharedFile) -> bool {
    options
        .get(modlist_name)
        .and_then(|per_file| per_file.get(file.key()).copied())
        .unwrap_or(true)
}

fn load_file_options(connection: &Connection) -> Result<FileOptions> {
    let stored: Option<String> = match connection.query_row(
        "SELECT value FROM global_settings WHERE key = ?1",
        [SHARED_FILE_OPTIONS_KEY],
        |row| row.get(0),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => {
            return Err(error).context("failed to read the shared file switches");
        }
    };

    let Some(stored) = stored else {
        return Ok(FileOptions::new());
    };
    serde_json::from_str(&stored).with_context(|| {
        format!(
            "the shared file switches stored under '{SHARED_FILE_OPTIONS_KEY}' are unreadable, \
             so nothing is shared until that row is fixed"
        )
    })
}

pub fn set_shared_file_enabled(
    connection: &Connection,
    modlist_name: &str,
    file: SharedFile,
    enabled: bool,
) -> Result<()> {
    validate_path_component(modlist_name)?;

    // Una riga illeggibile non si sovrascrive: dentro ci sono anche le scelte
    // delle altre modlist, e ripartire dalla mappa vuota le cancellerebbe.
    let mut options = load_file_options(connection)?;
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
// Gli esiti, e i byte su disco
// ---------------------------------------------------------------------------

/// Il suffisso della copia di sicurezza di D80.
pub const BACKUP_SUFFIX: &str = ".bak";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SharedFileAction {
    Copied {
        bytes: u64,
    },
    /// Come `Copied`, ma il file che stava lì aveva byte diversi e non era mai
    /// stato condiviso: prima di sovrascriverlo se n'è tenuta una copia (D80).
    CopiedAfterBackup {
        bytes: u64,
        backup: String,
    },
    /// Prima di copiare dentro: il file dell'istanza era cambiato dopo l'ultima
    /// copia scritta dal launcher — la copia indietro di quel lancio non è
    /// avvenuta, perché il launcher è stato chiuso o aggiornato col gioco
    /// aperto — e quindi è tornato alla copia canonica invece di essere
    /// sovrascritto (D152). `backup` è la copia canonica di prima, quando
    /// c'era ed era diversa.
    BroughtBack {
        bytes: u64,
        into: String,
        backup: Option<String>,
    },
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
    Refused {
        reason: String,
    },
    /// Copiare non è riuscito. Non ferma il lancio: un file condiviso che non
    /// arriva è una lista vuota, un lancio che non parte è un pomeriggio perso.
    Failed {
        reason: String,
    },
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
                SharedFileAction::BroughtBack {
                    bytes,
                    into,
                    backup,
                } => {
                    let kept = backup
                        .as_ref()
                        .map(|backup| format!(", kept the previous shared copy as {backup}"))
                        .unwrap_or_default();
                    format!(
                        "brought back {bytes}B into {into} first: the instance changed it after \
                         the last copy and that copy back never happened (D152){kept}"
                    )
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

/// I byte di `path`, o `None` se il file non c'è. Ogni altro errore risale:
/// "non sono riuscito a leggerlo" e "non esiste" portano a decisioni opposte —
/// il secondo autorizza a scriverci sopra, il primo no.
fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Scrittura con rename (todo.md:1531): un lancio interrotto, un disco pieno o
/// una corrente che salta lasciano il file di prima, non mezzo file.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    crate::atomic_write::write_atomically(path, bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Se il `.bak` che c'è già è ancora la fotografia di qualcosa.
///
/// Fino alle scritture con rename un `.bak` poteva restare **troncato** da
/// un'interruzione, e la regola "una volta sola" di D80 lo teneva così per
/// sempre: l'unica copia della lista di prima diventava un file rotto. Un `.bak`
/// vuoto, o un NBT (`servers.dat`, `hotbar.nbt`) che comincia come un NBT — il
/// tag di un compound, `0x0A` — e poi non si legge fino in fondo, non è una
/// fotografia da proteggere: si riscrive.
fn backup_is_usable(backup: &Path) -> bool {
    let Ok(bytes) = std::fs::read(backup) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }
    let is_nbt = backup
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(BACKUP_SUFFIX))
        .is_some_and(|stem| stem == SERVERS_FILENAME || stem == HOTBAR_FILENAME);
    let cut_nbt = is_nbt
        && bytes.first() == Some(&0x0A)
        && fastnbt::from_bytes::<fastnbt::Value>(&bytes).is_err();
    !cut_nbt
}

/// `keep_backup` distingue i due versi: entrando in un'istanza si può stare per
/// sovrascrivere un file che l'utente ha scritto e che nessuno ha mai
/// condiviso (D80); tornando indietro si sovrascrive una copia canonica, che
/// per definizione è già condivisa.
fn copy_bytes(bytes: &[u8], target: &Path, keep_backup: bool) -> Result<SharedFileAction> {
    // Un file che c'è ma non si legge non si sovrascrive: non si saprebbe né se
    // è uguale né cosa mettere nel `.bak`.
    let existing = read_if_exists(target)?;
    if existing.as_deref() == Some(bytes) {
        return Ok(SharedFileAction::AlreadyIdentical);
    }

    // D80. La copia di sicurezza nasce **una volta sola**: al secondo lancio il
    // file che si sta sovrascrivendo è quello che abbiamo scritto noi, e
    // salvarlo di nuovo cancellerebbe l'unica cosa che vale la pena tenere,
    // cioè la lista che l'utente aveva prima che la condivisione esistesse.
    // L'eccezione è un `.bak` rotto, che non protegge niente.
    let mut backup_name = None;
    if keep_backup {
        if let Some(existing) = existing.as_deref() {
            let backup = backup_path(target);
            if !backup.exists() || !backup_is_usable(&backup) {
                write_file(&backup, existing)?;
                backup_name = backup
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
            }
        }
    }

    write_file(target, bytes)?;

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

// ---------------------------------------------------------------------------
// L'ultima copia scritta dal launcher (D152, condivisi-3)
// ---------------------------------------------------------------------------

/// Il file in cui un'istanza ricorda l'ultima sincronizzazione, nella sua
/// cartella `.cubic/` (D39): nascosto, fuori dalle cartelle del gioco.
pub const SYNC_RECORD_FILENAME: &str = "shared-files.json";

/// Quello che il launcher sa dell'ultima volta che istanza e copia canonica
/// avevano gli stessi byte.
///
/// Esiste per un buco preciso (`concorrenza-1`): la copia indietro vive dopo
/// `process.wait()`, quindi se il launcher viene chiuso o aggiornato col gioco
/// aperto non avviene, e la copia dentro del lancio dopo scriveva sopra quello
/// che il gioco aveva salvato. Con l'impronta dell'ultima copia si riconosce un
/// file dell'istanza che è cambiato dopo, e lo si riporta indietro invece di
/// perderlo (D152).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncRecord {
    /// La modlist che teneva i file quando il gioco è partito. La copia
    /// indietro va lì e non al proprietario di adesso: collegare la modlist a
    /// un'altra mentre il gioco è aperto non deve invertire il verso
    /// (`condivisi-3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    /// Per file (la chiave stabile), lo sha256 dei byte che istanza e copia
    /// canonica avevano in comune all'ultima copia, in un verso o nell'altro.
    #[serde(default)]
    synced: BTreeMap<String, String>,
}

fn sync_record_path(instance_root: &Path) -> PathBuf {
    instance_root.join(".cubic").join(SYNC_RECORD_FILENAME)
}

/// Un registro assente è il caso di ogni istanza lanciata prima di D152, e un
/// registro illeggibile vale come assente: per quei file decide la regola di
/// ripiego di [`changed_since_last_copy`], che non perde niente.
fn load_sync_record(instance_root: &Path) -> Option<SyncRecord> {
    let bytes = read_if_exists(&sync_record_path(instance_root))
        .ok()
        .flatten()?;
    serde_json::from_slice(&bytes).ok()
}

/// Scritto con rename, come i file che descrive.
fn save_sync_record(instance_root: &Path, record: &SyncRecord) -> Result<()> {
    let json = serde_json::to_vec_pretty(record).context("failed to serialize the sync record")?;
    write_file(&sync_record_path(instance_root), &json)
}

fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Il proprietario registrato al lancio, se nomina ancora una modlist che c'è.
/// Un registro arrivato con un archivio importato sotto un altro nome, o una
/// modlist cancellata, non deve far nascere una cartella nuova.
fn recorded_owner(launcher_paths: &LauncherPaths, record: Option<&SyncRecord>) -> Option<String> {
    let owner = record?.owner.clone()?;
    validate_path_component(&owner).ok()?;
    // Una mod list esiste quando ha il suo rules.json (dati-5), non quando ne
    // resta solo la cartella.
    missing_canonical_owner(launcher_paths, &owner)
        .is_none()
        .then_some(owner)
}

/// Il file dell'istanza è cambiato dopo l'ultima copia scritta dal launcher?
///
/// Con il registro la risposta è un confronto di impronte. Senza — le istanze
/// lanciate prima di D152 — si guarda il `.bak` di D80 e la data: un `.bak`
/// accanto al file dice che il launcher ci ha già scritto sopra, quindi il file
/// è nella condivisione da prima; se è anche più recente della copia canonica,
/// l'ha scritto il gioco dopo l'ultima copia. Senza `.bak` vale D80 com'era: il
/// file non è mai stato condiviso, la copia canonica vince e lui finisce nel
/// `.bak`, quindi non si perde comunque.
fn changed_since_last_copy(
    recorded: Option<&String>,
    local: &Path,
    local_bytes: &[u8],
    canonical: &Path,
) -> bool {
    if let Some(recorded) = recorded {
        return *recorded != fingerprint(local_bytes);
    }

    if !backup_path(local).exists() {
        return false;
    }
    let modified = |path: &Path| std::fs::metadata(path).and_then(|meta| meta.modified());
    match (modified(local), modified(canonical)) {
        (Ok(local), Ok(canonical)) => local > canonical,
        // Senza copia canonica non c'è niente da sovrascrivere: ci pensa
        // l'adozione della copia dentro.
        _ => false,
    }
}

/// La guardia di D79 nel verso istanza → copia canonica: si ferma solo
/// **un'istanza più vecchia della canonica**. Rifiutare anche il verso opposto —
/// un'istanza 1.21 che riscrive una canonica 1.20.1 — congelerebbe la canonica
/// per sempre: le uniche scritture ammesse sarebbero quelle a `DataVersion`
/// identica, e le modifiche dell'istanza più nuova sparirebbero in silenzio.
///
/// Una copia canonica che c'è ma non si legge è un errore, non un via libera:
/// prima diventava "nessuna versione" e la guardia si spegneva proprio quando
/// non poteva sapere.
fn hotbar_write_back_refusal(local_bytes: &[u8], canonical: &Path) -> Result<Option<String>> {
    let Some(canonical_bytes) = read_if_exists(canonical)? else {
        return Ok(None);
    };
    let local_version = nbt_data_version(local_bytes);
    let canonical_version = nbt_data_version(&canonical_bytes);
    if let (Some(local_version), Some(canonical_version)) = (local_version, canonical_version) {
        if local_version < canonical_version {
            return Ok(Some(format!(
                "refused: this instance's hotbars are DataVersion {local_version} and the \
                 shared copy is {canonical_version}; writing back would downgrade what the \
                 other instances see (D79)"
            )));
        }
    }
    Ok(None)
}

/// La copia indietro che non è avvenuta, fatta adesso, prima della copia
/// dentro (D152). Stessa guardia delle hotbar della copia indietro vera; in
/// più, la copia canonica che si sovrascrive va nel suo `.bak`, **ogni
/// volta**: qui si sta scegliendo fra due versioni che sono cambiate tutte e
/// due, e quella che perde deve restare su disco.
fn bring_back(file: SharedFile, local_bytes: &[u8], canonical: &Path) -> Result<SharedFileAction> {
    if file == SharedFile::Hotbar {
        if let Some(reason) = hotbar_write_back_refusal(local_bytes, canonical)? {
            return Ok(SharedFileAction::Refused { reason });
        }
    }

    let existing = read_if_exists(canonical)?;
    if existing.as_deref() == Some(local_bytes) {
        return Ok(SharedFileAction::AlreadyIdentical);
    }

    let mut backup_name = None;
    if let Some(existing) = existing.as_deref() {
        let backup = backup_path(canonical);
        write_file(&backup, existing)?;
        backup_name = backup
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
    }
    write_file(canonical, local_bytes)?;

    Ok(SharedFileAction::BroughtBack {
        bytes: local_bytes.len() as u64,
        into: canonical.display().to_string(),
        backup: backup_name,
    })
}

// ---------------------------------------------------------------------------
// Il gioco aperto (condivisi-3)
// ---------------------------------------------------------------------------

/// La modlist il cui gioco sta girando, segnata dalla copia dentro e tolta
/// dalla copia indietro. Vale solo insieme al PID del gioco: se lo spawn
/// fallisce dopo la copia dentro, il PID non c'è e la riga rimasta non ferma
/// nessuno.
static MODLIST_IN_GAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn mark_in_game(modlist_name: Option<&str>) {
    if let Ok(mut slot) = MODLIST_IN_GAME.lock() {
        *slot = modlist_name.map(str::to_string);
    }
}

fn running_modlist() -> Option<String> {
    crate::launch_preview::current_minecraft_pid()?;
    MODLIST_IN_GAME.lock().ok()?.clone()
}

/// Collegare o scollegare mentre il gioco di una delle modlist coinvolte è
/// aperto cambia il proprietario della copia canonica sotto i piedi del gioco.
/// La copia indietro va comunque al proprietario registrato al lancio, ma il
/// gesto resta ambiguo: chi l'ha confermato si aspetta che valga da subito.
fn link_refusal(
    groups: &[SharedGroup],
    involved: &[&str],
    running: Option<&str>,
) -> Option<String> {
    let running = running?;
    let touches_running = involved.iter().any(|name| {
        *name == running
            || groups
                .iter()
                .any(|group| group.contains(name) && group.contains(running))
    });
    touches_running.then(|| {
        format!("{running} is running: close the game before linking or unlinking its shared files")
    })
}

// ---------------------------------------------------------------------------
// La copia dentro e la copia fuori
// ---------------------------------------------------------------------------

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
/// **E prima di tutto, la copia indietro mancata (D152).** Se il file
/// dell'istanza è cambiato dopo l'ultima copia scritta dal launcher, il lancio
/// precedente è finito senza copia indietro: il file torna alla copia canonica
/// del proprietario di allora, e solo dopo si copia dentro. Se tornare non si
/// può (la guardia delle hotbar, un errore) l'istanza tiene il suo file e non
/// si copia niente.
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
    if let Some(reason) = missing_canonical_owner(launcher_paths, &canonical_owner) {
        return failed_for_all(&reason);
    }
    let options = match load_file_options(connection) {
        Ok(options) => options,
        Err(error) => return failed_for_all(&format!("{error:#}")),
    };

    let previous = load_sync_record(instance_root);
    // Dove sarebbe andata la copia indietro mancata: al proprietario del
    // lancio di allora, che può non essere quello di adesso.
    let recover_owner = recorded_owner(launcher_paths, previous.as_ref())
        .unwrap_or_else(|| canonical_owner.clone());
    let mut record = SyncRecord {
        owner: Some(canonical_owner.clone()),
        synced: previous.map(|record| record.synced).unwrap_or_default(),
    };

    let mut statuses = Vec::new();
    for file in SharedFile::ALL {
        if !enabled_in(&options, modlist_name, file) {
            statuses.push(SharedFileStatus {
                file,
                action: SharedFileAction::Disabled,
            });
            continue;
        }

        let actions = copy_one_into(
            launcher_paths,
            file,
            &canonical_owner,
            &recover_owner,
            instance_root,
            instance_data_version,
            &mut record,
        )
        .unwrap_or_else(|error| {
            vec![SharedFileAction::Failed {
                reason: format!("{error:#}"),
            }]
        });
        statuses.extend(
            actions
                .into_iter()
                .map(|action| SharedFileStatus { file, action }),
        );
    }

    if save_sync_record(instance_root, &record).is_err() {
        // Un registro vecchio rimasto lì sarebbe peggio di nessun registro: la
        // copia indietro andrebbe al proprietario di un lancio precedente.
        // Senza registro si torna al comportamento di prima — il proprietario
        // calcolato all'uscita — e al lancio dopo la regola di ripiego di
        // `changed_since_last_copy`, che non perde niente.
        let _ = std::fs::remove_file(sync_record_path(instance_root));
    }

    mark_in_game(Some(modlist_name));
    statuses
}

/// Un file solo, nei due passi di [`copy_into_instance`]. Torna una o due
/// azioni: la copia indietro mancata, quando c'è stata, e la copia dentro
/// quando ha fatto qualcosa di diverso dal trovare i byte già uguali.
fn copy_one_into(
    launcher_paths: &LauncherPaths,
    file: SharedFile,
    canonical_owner: &str,
    recover_owner: &str,
    instance_root: &Path,
    instance_data_version: Option<i64>,
    record: &mut SyncRecord,
) -> Result<Vec<SharedFileAction>> {
    let canonical = canonical_path(launcher_paths, canonical_owner, file)?;
    let local = instance_path(instance_root, file);
    let local_bytes = read_if_exists(&local)?;
    let mut actions = Vec::new();

    // 1. La copia indietro che il lancio precedente non ha fatto (D152).
    if let Some(local_bytes) = local_bytes.as_deref() {
        let recover_to = canonical_path(launcher_paths, recover_owner, file)?;
        if changed_since_last_copy(
            record.synced.get(file.key()),
            &local,
            local_bytes,
            &recover_to,
        ) {
            match bring_back(file, local_bytes, &recover_to)? {
                SharedFileAction::AlreadyIdentical => {}
                action @ SharedFileAction::BroughtBack { .. } => actions.push(action),
                // Rifiutata: l'istanza tiene il suo file, e copiarci sopra
                // adesso sarebbe esattamente la perdita che questo passo evita.
                other => return Ok(vec![other]),
            }
            record
                .synced
                .insert(file.key().to_string(), fingerprint(local_bytes));
        }
    }

    // 2. La copia dentro.
    let Some(canonical_bytes) = read_if_exists(&canonical)? else {
        let Some(local_bytes) = local_bytes else {
            actions.push(SharedFileAction::NothingToCopy);
            return Ok(actions);
        };
        // Adottare non sovrascrive niente: la copia canonica non c'è, quindi
        // non c'è nessun file dell'utente da salvare.
        let action = copy_bytes(&local_bytes, &canonical, false)?;
        record
            .synced
            .insert(file.key().to_string(), fingerprint(&local_bytes));
        actions.push(action);
        return Ok(actions);
    };

    if file == SharedFile::Hotbar {
        // La destinazione è l'istanza: la sua `DataVersion` è quella del suo
        // `hotbar.nbt` se ce l'ha, altrimenti quella della versione che sta per
        // partire.
        let target_version = local_bytes
            .as_deref()
            .and_then(nbt_data_version)
            .or(instance_data_version);
        if let Some(reason) = hotbar_downgrade_refusal(&canonical_bytes, target_version) {
            actions.push(SharedFileAction::Refused { reason });
            return Ok(actions);
        }
    }

    let action = copy_bytes(&canonical_bytes, &local, true)?;
    record
        .synced
        .insert(file.key().to_string(), fingerprint(&canonical_bytes));
    if action != SharedFileAction::AlreadyIdentical || actions.is_empty() {
        actions.push(action);
    }
    Ok(actions)
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
///
/// La copia canonica è quella **del lancio**, registrata dalla copia dentro, e
/// non quella di adesso: se nel frattempo la modlist è stata collegata a
/// un'altra, la lista del gioco non deve finire sopra quella che l'utente ha
/// appena scelto di adottare (`condivisi-3`).
pub fn copy_back_from_instance(
    launcher_paths: &LauncherPaths,
    connection: &Connection,
    modlist_name: &str,
    instance_root: &Path,
) -> Vec<SharedFileStatus> {
    mark_in_game(None);

    let previous = load_sync_record(instance_root);
    let canonical_owner = match recorded_owner(launcher_paths, previous.as_ref()) {
        Some(owner) => owner,
        None => match canonical_modlist(connection, modlist_name) {
            Ok(owner) => owner,
            Err(error) => {
                return failed_for_all(&error.to_string());
            }
        },
    };
    // dati-5: una portatrice sparita non si ricrea scrivendoci la copia.
    if let Some(reason) = missing_canonical_owner(launcher_paths, &canonical_owner) {
        return failed_for_all(&reason);
    }
    let options = match load_file_options(connection) {
        Ok(options) => options,
        Err(error) => return failed_for_all(&format!("{error:#}")),
    };
    let mut record = previous.unwrap_or_default();

    let statuses: Vec<SharedFileStatus> = SharedFile::ALL
        .into_iter()
        .map(|file| {
            let action = (|| -> Result<SharedFileAction> {
                if !enabled_in(&options, modlist_name, file) {
                    return Ok(SharedFileAction::Disabled);
                }

                let local = instance_path(instance_root, file);
                let Some(source) = read_if_exists(&local)? else {
                    return Ok(SharedFileAction::NothingToCopy);
                };

                let canonical = canonical_path(launcher_paths, &canonical_owner, file)?;

                if file == SharedFile::Hotbar {
                    // Un'unica direzione vietata: vedi `hotbar_write_back_refusal`.
                    if let Some(reason) = hotbar_write_back_refusal(&source, &canonical)? {
                        return Ok(SharedFileAction::Refused { reason });
                    }
                }

                let action = copy_bytes(&source, &canonical, false)?;
                record
                    .synced
                    .insert(file.key().to_string(), fingerprint(&source));
                Ok(action)
            })()
            .unwrap_or_else(|error| SharedFileAction::Failed {
                reason: format!("{error:#}"),
            });

            SharedFileStatus { file, action }
        })
        .collect();

    // Se non si scrive, il lancio dopo trova i file "cambiati" e li riporta
    // indietro identici: nessun danno, e niente da aggiungere al log.
    let _ = save_sync_record(instance_root, &record);

    statuses
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
    /// Il gioco di una modlist di questo gruppo è aperto: collegare o
    /// scollegare adesso cambierebbe dove va la copia indietro mentre il gioco
    /// ha ancora i file in mano (condivisi-3), quindi la GUI spegne i bottoni.
    pub game_running: bool,
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
        .map(|file| -> Result<SharedFileToggle> {
            Ok(SharedFileToggle {
                file,
                key: file.key().to_string(),
                filename: file.filename().to_string(),
                enabled: shared_file_enabled(connection, modlist_name, file)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let game_running = running_modlist()
        .is_some_and(|running| running == modlist_name || group.contains(&running));

    Ok(SharedFilesView {
        modlist: modlist_name.to_string(),
        group,
        canonical,
        linkable,
        files,
        game_running,
    })
}

fn open_connection(launcher_paths: &LauncherPaths) -> Result<Connection> {
    Connection::open(launcher_paths.database_path()).context("failed to open the database")
}

fn refuse_while_in_game(connection: &Connection, involved: &[&str]) -> Result<()> {
    let groups = load_groups(connection)?;
    match link_refusal(&groups, involved, running_modlist().as_deref()) {
        Some(reason) => bail!(reason),
        None => Ok(()),
    }
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
        refuse_while_in_game(&connection, &[&modlist, &other])?;
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
        refuse_while_in_game(&connection, &[&modlist])?;
        unlink_modlist(&launcher_paths, &connection, &modlist)?;
        shared_files_view(&launcher_paths, &connection, &modlist)
    })()
    .map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
#[path = "shared_files_tests.rs"]
mod tests;
