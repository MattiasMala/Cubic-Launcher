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
pub fn is_excluded_from_export(file_name: &str) -> bool {
    file_name == COMMAND_HISTORY_FILENAME
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
// La copia dentro e la copia fuori
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SharedFileAction {
    Copied { bytes: u64 },
    /// La copia canonica non c'è ed è questa istanza a non avere niente da
    /// darle: non c'è niente da fare, e il file dell'istanza resta suo.
    NothingToCopy,
    /// Sorgente e destinazione hanno già gli stessi byte. Vale la pena dirlo
    /// invece di copiare: risparmia una riscrittura e, soprattutto, lascia
    /// l'`mtime` dov'è.
    AlreadyIdentical,
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
                SharedFileAction::NothingToCopy => "nothing to copy".to_string(),
                SharedFileAction::AlreadyIdentical => "already identical".to_string(),
                SharedFileAction::Failed { reason } => format!("failed: {reason}"),
            };
            format!("{} {}", status.file.filename(), action)
        })
        .collect();

    format!("{direction}: {}", parts.join("; "))
}

fn copy_file(source: &Path, target: &Path) -> Result<SharedFileAction> {
    let bytes = std::fs::read(source)
        .with_context(|| format!("failed to read {}", source.display()))?;

    if let Ok(existing) = std::fs::read(target) {
        if existing == bytes {
            return Ok(SharedFileAction::AlreadyIdentical);
        }
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(target, &bytes)
        .with_context(|| format!("failed to write {}", target.display()))?;

    Ok(SharedFileAction::Copied {
        bytes: bytes.len() as u64,
    })
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
pub fn copy_into_instance(
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
                let canonical = canonical_path(launcher_paths, &canonical_owner, file)?;
                let local = instance_path(instance_root, file);

                if !canonical.exists() {
                    if local.exists() {
                        return copy_file(&local, &canonical);
                    }
                    return Ok(SharedFileAction::NothingToCopy);
                }

                copy_file(&canonical, &local)
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
                let local = instance_path(instance_root, file);
                if !local.exists() {
                    return Ok(SharedFileAction::NothingToCopy);
                }

                let canonical = canonical_path(launcher_paths, &canonical_owner, file)?;
                copy_file(&local, &canonical)
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

#[cfg(test)]
#[path = "shared_files_tests.rs"]
mod tests;
